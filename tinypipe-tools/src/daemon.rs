//! `tinypipe-tools/src/daemon.rs` — gRPC tool daemon köprüsü.
//!
//! Worker'lar `tinypipe-daemon`'a kaydolur; CLI/VM (senkron) bu köprü üzerinden
//! daemon'a unary `Invoke`/`ListTools` çağrısı yapar. Köprü kendi tokio
//! runtime'ını (`OnceLock`) sürdürür — VM'i async hale getirmek gerekmez.
//!
//! Hata davranışı:
//! - Daemon çalışmıyorsa çağrı hızlı `ConnectionRefused` ile döner;
//!   `register_daemon_tools` sessizce 0 tool kaydeder (graph'lar built-in
//!   tool'larla çalışmaya devam eder), `invoke_daemon_tool` açıklayıcı hata döner.
//! - Worker yoksa daemon `not_found` ("tool 'X' has no registered worker"),
//! - Worker kopmuşsa fail-fast `worker disconnected` döner.

use std::collections::HashMap;
use std::sync::OnceLock;

use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::Value;
use tinypipe_proto::tinypipe::v1::tool_dispatch_service_client::ToolDispatchServiceClient;
use tinypipe_proto::tinypipe::v1::{InvokeRequest, InvokeResponse, ListToolsRequest, ToolDefinition};

pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:50051";

/// Daemon adresi: `TINYPIPE_DAEMON_ADDR` env'i, yoksa varsayılan.
pub fn daemon_addr_from_env() -> String {
    std::env::var("TINYPIPE_DAEMON_ADDR").unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_string())
}

/// Köprü için paylaşılan tokio runtime. `block_on` VM dispatch'i içinden
/// çağrıldığından runtime ayrı bir iş parçacığı havuzu kurar (nested yok).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .expect("tinypipe-tools: failed to start bridge runtime")
    })
}

/// Senkron bağlamdan future çalıştırır:
/// - Zaten bir tokio runtime içindeysek (test/daemon bağlamı) `block_in_place`
///   ile mevcut runtime kullanılır — yeni runtime kurmak patlar.
/// - Değilse köprünün kendi runtime'ında çalıştırılır.
fn bridge_block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => runtime().block_on(fut),
    }
}

// ── gRPC yardımcıları ──────────────────────────────────────────────

async fn connect(addr: &str) -> Result<ToolDispatchServiceClient<tonic::transport::Channel>, String> {
    let endpoint = tonic::transport::Endpoint::new(format!("http://{addr}"))
        .map_err(|e| format!("invalid daemon address '{addr}': {e}"))?;
    endpoint
        .connect()
        .await
        .map(ToolDispatchServiceClient::new)
        .map_err(|e| format!("daemon connection failed ({addr}): {e} — daemon çalışıyor mu? `tinypipe daemon` komutunu deneyin"))
}

/// Daemon'dan kayıtlı tool'ları çeker. Daemon yoksa hata döner.
pub fn list_daemon_tools(addr: &str) -> Result<Vec<ToolDefinition>, String> {
    bridge_block_on(async {
        let mut client = connect(addr).await?;
        let resp = client
            .list_tools(ListToolsRequest {})
            .await
            .map_err(|e| format!("list_tools failed: {e}"))?;
        Ok(resp.into_inner().tools)
    })
}

/// Daemon'a tek tool çağrısı yapar (senkron). Raw cevap döner — arayan
/// `success`/`error_message`'ı kendine göre yorumlar.
pub fn invoke_daemon_tool(
    addr: &str,
    tool: &str,
    args_json: &str,
    kwargs_json: &str,
    env: HashMap<String, String>,
) -> Result<InvokeResponse, String> {
    bridge_block_on(async {
        let mut client = connect(addr).await?;
        let resp = client
            .invoke(InvokeRequest {
                tool_name: tool.to_string(),
                args_json: args_json.to_string(),
                kwargs_json: kwargs_json.to_string(),
                env,
            })
            .await
            .map_err(|e| format!("invoke '{tool}' failed: {e}"))?;
        Ok(resp.into_inner())
    })
}

// ── Tool kaydı ─────────────────────────────────────────────────────

/// Daemon'ın tool'larını registry'ye closure olarak kaydeder.
/// Dönüş: kaydedilen tool sayısı.
///
/// - Daemon yoksa / listelenemezse `Err` döner (arayan sessizce geçebilir).
/// - Registry'de zaten kayıtlı (built-in) isimle çakışan tool'lar **atlanır**
///   — yerel tanım kazanır, çakışanlar dönen sayıya dahil edilmez.
pub fn register_daemon_tools(reg: &crate::MockToolRegistry, addr: &str) -> Result<usize, String> {
    let tools = list_daemon_tools(addr)?;
    let mut registered = 0;
    for def in &tools {
        if reg.resolve(&def.name, "0").is_ok() {
            continue; // built-in kazanır
        }
        let name = def.name.clone();
        let addr = addr.to_string();
        let name_key = name.clone();
        reg.add(&name_key, move |args, kwargs, env| {
            let args_json = tp_to_json(&Value::Array(args.to_vec())).to_string();
            let kwargs_json = tp_to_json(&Value::Object(kwargs.clone())).to_string();
            let env_map: HashMap<String, String> = env.list().into_iter().collect();
            let resp = invoke_daemon_tool(&addr, &name, &args_json, &kwargs_json, env_map)?;
            if !resp.success {
                return Err(if resp.error_message.is_empty() {
                    format!("tool '{name}' failed")
                } else {
                    resp.error_message
                });
            }
            serde_json::from_str(&resp.output_json).map(json_to_tp).map_err(|e| {
                format!(
                    "tool '{name}' returned invalid JSON ({e}): {}",
                    truncate(&resp.output_json, 200)
                )
            })
        });
        registered += 1;
    }
    Ok(registered)
}

// ── Değer dönüşümleri (CLI ile paylaşılır) ─────────────────────────

/// Convert serde_json::Value to tinypipe_api::Value.
pub fn json_to_tp(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(arr) => Value::Array(arr.into_iter().map(json_to_tp).collect()),
        serde_json::Value::Object(obj) => Value::Object(
            obj.into_iter().map(|(k, v)| (k, json_to_tp(v))).collect(),
        ),
    }
}

/// Convert tinypipe_api::Value to serde_json::Value.
pub fn tp_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(tp_to_json).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), tp_to_json(v));
            }
            serde_json::Value::Object(out)
        }
    }
}

/// UTF-8 güvenli kısaltma.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_tp_roundtrip() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"a":1,"b":[1.5,true,null,"x"],"c":{"d":-2}}"#).unwrap();
        let tp = json_to_tp(json.clone());
        assert_eq!(tp_to_json(&tp), json);
    }

    #[test]
    fn register_daemon_tools_no_daemon_is_err() {
        let reg = crate::MockToolRegistry::new();
        // Kapalı port: hızlı hata beklenir.
        assert!(register_daemon_tools(&reg, "127.0.0.1:1").is_err());
    }

    #[test]
    fn daemon_addr_env_default() {
        std::env::remove_var("TINYPIPE_DAEMON_ADDR");
        assert_eq!(daemon_addr_from_env(), DEFAULT_DAEMON_ADDR);
    }
}
