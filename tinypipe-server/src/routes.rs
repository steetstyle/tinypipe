use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::engine::run_execution;
use crate::meta::{RouteMethod, route_key};
use crate::state::AppState;

/// Yayınlanmış route'ların kök catch-all'u (`/{*route}`).
///
/// Input kaynağı metoda göre değişir:
/// - GET/HEAD/DELETE → query param'lar
/// - POST/PUT/PATCH → JSON body (1 MiB sınırı)
/// - OPTIONS → graph çalıştırmaz; CORS preflight yanıtı (204 + `Allow`)
///
/// ## Güvenlik sözleşmesi (header/cookie izolasyonu)
/// Publish handler **hiçbir request header'ını okumaz ve graph'a iletmez**:
/// inputs yalnızca query param'lardan veya JSON body'den gelir, env her zaman
/// boştur. `Authorization`, `Cookie` vb. header'lar ne graph'a ne tool'lara
/// asla ulaşır; 405/400 yanıtları bile yalnızca yol/metod bilgisi içerir.
/// (API anahtarı yalnızca `/api/*` rotalarında `Authorization: Bearer` okunur —
/// bu header publish yollarında kullanılmaz.)
///
/// `http_cache_ttl` varsa yanıt `X-Tinypipe-Cache: HIT/MISS` ile servis edilir.
pub async fn publish(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let path_no_slash = path.strip_prefix('/').unwrap_or(&path).to_string();
    let method = req.method().clone();

    // OPTIONS: yayınlanmış her path'te otomatik CORS preflight (graph çalışmaz).
    if method == Method::OPTIONS {
        return options_preflight(&state, &path_no_slash).await;
    }

    let rc = {
        let routes = state.routes.read().await;
        req_method(method.as_str())
            .map(|m| route_key(&path_no_slash, m))
            .and_then(|key| routes.get(&key).cloned())
    };
    let rc = match rc {
        Some(rc) => rc,
        None => return method_miss(&state, &path_no_slash, &method).await,
    };

    let is_head = method == Method::HEAD;

    let inputs = match method.as_str() {
        // GET/HEAD/DELETE: body yok, inputs query param'lardan.
        "GET" | "HEAD" | "DELETE" => parse_query(req.uri().query().unwrap_or("")),
        // POST/PUT/PATCH: JSON body.
        _ => match to_bytes(req.into_body(), 1_048_576).await {
            Ok(bytes) if bytes.is_empty() => serde_json::json!({}),
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "POST/PUT/PATCH body must be a JSON object",
                        })),
                    )
                        .into_response();
                }
            },
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("body read failed: {e}") })),
                )
                    .into_response();
            }
        },
    };

    let state2 = state.clone();
    let rc2 = rc.clone();
    let inputs2 = inputs.clone();

    let key = if rc.cache_ttl > 0 {
        let mut buf = path_no_slash.as_bytes().to_vec();
        buf.push(0);
        buf.extend_from_slice(&serde_json::to_vec(&inputs).unwrap_or_default());
        Some(AppState::fnv1a(&buf))
    } else {
        None
    };

    if let Some(k) = key {
        if let Some((expires, body)) = state.resp_cache.read().await.get(&k) {
            if *expires > std::time::Instant::now() {
                return cache_response(StatusCode::OK, body.clone(), true);
            }
        }
    }

    let plan = state2.plans.read().await.get(&rc2.graph_id).cloned();
    let Some(plan) = plan else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "plan not loaded for published graph" })),
        )
            .into_response();
    };

    let audit = state2.audit;
    let timeout = if rc2.timeout_ms > 0 { Some(rc2.timeout_ms) } else { None };
    let outcome = tokio::task::spawn_blocking(move || {
        run_execution(&state2, None, (*plan).clone(), &inputs2, &HashMap::new(), None, true, timeout, audit)
    })
    .await;

    let outcome = match outcome {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let status = if e.contains("not found") {
                StatusCode::NOT_FOUND
            } else if e.starts_with("missing environment variables") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (status, Json(serde_json::json!({ "error": e }))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("task join: {e}") })),
            )
                .into_response();
        }
    };

    let status = match outcome.get("status").and_then(|s| s.as_str()) {
        Some("completed") => StatusCode::OK,
        Some("paused") => StatusCode::ACCEPTED,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };

    let body_bytes = match serde_json::to_vec(&outcome) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("serialize: {e}") })),
            )
                .into_response();
        }
    };

    if let Some(k) = key {
        if status == StatusCode::OK && rc.cache_ttl > 0 {
            state
                .resp_cache
                .write()
                .await
                .insert(k, (std::time::Instant::now() + std::time::Duration::from_secs(rc.cache_ttl as u64), body_bytes.clone()));
        }
    }

    let resp = cache_response(status, body_bytes, false);
    if is_head {
        // HEAD: GET ile aynı status + header'lar, gövde yok.
        let parts = resp.into_parts().0;
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = parts.status;
        *resp.headers_mut() = parts.headers;
        return resp;
    }
    resp
}

