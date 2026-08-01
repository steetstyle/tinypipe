//! Daemon entegrasyon testleri: sahte worker'lar üzerinden gerçek gRPC akışı.
//!
//! `spawn_worker` gerçek bir tonic client kurar; handler her task için bir
//! `TaskResponse` üretir. Her test kendi daemon'unu (127.0.0.1:0) bind eder.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream;
use tinypipe_proto::tinypipe::v1::{
    tool_dispatch_service_client::ToolDispatchServiceClient,
    tool_worker_service_client::ToolWorkerServiceClient,
    InvokeRequest, InvokeResponse, ListToolsRequest, TaskRequest, TaskResponse, ToolDefinition,
};
use tonic::transport::Endpoint;
use tonic::Status;

use crate::{bind_and_serve, Daemon, WorkerGroup};

fn def(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: String::new(),
        input_schema_json: String::new(),
        output_schema_json: String::new(),
        pure: false,
        timeout_ms: 0,
    }
}

fn ok_response(task: &TaskRequest, output: &str) -> TaskResponse {
    TaskResponse {
        task_id: task.task_id.clone(),
        success: true,
        output_json: output.into(),
        error_message: String::new(),
        registered_tools: Vec::new(),
    }
}

/// Durdurulabilir sahte worker. `stop()` çağrılınca bağlantı kapanır.
/// Drop edilirse worker canlı kalır (Notify drop'ta tetiklenmez).
struct FakeWorker {
    stop: Option<Arc<tokio::sync::Notify>>,
}

impl FakeWorker {
    fn stop(mut self) {
        if let Some(notify) = self.stop.take() {
            notify.notify_one();
        }
    }
}

async fn spawn_worker(
    addr: std::net::SocketAddr,
    tools: Vec<ToolDefinition>,
    handler: impl Fn(TaskRequest) -> Pin<Box<dyn Future<Output = TaskResponse> + Send>>
        + Send
        + Sync
        + 'static,
) -> Result<FakeWorker, tonic::transport::Error> {
    let chan = Endpoint::new(format!("http://{addr}"))?.connect().await?;
    let mut client = ToolWorkerServiceClient::new(chan);

    // Kayıt mesajı + handler cevapları tek kanaldan akar: önce registration,
    // sonra her task için handler'ın ürettiği cevap.
    let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<TaskResponse>(16);
    let registration = TaskResponse {
        task_id: String::new(),
        success: true,
        output_json: String::new(),
        error_message: String::new(),
        registered_tools: tools,
    };

    // İlk mesaj kayıt olmalı; gerisi handler cevapları.
    resp_tx.send(registration).await.unwrap();
    let req_stream = stream::unfold(resp_rx, |mut rx| async move {
        rx.recv().await.map(|msg| (msg, rx))
    });
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_in = stop.clone();

    tokio::spawn(async move {
        let mut stream = match client.connect_worker(req_stream).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                eprintln!("worker connect failed: {e}");
                return;
            }
        };
        let run = async {
            while let Ok(Some(task)) = stream.message().await {
                let resp = handler(task).await;
                if resp_tx.send(resp).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = run => {}
            _ = stop_in.notified() => {}
        }
    });
    Ok(FakeWorker { stop: Some(stop) })
}

