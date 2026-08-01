//! Köprü entegrasyon testi: gerçek in-process daemon + sahte worker üzerinden
//! `register_daemon_tools` → registry dispatch → daemon invoke → worker cevabı.

use std::collections::HashMap;
use std::sync::Arc;

use futures::stream;
use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{CallTarget, Context, Value};
use tinypipe_daemon::Daemon;
use tinypipe_proto::tinypipe::v1::{
    tool_worker_service_client::ToolWorkerServiceClient, TaskResponse, ToolDefinition,
};
use tinypipe_tools::daemon::{invoke_daemon_tool, register_daemon_tools};
use tonic::transport::Endpoint;

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

async fn spawn_echo_worker(addr: std::net::SocketAddr, tool: &str) {
    let chan = Endpoint::new(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ToolWorkerServiceClient::new(chan);
    let registration = TaskResponse {
        task_id: String::new(),
        success: true,
        output_json: String::new(),
        error_message: String::new(),
        registered_tools: vec![def(tool)],
    };
    let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<TaskResponse>(16);
    resp_tx.send(registration).await.unwrap();
    let req_stream = stream::unfold(resp_rx, |mut rx| async move {
        rx.recv().await.map(|msg| (msg, rx))
    });
    tokio::spawn(async move {
        let mut stream = match client.connect_worker(req_stream).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                eprintln!("worker connect failed: {e}");
                return;
            }
        };
        while let Ok(Some(task)) = stream.message().await {
            let resp = TaskResponse {
                task_id: task.task_id.clone(),
                success: true,
                output_json: format!(r#"{{"echoed":{}}}"#, task.args_json),
                error_message: String::new(),
                registered_tools: Vec::new(),
            };
            if resp_tx.send(resp).await.is_err() {
                break;
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_registers_and_dispatches_remote_tool() {
    let daemon = Arc::new(Daemon::new("127.0.0.1:0"));
    let addr = tinypipe_daemon::bind_and_serve(daemon, std::future::pending())
        .await
        .unwrap();
    let addr_str = addr.to_string();
    spawn_echo_worker(addr, "remote.echo").await;

    // Köprü daemon'a bağlanıp tool'ları kaydeder.
    let reg = tinypipe_tools::MockToolRegistry::new();
    let n = register_daemon_tools(&reg, &addr_str).unwrap();
    assert_eq!(n, 1);
    assert!(reg.resolve("remote.echo", "0").is_ok());

    // Registry üzerinden dispatch → daemon → worker → cevap.
    let mut ct = CallTarget::new("remote.echo");
    ct.args.push(Value::Int(7));
    let result = reg
        .dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty())
        .unwrap();
    assert_eq!(
        result,
        Value::Object(HashMap::from([(
            "echoed".to_string(),
            Value::Array(vec![Value::Int(7)])
        )]))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_no_daemon_returns_actionable_error() {
    let err = invoke_daemon_tool("127.0.0.1:1", "x", "[]", "{}", HashMap::new()).unwrap_err();
    assert!(err.contains("daemon çalışıyor mu"), "err: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_dispatches_kwargs_and_env() {
    let daemon = Arc::new(Daemon::new("127.0.0.1:0"));
    let addr = tinypipe_daemon::bind_and_serve(daemon, std::future::pending())
        .await
        .unwrap();
    let addr_str = addr.to_string();
    let chan = Endpoint::new(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut client = ToolWorkerServiceClient::new(chan);
    let registration = TaskResponse {
        task_id: String::new(),
        success: true,
        output_json: String::new(),
        error_message: String::new(),
        registered_tools: vec![def("probe")],
    };
    let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<TaskResponse>(16);
    resp_tx.send(registration).await.unwrap();
    let req_stream = stream::unfold(resp_rx, |mut rx| async move {
        rx.recv().await.map(|msg| (msg, rx))
    });
    tokio::spawn(async move {
        let mut stream = client.connect_worker(req_stream).await.unwrap().into_inner();
        while let Ok(Some(task)) = stream.message().await {
            let resp = TaskResponse {
                task_id: task.task_id.clone(),
                success: true,
                output_json: format!(
                    r#"{{"kws":{}, "env":{:?}}}"#,
                    task.kwargs_json, task.env
                ),
                error_message: String::new(),
                registered_tools: Vec::new(),
            };
            resp_tx.send(resp).await.unwrap();
        }
    });

    let reg = tinypipe_tools::MockToolRegistry::new();
    register_daemon_tools(&reg, &addr_str).unwrap();

    let mut ct = CallTarget::new("probe");
    ct.kwargs.insert("key".into(), Value::String("v".into()));
    let env = tinypipe_env::Env::new(vec![Arc::new(
        tinypipe_env::static_provider::StaticEnvProvider::new(HashMap::from([(
            "MY_VAR".to_string(),
            "1".to_string(),
        )])),
    )]);
    let result = reg.dispatch(&ct, &Context::new(), &env).unwrap();
    let Value::Object(map) = result else {
        panic!("expected object");
    };
    assert_eq!(
        map.get("kws"),
        Some(&Value::Object(HashMap::from([(
            "key".to_string(),
            Value::String("v".into())
        )])))
    );
    let Value::Object(env_map) = map.get("env").unwrap() else {
        panic!("expected env object");
    };
    assert_eq!(
        env_map.get("MY_VAR"),
        Some(&Value::String("1".into())),
        "env not forwarded: {env_map:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_builtin_name_wins_over_remote() {
    let daemon = Arc::new(Daemon::new("127.0.0.1:0"));
    let addr = tinypipe_daemon::bind_and_serve(daemon, std::future::pending())
        .await
        .unwrap();
    let addr_str = addr.to_string();
    spawn_echo_worker(addr, "array.len").await;

    let reg = tinypipe_tools::default_tools();
    let n = register_daemon_tools(&reg, &addr_str).unwrap();
    assert_eq!(n, 0, "array.len built-in kazanmalı, remote kaydedilmemeli");

    let mut ct = CallTarget::new("array.len");
    ct.args.push(Value::Array(vec![Value::Int(1), Value::Int(2)]));
    let result = reg
        .dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty())
        .unwrap();
    assert_eq!(result, Value::Int(2));
}