fn cache_response(status: StatusCode, body: Vec<u8>, hit: bool) -> Response {
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(if hit { "max-age=300" } else { "no-store" }),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-tinypipe-cache"),
        header::HeaderValue::from_static(if hit { "HIT" } else { "MISS" }),
    );
    resp
}

/// İstek metodu → `RouteMethod` (bilinmeyen metod `None` döner, 405'e düşer).
fn req_method(m: &str) -> Option<RouteMethod> {
    match m {
        "GET" => Some(RouteMethod::Get),
        "POST" => Some(RouteMethod::Post),
        "PUT" => Some(RouteMethod::Put),
        "PATCH" => Some(RouteMethod::Patch),
        "DELETE" => Some(RouteMethod::Delete),
        "HEAD" => Some(RouteMethod::Head),
        _ => None,
    }
}

/// Route tablosunda path'in yayınlanan metodlarını döner (Allow header'ı için).
async fn published_methods(state: &Arc<AppState>, path: &str) -> Vec<RouteMethod> {
    let routes = state.routes.read().await;
    RouteMethod::ALL
        .iter()
        .filter(|m| routes.contains_key(&route_key(path, **m)))
        .copied()
        .collect()
}

/// Path'teki route istek metodunu kabul etmiyor → 405 + `Allow`.
async fn method_miss(state: &Arc<AppState>, path: &str, method: &Method) -> Response {
    let allow = published_methods(state, path).await;
    if allow.is_empty() {
        return not_found(state, path).await;
    }
    let allow_str = allow.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
    let mut resp = (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({
            "error": format!("route '/{path}' does not accept {method}; allowed: {allow_str}"),
        })),
    )
        .into_response();
    if let Ok(v) = header::HeaderValue::from_str(&allow_str) {
        resp.headers_mut().insert(header::ALLOW, v);
    }
    resp
}

/// OPTIONS preflight: graph çalışmaz; yalnızca yayınlanmış path'lerde 204 döner.
async fn options_preflight(state: &Arc<AppState>, path: &str) -> Response {
    let allow = published_methods(state, path).await;
    if allow.is_empty() {
        return not_found(state, path).await;
    }
    let allow_str = allow.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    if let Ok(v) = header::HeaderValue::from_str(&allow_str) {
        resp.headers_mut().insert(header::ALLOW, v);
    }
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        header::HeaderValue::from_str(&allow_str).unwrap_or_else(|_| header::HeaderValue::from_static("GET, POST")),
    );
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        header::HeaderValue::from_static("Content-Type"),
    );
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_MAX_AGE,
        header::HeaderValue::from_static("86400"),
    );
    resp
}

async fn not_found(state: &Arc<AppState>, path: &str) -> Response {
    // Yol bazlı liste (aynı path birden çok metod yayınlayabilir).
    let mut published: Vec<String> = state
        .routes
        .read()
        .await
        .keys()
        .map(|k| k.split('\u{1}').next().unwrap_or(k).to_string())
        .collect();
    published.sort();
    published.dedup();
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": format!("no published route for '{}'", path),
            "published": published,
        })),
    )
        .into_response()
}

/// `?a=1&b=hello` → `{"a":1,"b":"hello"}` (sayı/bool denenir, sonra string).
fn parse_query(raw: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        };
        if !k.is_empty() {
            map.insert(k, parse_scalar(v));
        }
    }
    serde_json::Value::Object(map)
}

fn parse_scalar(s: String) -> serde_json::Value {
    match s.as_str() {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" => return serde_json::Value::Null,
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        serde_json::Value::Number(n.into())
    } else if let Ok(f) = s.parse::<f64>() {
        serde_json::json!(f)
    } else {
        serde_json::Value::String(s)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_inputs_are_the_only_get_channel() {
        // İzolasyon garantisinin testi: publish handler'ı yalnızca
        // query param'ları okur; header değerleri bu parse'ın girişinde
        // yoktur ve başka hiçbir kanaldan inputs'a karışamaz.
        let v = parse_query("a=1&b=hello%20world&flag=true&empty&nope=null&e=2.5");
        assert_eq!(v["a"], serde_json::json!(1));
        assert_eq!(v["b"], serde_json::json!("hello world"));
        assert_eq!(v["flag"], serde_json::json!(true));
        assert_eq!(v["empty"], serde_json::json!(""));
        assert_eq!(v["nope"], serde_json::Value::Null);
        assert_eq!(v["e"], serde_json::json!(2.5));
        assert!(!v.as_object().unwrap().contains_key("Authorization"));
        assert!(!v.as_object().unwrap().contains_key("Cookie"));
    }

    #[test]
    fn query_percent_decoding() {
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("k%C3%BC%C3%A7%C3%BCk"), "küçük");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn query_bool_and_number_scalars() {
        assert_eq!(parse_scalar("42".into()), serde_json::json!(42));
        assert_eq!(parse_scalar("-7".into()), serde_json::json!(-7));
        assert_eq!(parse_scalar("3.14".into()), serde_json::json!(3.14));
        assert_eq!(parse_scalar("true".into()), serde_json::json!(true));
        assert_eq!(parse_scalar("0".into()), serde_json::json!(0));
    }
}