async fn invoke(
    addr: std::net::SocketAddr,
    tool: &str,
    args_json: &str,
) -> Result<tonic::Response<InvokeResponse>, Status> {
    let chan = Endpoint::new(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut client = ToolDispatchServiceClient::new(chan);
    client
        .invoke(InvokeRequest {
            tool_name: tool.into(),
            args_json: args_json.into(),
            kwargs_json: String::new(),
            env: HashMap::new(),
        })
        .await
}

async fn start_daemon() -> (std::net::SocketAddr, Arc<Daemon>) {
    let daemon = Arc::new(Daemon::new("127.0.0.1:0"));
    let addr = bind_and_serve(daemon.clone(), std::future::pending())
        .await
        .unwrap();
    (addr, daemon)
}

// ── Temel akış ─────────────────────────────────────────────────────

#[tokio::test]
async fn register_and_invoke_roundtrip() {
    let (addr, _) = start_daemon().await;
    spawn_worker(addr, vec![def("echo")], |task| Box::pin(async move {
        ok_response(&task, &task.args_json)
    }))
    .await
    .unwrap();

    let resp = invoke(addr, "echo", r#"{"x":1}"#).await.unwrap().into_inner();
    assert!(resp.success, "echo should succeed: {:?}", resp.error_message);
    assert_eq!(resp.output_json, r#"{"x":1}"#);
    assert!(resp.duration_us > 0);
}

#[tokio::test]
async fn kwargs_and_env_forwarded() {
    let (addr, _) = start_daemon().await;
    spawn_worker(addr, vec![def("probe")], |task| Box::pin(async move {
        ok_response(&task, &format!("kws={} env={:?}", task.kwargs_json, task.env))
    }))
    .await
    .unwrap();

    let chan = Endpoint::new(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut client = ToolDispatchServiceClient::new(chan);
    let resp = client
        .invoke(InvokeRequest {
            tool_name: "probe".into(),
            args_json: r#"[]"#.into(),
            kwargs_json: r#"{"a":1}"#.into(),
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.output_json, r#"kws={"a":1} env={"FOO": "bar"}"#);
}

// ── Worker kaydı / ListTools ───────────────────────────────────────

#[tokio::test]
async fn list_tools_sorted() {
    let (addr, _) = start_daemon().await;
    spawn_worker(addr, vec![def("zulu"), def("alpha"), def("beta")], |task| Box::pin(async move {
        ok_response(&task, "")
    }))
    .await
    .unwrap();

    let chan = Endpoint::new(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut client = ToolDispatchServiceClient::new(chan);
    let tools = client
        .list_tools(ListToolsRequest {})
        .await
        .unwrap()
        .into_inner()
        .tools;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta", "zulu"]);
}

#[tokio::test]
async fn invalid_registration_rejected() {
    let (addr, _) = start_daemon().await;
    let chan = Endpoint::new(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut client = ToolWorkerServiceClient::new(chan);

    let bad = TaskResponse {
        task_id: String::new(),
        success: true,
        output_json: String::new(),
        error_message: String::new(),
        registered_tools: vec![def("")],
    };
    let err = client
        .connect_worker(stream::iter(vec![bad]))
        .await
        .expect_err("empty tool name must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn unregistered_tool_not_found() {
    let (addr, _) = start_daemon().await;
    let err = invoke(addr, "ghost", "[]").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

// ── Round-robin ────────────────────────────────────────────────────

#[tokio::test]
async fn round_robin_across_workers() {
    let (addr, _) = start_daemon().await;
    spawn_worker(addr, vec![def("greet")], |task| Box::pin(async move {
        ok_response(&task, "worker-1")
    }))
    .await
    .unwrap();
    spawn_worker(addr, vec![def("greet")], |task| Box::pin(async move {
        ok_response(&task, "worker-2")
    }))
    .await
    .unwrap();

    let mut outputs = Vec::new();
    for _ in 0..3 {
        let resp = invoke(addr, "greet", "[]").await.unwrap().into_inner();
        assert!(resp.success);
        outputs.push(resp.output_json);
    }
    assert_eq!(outputs, ["worker-1", "worker-2", "worker-1"]);
}

#[test]
fn worker_group_round_robin_cycles() {
    let group = WorkerGroup::new();
    let (tx1, _rx1) = tokio::sync::mpsc::channel(8);
    let (tx2, _rx2) = tokio::sync::mpsc::channel(8);
    group.add("w1", tx1);
    group.add("w2", tx2);

    for _ in 0..3 {
        let (id, _) = group.get_next_worker().unwrap();
        assert_eq!(id, "w1");
        let (id, _) = group.get_next_worker().unwrap();
        assert_eq!(id, "w2");
    }
    assert_eq!(group.worker_count(), 2);
    group.remove_worker("w1");
    assert_eq!(group.worker_count(), 1);
    let (id, _) = group.get_next_worker().unwrap();
    assert_eq!(id, "w2");
}

// ── Fail-fast ──────────────────────────────────────────────────────

#[tokio::test]
async fn worker_disconnect_fails_pending_tasks_fast() {
    let (addr, _) = start_daemon().await;
    let worker = spawn_worker(addr, vec![def("slow")], |task| Box::pin(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        ok_response(&task, "late")
    }))
    .await
    .unwrap();

    let start = std::time::Instant::now();
    let invoke_fut = tokio::spawn(invoke(addr, "slow", "[]"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    worker.stop();

    let resp = tokio::time::timeout(Duration::from_secs(2), invoke_fut)
        .await
        .expect("invoke must resolve quickly after disconnect")
        .unwrap()
        .unwrap()
        .into_inner();
    assert!(!resp.success);
    assert!(resp.error_message.contains("worker disconnected"));
    assert!(start.elapsed() < Duration::from_secs(2));

    // Son worker ayrılınca tool tanımı da listeden düşer.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let chan = Endpoint::new(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ToolDispatchServiceClient::new(chan);
    let tools = client
        .list_tools(ListToolsRequest {})
        .await
        .unwrap()
        .into_inner()
        .tools;
    assert!(
        !tools.iter().any(|t| t.name == "slow"),
        "disconnected tool must disappear from list_tools: {tools:?}"
    );
}

// ── Zaman aşımı ────────────────────────────────────────────────────

#[tokio::test]
async fn per_tool_timeout_enforced() {
    let (addr, _) = start_daemon().await;
    let mut slow = def("slow");
    slow.timeout_ms = 50;
    spawn_worker(addr, vec![slow], |task| Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ok_response(&task, "late")
    }))
    .await
    .unwrap();

    let start = std::time::Instant::now();
    let err = invoke(addr, "slow", "[]").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
    assert!(err.message().contains("timed out after 50ms"));
    assert!(start.elapsed() < Duration::from_millis(400), "timeout fired too late");
}

#[tokio::test]
async fn default_timeout_used_when_tool_specifies_zero() {
    let daemon = Arc::new(Daemon::with_default_timeout(
        "127.0.0.1:0",
        Duration::from_millis(75),
    ));
    let addr = bind_and_serve(daemon, std::future::pending()).await.unwrap();
    spawn_worker(addr, vec![def("slow")], |task| Box::pin(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        ok_response(&task, "late")
    }))
    .await
    .unwrap();

    let start = std::time::Instant::now();
    let err = invoke(addr, "slow", "[]").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
    assert!(err.message().contains("timed out after 75ms"));
    assert!(start.elapsed() < Duration::from_millis(400));
}

#[tokio::test]
async fn late_response_after_timeout_does_not_poison_daemon() {
    let (addr, _) = start_daemon().await;
    let mut slow = def("slow");
    slow.timeout_ms = 50;
    let mut fast = def("fast");
    fast.timeout_ms = 2000;
    spawn_worker(addr, vec![slow, fast], |task| Box::pin(async move {
        if task.tool_name == "slow" {
            tokio::time::sleep(Duration::from_millis(200)).await;
            ok_response(&task, "late")
        } else {
            ok_response(&task, "ok")
        }
    }))
    .await
    .unwrap();

    assert_eq!(
        invoke(addr, "slow", "[]").await.unwrap_err().code(),
        tonic::Code::DeadlineExceeded
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let resp = invoke(addr, "fast", "[]").await.unwrap().into_inner();
    assert!(resp.success);
    assert_eq!(resp.output_json, "ok");
}

// ── Eşleştirme ─────────────────────────────────────────────────────

#[tokio::test]
async fn out_of_order_responses_matched_by_task_id() {
    let (addr, _) = start_daemon().await;
    spawn_worker(addr, vec![def("parity")], |task| Box::pin(async move {
        let last = task.task_id.chars().last().unwrap().to_digit(16).unwrap();
        if last % 2 == 0 {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        ok_response(&task, if last % 2 == 0 { "even" } else { "odd" })
    }))
    .await
    .unwrap();

    let mut results = Vec::new();
    for i in 0..6 {
        let args = format!(r#"{{"i":{i}}}"#);
        let handle = tokio::spawn(async move { invoke(addr, "parity", &args).await.unwrap().into_inner() });
        results.push(handle);
    }
    for (i, handle) in results.into_iter().enumerate() {
        let resp = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("all invokes must finish")
            .unwrap();
        assert!(resp.success, "task {i} failed: {}", resp.error_message);
    }
}

// ── Bağımsızlık ────────────────────────────────────────────────────

#[tokio::test]
async fn two_daemons_do_not_share_state() {
    let (addr1, _) = start_daemon().await;
    let (addr2, _) = start_daemon().await;

    spawn_worker(addr1, vec![def("echo")], |task| Box::pin(async move {
        ok_response(&task, "daemon-1")
    }))
    .await
    .unwrap();
    spawn_worker(addr2, vec![def("echo")], |task| Box::pin(async move {
        ok_response(&task, "daemon-2")
    }))
    .await
    .unwrap();

    assert_eq!(invoke(addr1, "echo", "[]").await.unwrap().into_inner().output_json, "daemon-1");
    assert_eq!(invoke(addr2, "echo", "[]").await.unwrap().into_inner().output_json, "daemon-2");
}
