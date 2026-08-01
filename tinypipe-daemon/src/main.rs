//! `tinypipe-daemon` binary — worker'ları ağırlayan gRPC daemon.
//!
//! Yapılandırma (env):
//! - `TINYPIPE_DAEMON_ADDR` — dinlenecek adres (default `127.0.0.1:50051`)
//! - `TINYPIPE_DAEMON_DEFAULT_TIMEOUT_MS` — tool başına varsayılan zaman aşımı
//!   (default 30000; tool kendi `timeout_ms`'ini bildirirse o kazanır)
//! - `TINYPIPE_DAEMON_KEEPALIVE_MS` — HTTP/2 + TCP keepalive aralığı (default 30000)

use std::time::Duration;

use tinypipe_daemon::Daemon;
use tinypipe_proto::tinypipe::v1::{
    tool_dispatch_service_server::ToolDispatchServiceServer,
    tool_worker_service_server::ToolWorkerServiceServer,
};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let addr = std::env::var("TINYPIPE_DAEMON_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());
    let timeout_ms = env_u64("TINYPIPE_DAEMON_DEFAULT_TIMEOUT_MS", 30_000);
    let keepalive_ms = env_u64("TINYPIPE_DAEMON_KEEPALIVE_MS", 30_000);
    let keepalive = Duration::from_millis(keepalive_ms.max(1_000));

    tracing::info!(
        addr = %addr,
        default_timeout_ms = timeout_ms,
        keepalive_ms = keepalive_ms,
        "tinypipe-daemon starting"
    );

    let daemon = std::sync::Arc::new(
        Daemon::with_default_timeout(addr.clone(), Duration::from_millis(timeout_ms.max(1)))
            .with_api_key(resolve_api_key()),
    );
    let socket_addr = addr.parse()?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // TCP keepalive: yarım açık worker bağlantılarını erken tespit eder
            // (worker çöktü ama EOF gelmedi senaryosu). Bind runtime içinden
            // yapılmalı (reactor gerektirir).
            let incoming = tonic::transport::server::TcpIncoming::bind(socket_addr)?
                .with_nodelay(Some(true))
                .with_keepalive(Some(keepalive))
                .with_keepalive_interval(Some(Duration::from_secs(10)))
                .with_keepalive_retries(Some(3));

            tonic::transport::Server::builder()
                .http2_keepalive_interval(Some(keepalive))
                .http2_keepalive_timeout(Some(Duration::from_secs(10)))
                .add_service(ToolWorkerServiceServer::new(tinypipe_daemon::DaemonServer(
                    daemon.clone(),
                )))
                .add_service(ToolDispatchServiceServer::new(tinypipe_daemon::DaemonServer(
                    daemon,
                )))
                .serve_with_incoming_shutdown(incoming, shutdown_signal())
                .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        })?;
    tracing::info!("tinypipe-daemon stopped");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Worker kayıt API anahtarı çözümleme sırası:
///   1. `--no-auth` / `TINYPIPE_DAEMON_NO_AUTH=1` → auth tamamen kapalı (yerel dev).
///   2. `--api-key <key>` / `TINYPIPE_DAEMON_API_KEY=<key>` → verilen anahtar.
///   3. Hiçbiri yok → rastgele anahtar üretilir ve loglanır (güvenlik default açık).
fn resolve_api_key() -> Option<String> {
    let mut args = std::env::args().skip(1);
    let mut flag_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--no-auth" => return None,
            "--api-key" => flag_key = args.next(),
            other if other.starts_with("--api-key=") => {
                flag_key = Some(other["--api-key=".len()..].to_string())
            }
            other if other.starts_with("--no-auth=") => {
                if &other["--no-auth=".len()..] == "1" {
                    return None;
                }
            }
            _ => {}
        }
    }

    if std::env::var("TINYPIPE_DAEMON_NO_AUTH").as_deref() == Ok("1") {
        return None;
    }

    if let Some(key) = flag_key.filter(|k| !k.is_empty()) {
        tracing::info!("worker auth: enabled (key from --api-key / TINYPIPE_DAEMON_API_KEY)");
        return Some(key);
    }
    if let Ok(key) = std::env::var("TINYPIPE_DAEMON_API_KEY") {
        if !key.trim().is_empty() {
            tracing::info!("worker auth: enabled (key from TINYPIPE_DAEMON_API_KEY)");
            return Some(key);
        }
    }

    let generated = uuid::Uuid::new_v4().to_string();
    tracing::warn!(
        "worker auth: NO key configured — generated random key; workers must set TINYPIPE_DAEMON_API_KEY={}",
        generated
    );
    tracing::warn!(
        "worker auth: to disable (local dev only) run with --no-auth or TINYPIPE_DAEMON_NO_AUTH=1"
    );
    Some(generated)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
