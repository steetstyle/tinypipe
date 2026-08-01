// tinypipe Go worker örneği — `tinypipe-daemon`'a bağlanan minimal worker.
//
// Kullanım:
//   go run .                                  # TINYPIPE_DAEMON_ADDR (default 127.0.0.1:50051)
//
// Graceful shutdown: SIGINT/SIGTERM → CloseSend → daemon worker'ı havuzdan
// çıkarır, bekleyen task'lerine fail-fast verir.
//
// E2E doğrulama:
//   1. tinypipe-daemon çalıştır (ayrı terminal)
//   2. go run .                                → worker kaydolur (2 tool)
//   3. tinypipe-cli tools list                 → send_email, text.reverse görünür
//   4. tinypipe-cli tools test text.reverse '["hello"]'
package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"tinypipe-worker/worker"
)

func main() {
	addr := os.Getenv("TINYPIPE_DAEMON_ADDR")
	if addr == "" {
		addr = "127.0.0.1:50051"
	}

	w := worker.New(addr)
	if err := w.RegisterTool(worker.Tool{
		Name:        "send_email",
		Description: "Sends an email (stub).",
		TimeoutMs:   5000,
	}, sendEmail); err != nil {
		log.Fatalf("register: %v", err)
	}
	if err := w.RegisterTool(worker.Tool{
		Name:        "text.reverse",
		Description: "Reverses a string.",
	}, reverseText); err != nil {
		log.Fatalf("register: %v", err)
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	log.Printf("go-worker: starting (daemon=%s)", addr)
	if err := w.Start(ctx); err != nil {
		log.Fatalf("worker: %v", err)
	}
	log.Printf("go-worker: stopped")
}

// sendEmail — stub: 300ms bekler, teslim raporu döner.
func sendEmail(_ []any, kwargs map[string]any) (any, error) {
	to, _ := kwargs["to"].(string)
	if to == "" {
		return nil, os.ErrInvalid
	}
	time.Sleep(300 * time.Millisecond)
	return map[string]any{
		"status": "sent",
		"to":     to,
		"at":     time.Now().UTC().Format(time.RFC3339),
	}, nil
}

// reverseText — input'u tersine çevirir.
// Konvansiyon: tinypipe dili `call("text.reverse", value=s)` ile kwargs
// gönderir; CLI `tools test` ise args dizisi gönderir — ikisini de kabul et.
func reverseText(args []any, kwargs map[string]any) (any, error) {
	var s string
	switch {
	case len(args) > 0:
		v, ok := args[0].(string)
		if !ok {
			return nil, os.ErrInvalid
		}
		s = v
	default:
		v, ok := kwargs["value"].(string)
		if !ok {
			return nil, os.ErrInvalid
		}
		s = v
	}
	runes := []rune(s)
	for i, j := 0, len(runes)-1; i < j; i, j = i+1, j-1 {
		runes[i], runes[j] = runes[j], runes[i]
	}
	return string(runes), nil
}
