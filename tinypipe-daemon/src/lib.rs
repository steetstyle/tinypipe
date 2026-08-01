//! `tinypipe-daemon` — gRPC pull mimarisi ile tool worker'larını ağırlayan daemon.
//!
//! Akış:
//! ```text
//! tinypipe-cli ──unary Invoke/ListTools──▶ tinypipe-daemon ──bidi ConnectWorker──▶ worker
//! ```
//!
//! Tasarım noktaları:
//! - **Worker kaydı:** worker'ın ilk `TaskResponse`'unda `registered_tools` gelir;
//!   her tool, worker'ın task kanalına (mpsc) bağlanır. Aynı tool adına sahip
//!   birden fazla worker **round-robin** (AtomicUsize, kilit yarışması yok) dağıtılır.
//! - **Bekleme havuzu:** `task_id → oneshot::Sender<TaskResponse>` eşleşmesi
//!   (`DashMap`). Cevap eşleştirmesi task_id üzerinden yapılır; worker'lar
//!   task'leri paralel işleyip farklı sırada cevaplayabilir.
//! - **Zaman aşımı:** tool başına `ToolDefinition.timeout_ms` (0 = daemon varsayılanı,
//!   default 30s). Timeout patladığında pending girişi garantili kaldırılır.
//! - **Disconnect fail-fast:** bir worker koparsa (stream Drop/err), o worker'a
//!   atanmış bekleyen task'lere anında `worker disconnected` cevabı gönderilir —
//!   CLI tarafı 30 sn beklemez.
//! - **Keepalive:** HTTP/2 + TCP keepalive (yarım açık bağlantıları erken tespit).

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use futures::Stream;
use tinypipe_proto::tinypipe::v1::{
    tool_dispatch_service_server::{ToolDispatchService, ToolDispatchServiceServer},
    tool_worker_service_server::{ToolWorkerService, ToolWorkerServiceServer},
    InvokeRequest, InvokeResponse, ListToolsRequest, ListToolsResponse, TaskRequest, TaskResponse,
    ToolDefinition,
};
use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status, Streaming};

/// Varsayılan çağrı zaman aşımı (tool `timeout_ms=0` bildirirse kullanılır).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bekleyen iş kaydı: cevap kanalı.
struct PendingTask {
    tx: oneshot::Sender<TaskResponse>,
}

/// Aynı tool adına sahip worker grubu — round-robin dağıtımı.
///
/// `next_idx` AtomicUsize: dağıtım sırasında kilit tutulmaz; worker listesi
/// yalnızca kayıt/kaldırma anında kısa kilitlenir.
struct WorkerGroup {
    workers: Mutex<Vec<(String, mpsc::Sender<TaskRequest>)>>,
    next_idx: AtomicUsize,
}

impl WorkerGroup {
    fn new() -> Self {
        WorkerGroup {
            workers: Mutex::new(Vec::new()),
            next_idx: AtomicUsize::new(0),
        }
    }

    fn add(&self, worker_id: &str, tx: mpsc::Sender<TaskRequest>) {
        self.workers.lock().unwrap().push((worker_id.to_owned(), tx));
    }

    fn remove_worker(&self, worker_id: &str) {
        self.workers.lock().unwrap().retain(|(id, _)| id != worker_id);
    }

    fn worker_count(&self) -> usize {
        self.workers.lock().unwrap().len()
    }

    /// Sıradaki worker'ı seçer: `(worker_id, task kanalı)` döner.
    fn get_next_worker(&self) -> Option<(String, mpsc::Sender<TaskRequest>)> {
        let workers = self.workers.lock().unwrap();
        if workers.is_empty() {
            return None;
        }
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        Some(workers[idx % workers.len()].clone())
    }
}

/// Daemon durumu — servisler üzerinden paylaşılır.
pub struct Daemon {
    addr: String,
    default_timeout: Duration,
    tool_groups: DashMap<String, Arc<WorkerGroup>>,
    tool_defs: DashMap<String, ToolDefinition>,
    pending: DashMap<String, PendingTask>,
    in_flight: Mutex<HashMap<String, HashSet<String>>>,
}

impl Daemon {
    pub fn new(addr: impl Into<String>) -> Self {
        Daemon::with_default_timeout(addr, DEFAULT_TIMEOUT)
    }

