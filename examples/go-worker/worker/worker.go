// Package worker — tinypipe daemon için minimal Go worker SDK.
//
// Worker, daemon'a OUTBOUND bağlanır (pull modeli): tool'larını tek bir
// bidi stream üzerinden kaydeder ve iş bekler. Daemon, CLI'dan gelen
// Invoke'ları task_id ile eşleştirip bu stream'e yönlendirir; cevaplar
// herhangi bir sırada dönebilir.
//
// Önemli tasarım noktaları:
//   - grpc-go SendMsg goroutine-safe değildir → tüm cevaplar tek mutex ile
//     serialize edilir. Handler'lar paralel çalışabilir.
//   - Handler panic'leri recover edilir ve hata cevabına dönüştürülür.
//   - Daemon kapalıysa exponansiyel backoff ile yeniden bağlanılır.
//   - Graceful shutdown: Start'a verilen ctx iptal edilince CloseSend +
//     bağlantı kapatılır; daemon worker'ı havuzdan idempotent çıkarır.
package worker

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	tinypipev1 "tinypipe-worker/gen"
)

// ToolFunc — bir tool'un uygulaması. Dönen değer JSON'a çevrilir;
// hata dönerse cevap success=false ile işaretlenir.
type ToolFunc func(args []any, kwargs map[string]any) (any, error)

// Tool — daemon'a kaydedilen tool tanımı.
type Tool struct {
	Name        string
	Description string
	InputSchema string // JSON Schema metni (opsiyonel)
	OutputSchema string
	TimeoutMs   uint64 // 0 = daemon varsayılanı (30s)
}

// toProto — TaskResponse kayıt mesajı için gRPC tanımına çevirir.
func (t *Tool) toProto() *tinypipev1.ToolDefinition {
	return &tinypipev1.ToolDefinition{
		Name:             t.Name,
		Description:      t.Description,
		InputSchemaJson:  t.InputSchema,
		OutputSchemaJson: t.OutputSchema,
		TimeoutMs:        t.TimeoutMs,
	}
}

// Worker — daemon bağlantısını yönetir.
type Worker struct {
	addr    string
	apiKey  string
	mu      sync.Mutex // grpc-go SendMsg goroutine-safe değil
	stream  grpc.BidiStreamingClient[tinypipev1.TaskResponse, tinypipev1.TaskRequest]
	conn    *grpc.ClientConn
	handler map[string]ToolFunc
	tools   []*Tool

	backoffBase time.Duration
	backoffMax  time.Duration
}

// New — yeni worker kurar.
func New(addr string) *Worker {
	return &Worker{
		addr:        addr,
		handler:     make(map[string]ToolFunc),
		backoffBase: 2 * time.Second,
		backoffMax:  30 * time.Second,
	}
}

// SetAPIKey — daemon kayıt anahtarını ayarlar (TINYPIPE_DAEMON_API_KEY).
// Daemon auth açıkken eksik/yanlış anahtar kaydı reddedilir.
func (w *Worker) SetAPIKey(key string) {
	w.apiKey = key
}

// RegisterTool — tool kaydeder. Aynı adla tekrar kayıt hata döndürür.
func (w *Worker) RegisterTool(t Tool, fn ToolFunc) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if _, exists := w.handler[t.Name]; exists {
		return fmt.Errorf("tool %q already registered", t.Name)
	}
	w.handler[t.Name] = fn
	w.tools = append(w.tools, &t)
	return nil
}

// Start — ctx iptal edilene kadar bağlanır, kaydolur ve işleri çalıştırır.
// Daemon kapalıysa backoff ile yeniden dener.
func (w *Worker) Start(ctx context.Context) error {
	backoff := w.backoffBase
	for {
		err := w.runOnce(ctx)
		if ctx.Err() != nil {
			return nil
		}
		log.Printf("worker: connection lost (%v) — retrying in %s", err, backoff)
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(backoff):
		}
		if backoff < w.backoffMax {
			backoff = min(backoff*2, w.backoffMax)
		}
	}
}

