//! `http_request` — HTTP isteği atar (ureq, blocking).
//!
//! kwargs: `url` (zorunlu), `method` (GET varsayılan), `headers` (object),
//! `body` (string), `timeout_secs` (30 varsayılan).
//! Sonuç: `{status, body, headers}`.

use std::collections::HashMap;
use std::time::Duration;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;
use crate::tools::{obj, str_v};

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema(
        "http_request",
        input_schema(),
        output_schema(),
        false,
        http_request,
    );
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "Target URL" },
            "method": { "type": "string", "description": "GET (default), POST, PUT, DELETE, ..." },
            "headers": { "type": "object", "description": "HTTP headers (string → string)" },
            "body": { "type": "string", "description": "Request body (for POST/PUT)" },
            "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 30)" }
        },
        "required": ["url"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": { "type": "integer" },
            "body": { "type": "string" },
            "headers": { "type": "object" }
        }
    })
}

/// HTTP isteği atar. Sonuç: `{status, body, headers}`.
pub fn http_request(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let get_str = |k: &str| kwargs.get(k).and_then(|v| v.as_str()).map(String::from);
    let url = get_str("url")
        .or_else(|| args.first().and_then(|v| v.as_str()).map(String::from))
        .ok_or_else(|| "http_request: missing 'url' (string)".to_string())?;
    let method = get_str("method")
        .unwrap_or_else(|| "GET".into())
        .to_uppercase();
    let timeout_secs = kwargs
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);
    let headers: Vec<(String, String)> = match kwargs.get("headers") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_default()))
            .collect(),
        _ => Vec::new(),
    };
    let body: Option<String> = match kwargs.get("body") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_secs))
        .build();
    let mut req = agent.request(&method, &url);
    for (k, v) in &headers {
        req = req.set(k, v);
    }
    let resp = match body {
        Some(b) => req.send_string(&b),
        None => req.call(),
    }
    .map_err(|e| format!("http_request: {} {} failed: {}", method, url, e))?;

    let status = resp.status() as i64;
    let mut resp_headers: HashMap<String, Value> = HashMap::new();
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            resp_headers.insert(name, str_v(v));
        }
    }
    let body_text = resp.into_string().unwrap_or_default();

    let mut out = HashMap::new();
    out.insert("status".into(), Value::Int(status));
    out.insert("body".into(), str_v(&body_text));
    out.insert("headers".into(), obj(resp_headers));
    Ok(obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_request_missing_url() {
        let err = http_request(&[], &HashMap::new(), &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("missing 'url'"));
    }

    #[test]
    fn test_http_request_connection_refused() {
        let mut kwargs = HashMap::new();
        kwargs.insert("url".into(), Value::String("http://127.0.0.1:1/x".into()));
        kwargs.insert("timeout_secs".into(), Value::Int(2));
        let err = http_request(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("failed"));
    }

    #[test]
    fn test_http_request_local_server() {
        // Gerçek istek — local TcpListener ile mini HTTP sunucusu.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "{\"ok\":true}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let mut kwargs = HashMap::new();
        kwargs.insert("url".into(), Value::String(format!("http://{}/ping", addr)));
        kwargs.insert("method".into(), Value::String("GET".into()));
        let result = http_request(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        handle.join().unwrap();

        let Value::Object(m) = result else {
            panic!("expected object result");
        };
        assert_eq!(m.get("status"), Some(&Value::Int(200)));
        assert_eq!(m.get("body"), Some(&Value::String("{\"ok\":true}".into())));
        let headers = m.get("headers").unwrap();
        let Value::Object(h) = headers else {
            panic!("expected headers object");
        };
        assert!(h.contains_key("content-type"));
    }

    #[test]
    fn test_http_request_post_body() {
        // POST + body gönderimi doğrula.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            // Header + body ayrı paketlerde gelebilir — Content-Length kadar oku.
            let mut request = String::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(idx) = request.find("\r\n\r\n") {
                    let clen: usize = request[..idx]
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if request.len() >= idx + 4 + clen {
                        break;
                    }
                }
            }
            let lowered = request.to_lowercase();
            assert!(lowered.starts_with("post /submit"));
            assert!(lowered.contains("content-type: application/json"));
            assert!(request.contains("{\"a\":1}"));
            let resp = "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
        });

        let mut kwargs = HashMap::new();
        kwargs.insert(
            "url".into(),
            Value::String(format!("http://{}/submit", addr)),
        );
        kwargs.insert("method".into(), Value::String("POST".into()));
        kwargs.insert("body".into(), Value::String("{\"a\":1}".into()));
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".into(),
            Value::String("application/json".into()),
        );
        kwargs.insert("headers".into(), Value::Object(headers));
        let result = http_request(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        handle.join().unwrap();
        let Value::Object(m) = result else {
            panic!("expected object result");
        };
        assert_eq!(m.get("status"), Some(&Value::Int(201)));
    }
}