    /// Özelleştirilmiş varsayılan zaman aşımıyla kurulum.
    pub fn with_default_timeout(addr: impl Into<String>, default_timeout: Duration) -> Self {
        Daemon {
            addr: addr.into(),
            default_timeout,
            tool_groups: DashMap::new(),
            tool_defs: DashMap::new(),
            pending: DashMap::new(),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Kayıtlı tool tanımları (ad sıralı) — status/test için.
    pub fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools: Vec<ToolDefinition> = self
            .tool_defs
            .iter()
            .map(|e| e.value().clone())
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Bir tool adına bağlı worker sayısı — status/test için.
    pub fn worker_count(&self, tool_name: &str) -> usize {
        self.tool_groups
            .get(tool_name)
            .map(|g| g.worker_count())
            .unwrap_or(0)
    }

    // ── Worker kaydı / temizlik ────────────────────────────────────

    /// Worker'ın bildirdiği tool tanımlarını kaydeder.
    fn register_tool(
        &self,
        def: &ToolDefinition,
        worker_id: &str,
        tx: mpsc::Sender<TaskRequest>,
    ) -> Result<(), Status> {
        if def.name.trim().is_empty() {
            return Err(Status::invalid_argument("tool name must not be empty"));
        }
        let group = self
            .tool_groups
            .entry(def.name.clone())
            .or_insert_with(|| Arc::new(WorkerGroup::new()))
            .clone();
        group.add(worker_id, tx);
        self.tool_defs.insert(def.name.clone(), def.clone());
        Ok(())
    }

    /// Worker'ı tüm gruplardan çıkarır ve bekleyen task'lerine fail-fast verir.
    /// İdempotent — birden fazla yoldan (inbound task sonu, outbound Drop) çağrılabilir.
    fn disconnect_worker(&self, worker_id: &str) {
        let mut emptied = Vec::new();
        for entry in self.tool_groups.iter() {
            entry.value().remove_worker(worker_id);
            if entry.value().worker_count() == 0 {
                emptied.push(entry.key().clone());
            }
        }
        // Son worker de ayrıldıysa tanım da silinir (list_tools gerçeği göstersin).
        for name in emptied {
            self.tool_groups.remove(&name);
            self.tool_defs.remove(&name);
        }
        let task_ids: Vec<String> = self
            .in_flight
            .lock()
            .unwrap()
            .remove(worker_id)
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_default();
        for task_id in task_ids {
            if let Some((_, pending)) = self.pending.remove(&task_id) {
                let _ = pending.tx.send(TaskResponse {
                    task_id,
                    success: false,
                    output_json: String::new(),
                    error_message: "worker disconnected".to_string(),
                    registered_tools: Vec::new(),
                });
            }
        }
    }

    // ── Dispatch ───────────────────────────────────────────────────

    async fn invoke_inner(&self, req: InvokeRequest) -> Result<InvokeResponse, Status> {
        let start = std::time::Instant::now();
        let tool_name = req.tool_name.clone();

        let def = self
            .tool_defs
            .get(&tool_name)
            .map(|d| d.value().clone())
            .ok_or_else(|| Status::not_found(format!("tool '{tool_name}' has no registered worker")))?;
        let group = self
            .tool_groups
            .get(&tool_name)
            .map(|g| g.value().clone())
            .ok_or_else(|| Status::not_found(format!("tool '{tool_name}' has no registered worker")))?;
        let (worker_id, tx) = group
            .get_next_worker()
            .ok_or_else(|| Status::unavailable(format!("no worker available for tool '{tool_name}'")))?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let timeout = if def.timeout_ms > 0 {
            Duration::from_millis(def.timeout_ms)
        } else {
            self.default_timeout
        };
        let (res_tx, res_rx) = oneshot::channel();

        // Sıralama önemli: pending + in_flight kayıtları try_send'den ÖNCE
        // yazılır — worker bu anda koparsa fail-fast bunları bulur ve temizler.
        self.pending
            .insert(task_id.clone(), PendingTask { tx: res_tx });
        self.in_flight
            .lock()
            .unwrap()
            .entry(worker_id.clone())
            .or_default()
            .insert(task_id.clone());

        let task = TaskRequest {
            task_id: task_id.clone(),
            tool_name,
            args_json: req.args_json,
            kwargs_json: req.kwargs_json,
            env: req.env,
        };
        if tx.try_send(task).is_err() {
            self.pending.remove(&task_id);
            self.in_flight.lock().unwrap().entry(worker_id.clone()).or_default().remove(&task_id);
            return Err(Status::unavailable("worker connection lost"));
        }

        match tokio::time::timeout(timeout, res_rx).await {
            Ok(Ok(resp)) => {
                self.in_flight.lock().unwrap().entry(worker_id.clone()).or_default().remove(&task_id);
                Ok(InvokeResponse {
                    success: resp.success,
                    output_json: resp.output_json,
                    error_message: resp.error_message,
                    duration_us: start.elapsed().as_micros() as u64,
                })
            }
            Ok(Err(_)) => {
                self.in_flight.lock().unwrap().entry(worker_id.clone()).or_default().remove(&task_id);
                Err(Status::unavailable("worker disconnected while processing"))
            }
            Err(_) => {
                self.pending.remove(&task_id);
                self.in_flight.lock().unwrap().entry(worker_id.clone()).or_default().remove(&task_id);
                Err(Status::deadline_exceeded(format!(
                    "tool '{}' timed out after {}ms",
                    def.name,
                    timeout.as_millis()
                )))
            }
        }
    }
}

// ─── gRPC servisleri ────────────────────────────────────────────────

/// Daemon'ı servis implementasyonlarında kullanmak için taşıyıcı.
/// `Arc<Daemon>`'ı spawn edilen görevlere klonlamak için tutar.
pub struct DaemonServer(pub Arc<Daemon>);

#[tonic::async_trait]
impl ToolWorkerService for DaemonServer {
    type ConnectWorkerStream =
        Pin<Box<dyn Stream<Item = Result<TaskRequest, Status>> + Send>>;

    async fn connect_worker(
        &self,
        request: Request<Streaming<TaskResponse>>,
    ) -> Result<Response<Self::ConnectWorkerStream>, Status> {
        let mut inbound = request.into_inner();

        // 1. İlk mesaj = kayıt (registered_tools)
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("worker sent no registration message"))?;

        let worker_id = uuid::Uuid::new_v4().to_string();
        let daemon_owned = self.0.clone();
        let (tasks_tx, tasks_rx) = mpsc::channel::<TaskRequest>(256);

        for def in &first.registered_tools {
            self.0.register_tool(def, &worker_id, tasks_tx.clone())?;
        }
        tracing::info!(
            worker_id,
            tools = first.registered_tools.len(),
            "worker registered"
        );

        // 2. Outbound: task kuyruğu → gRPC stream (client kapandığında Drop → temizlik)
        let outbound = futures::stream::unfold(tasks_rx, |mut rx| async move {
            rx.recv().await.map(|task| (Ok::<_, Status>(task), rx))
        });
        let cleanup_daemon = daemon_owned.clone();
        let cleanup_worker_id = worker_id.clone();
        let outbound: Self::ConnectWorkerStream = Box::pin(CleanupStream {
            inner: Box::pin(outbound),
            cleanup: Some(Box::new(move || {
                cleanup_daemon.disconnect_worker(&cleanup_worker_id)
            })),
        });

        // 3. Inbound: cevapları işle; stream bitince temizle
        let daemon_in = daemon_owned;
        let worker_id_in = worker_id;
        tokio::spawn(async move {
            let result: Result<(), Status> = async {
                loop {
                    let Some(msg) = inbound.message().await? else {
                        break;
                    };
                    let Some((_, pending)) = daemon_in.pending.remove(&msg.task_id) else {
                        tracing::debug!(task_id = %msg.task_id, "late or unsolicited response ignored");
                        continue;
                    };
                    let _ = pending.tx.send(msg);
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                tracing::warn!(worker_id = %worker_id_in, error = %e, "worker stream error");
            }
            daemon_in.disconnect_worker(&worker_id_in);
        });

        Ok(Response::new(outbound))
    }
}

#[tonic::async_trait]
impl ToolDispatchService for DaemonServer {
    async fn invoke(
        &self,
        request: Request<InvokeRequest>,
    ) -> Result<Response<InvokeResponse>, Status> {
        let resp = self.0.invoke_inner(request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    async fn list_tools(
        &self,
        _request: Request<ListToolsRequest>,
    ) -> Result<Response<ListToolsResponse>, Status> {
        Ok(Response::new(ListToolsResponse {
            tools: self.0.tools(),
        }))
    }
}

/// Outbound stream'i saran temizlik sarmalayıcısı: client tarafı stream'i
/// bırakırsa (kapandı/iptal) worker temizliği garantili çalışır.
pub struct CleanupStream<S> {
    inner: S,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl<S> Stream for CleanupStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for CleanupStream<S> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// Daemon'ı bir adrese bind edip background'da çalıştırır.
/// `addr = "127.0.0.1:0"` → gerçek port döner (testler için).
pub async fn bind_and_serve(
    daemon: Arc<Daemon>,
    shutdown: impl futures::Future<Output = ()> + Send + 'static,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
    let incoming = tonic::transport::server::TcpIncoming::bind(daemon.addr().parse()?)?;
    let local = incoming.local_addr()?;

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ToolWorkerServiceServer::new(DaemonServer(daemon.clone())))
            .add_service(ToolDispatchServiceServer::new(DaemonServer(daemon)))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .expect("daemon serve failed");
    });
    Ok(local)
}

#[cfg(test)]
mod tests;