// runOnce — tek bağlantı ömrü: bağlan, kaydol, task döngüsü.
func (w *Worker) runOnce(ctx context.Context) error {
	w.close()

	conn, err := grpc.NewClient(w.addr,
		grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return fmt.Errorf("dial %s: %w", w.addr, err)
	}
	w.conn = conn

	client := tinypipev1.NewToolWorkerServiceClient(conn)
	stream, err := client.ConnectWorker(ctx)
	if err != nil {
		_ = conn.Close()
		return fmt.Errorf("connect worker: %w", err)
	}
	w.mu.Lock()
	w.stream = stream
	w.mu.Unlock()
	log.Printf("worker: connected to daemon at %s (%d tools)", w.addr, len(w.tools))

	// Kayıt mesajı: ilk TaskResponse'ta registered_tools (+ api_key) gider.
	protos := make([]*tinypipev1.ToolDefinition, 0, len(w.tools))
	for _, t := range w.tools {
		protos = append(protos, t.toProto())
	}
	registration := &tinypipev1.TaskResponse{RegisteredTools: protos, ApiKey: w.apiKey}
	w.sendLocked(registration)

	for {
		task, err := stream.Recv()
		if err != nil {
			return fmt.Errorf("recv task: %w", err)
		}
		go w.handleTask(task) // paralel işleme — cevaplar farklı sırada dönebilir
	}
}

// handleTask — tek task'ı çalıştırır ve cevabı gönderir.
func (w *Worker) handleTask(task *tinypipev1.TaskRequest) {
	start := time.Now()
	resp := &tinypipev1.TaskResponse{TaskId: task.TaskId}

	fn, ok := w.handler[task.ToolName]
	if !ok {
		resp.ErrorMessage = fmt.Sprintf("tool %q not registered on this worker", task.ToolName)
		w.sendLocked(resp)
		return
	}

	args, kwargs, err := decodeArgs(task)
	if err != nil {
		resp.ErrorMessage = err.Error()
		w.sendLocked(resp)
		return
	}

	out, err := runSafely(fn, args, kwargs)
	if err != nil {
		resp.ErrorMessage = err.Error()
	} else {
		resp.Success = true
		if resp.OutputJson, err = encodeOutput(out); err != nil {
			resp.Success = false
			resp.OutputJson = ""
			resp.ErrorMessage = fmt.Sprintf("output not JSON-serializable: %v", err)
		}
	}
	log.Printf("worker: %s %q done in %s (success=%v)", task.TaskId, task.ToolName, time.Since(start), resp.Success)
	w.sendLocked(resp)
}

// sendLocked — mutex ile serialize edilmiş send.
func (w *Worker) sendLocked(resp *tinypipev1.TaskResponse) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.stream == nil {
		return
	}
	if err := w.stream.Send(resp); err != nil {
		log.Printf("worker: send failed: %v", err)
	}
}

// runSafely — handler'ı panic korumalı çalıştırır.
func runSafely(fn ToolFunc, args []any, kwargs map[string]any) (out any, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v", r)
		}
	}()
	return fn(args, kwargs)
}

// decodeArgs — args_json/kwargs_json'ı Go değerlerine çevirir.
func decodeArgs(task *tinypipev1.TaskRequest) ([]any, map[string]any, error) {
	var args []any
	if err := json.Unmarshal([]byte(task.ArgsJson), &args); err != nil {
		return nil, nil, fmt.Errorf("invalid args_json: %v", err)
	}
	var kwargs map[string]any
	if err := json.Unmarshal([]byte(task.KwargsJson), &kwargs); err != nil {
		return nil, nil, fmt.Errorf("invalid kwargs_json: %v", err)
	}
	return args, kwargs, nil
}

// encodeOutput — handler dönüşünü JSON string'e çevirir.
func encodeOutput(out any) (string, error) {
	bytes, err := json.Marshal(out)
	return string(bytes), err
}

// close — aktif bağlantıyı kapatır (CloseSend + conn.Close).
func (w *Worker) close() {
	w.mu.Lock()
	stream, conn := w.stream, w.conn
	w.stream, w.conn = nil, nil
	w.mu.Unlock()
	if stream != nil {
		_ = stream.CloseSend()
	}
	if conn != nil {
		_ = conn.Close()
	}
}
