

/// Yayınlanmış (META `http_*`) route yapılandırması.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// Kaynak graph id.
    pub graph_id: String,
    /// Normalize edilmiş yol (baştaki `/` yok).
    pub path: String,
    /// İzin verilen HTTP metodu.
    pub method: RouteMethod,
    /// Token gerektirmez (META `http_public=true`).
    pub public: bool,
    /// VM zaman aşımı override (META `http_timeout_ms`, 0 = plan varsayılanı).
    pub timeout_ms: u32,
    /// GET yanıt önbelleği (sn, 0 = kapalı).
    pub cache_ttl: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl RouteMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteMethod::Get => "GET",
            RouteMethod::Post => "POST",
            RouteMethod::Put => "PUT",
            RouteMethod::Patch => "PATCH",
            RouteMethod::Delete => "DELETE",
            RouteMethod::Head => "HEAD",
            RouteMethod::Options => "OPTIONS",
        }
    }

    /// Tüm desteklenen metodlar (CORS preflight / `Allow` header'ı için).
    pub const ALL: [RouteMethod; 7] = [
        RouteMethod::Get,
        RouteMethod::Post,
        RouteMethod::Put,
        RouteMethod::Patch,
        RouteMethod::Delete,
        RouteMethod::Head,
        RouteMethod::Options,
    ];
}

/// Route tablosu anahtarı: `path + method`. Aynı path farklı metodlarla
/// yayınlanabilir (ör. `GET /hello` + `POST /hello` aynı graph'tan).
pub fn route_key(path: &str, method: RouteMethod) -> String {
    format!("{path}\u{1}{}", method.as_str())
}

/// `META(...)`'taki `http_*` sözleşmesi.
///
/// Desteklenen anahtarlar:
/// - `http_route` (string) — yayın yolu; kök seviyede yaşar (`/send-email`).
///   `/api`, `/healthz` önekleri rezervdir ve reddedilir.
/// - `http_method` (string) — `"GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD"`
///   (varsayılan `POST`). Geçersiz değer HATA döner, fallback yoktur.
///   Input kaynağı: GET/HEAD/DELETE → query param'lar, POST/PUT/PATCH → JSON body.
///   `OPTIONS` yayınlanamaz; yayınlanmış her path'te otomatik CORS preflight
///   (204 + `Allow`) döner.
/// - `http_public` (bool) — `true` ise token gerektirmez (varsayılan `false`).
/// - `http_timeout_ms` (u32) — VM zaman aşımı override (0 = plan varsayılanı).
/// - `http_cache_ttl` (u32) — GET yanıt önbelleği süresi sn (0 = kapalı).
///
/// `http_` önekiyle başlayan bilinmeyen anahtar da hata verir (yazım hatası yakalar).
pub fn parse_route_config(meta_json: &str, graph_name: &str) -> Result<Option<RouteConfig>, String> {
    if meta_json.trim().is_empty() {
        return Ok(None);
    }
    let meta: serde_json::Value = serde_json::from_str(meta_json)
        .map_err(|e| format!("graph '{graph_name}': invalid META JSON: {e}"))?;
    let obj = match meta {
        serde_json::Value::Object(map) => map,
        _ => return Err(format!("graph '{graph_name}': META must be a JSON object")),
    };

    let Some(route_val) = obj.get("http_route") else {
        return Ok(None); // yayınlanmamış
    };

    let mut unknown: Vec<&String> = obj
        .keys()
        .filter(|k| k.starts_with("http_") && !matches!(k.as_str(),
            "http_route" | "http_method" | "http_public" | "http_timeout_ms" | "http_cache_ttl"))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(format!(
            "graph '{graph_name}': unknown http_* META keys: {}",
            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }

    let route_raw = match route_val {
        serde_json::Value::String(s) => s.clone(),
        _ => return Err(format!("graph '{graph_name}': `http_route` must be a string")),
    };
    let path = route_raw.trim().trim_start_matches('/').to_string();
    if path.is_empty() {
        return Err(format!("graph '{graph_name}': `http_route` must not be empty"));
    }
    if path.len() > 128 {
        return Err(format!("graph '{graph_name}': `http_route` too long (max 128)"));
    }
    let lower = path.to_lowercase();
    if lower.starts_with("api") || lower.starts_with("healthz") || lower.starts_with("assets")
        || lower.starts_with("static")
    {
        return Err(format!(
            "graph '{graph_name}': `http_route` '/{path}' collides with a reserved path (api/healthz/assets/static)"
        ));
    }
    for ch in path.chars() {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.' | '~')) {
            return Err(format!(
                "graph '{graph_name}': `http_route` '/{path}' contains invalid character '{ch}'"
            ));
        }
    }

    let method = match obj.get("http_method") {
        None | Some(serde_json::Value::Null) => RouteMethod::Post,
        Some(serde_json::Value::String(s)) => match s.to_uppercase().as_str() {
            "GET" => RouteMethod::Get,
            "POST" => RouteMethod::Post,
            "PUT" => RouteMethod::Put,
            "PATCH" => RouteMethod::Patch,
            "DELETE" => RouteMethod::Delete,
            "HEAD" => RouteMethod::Head,
            // OPTIONS graph çalıştırmaz: her yayınlanmış path'te otomatik CORS
            // preflight olarak servis edilir; ayrıca yayınlanması anlamsız.
            "OPTIONS" => {
                return Err(format!(
                    "graph '{graph_name}': `http_method` OPTIONS is served automatically as CORS preflight; publish a different method"
                ))
            }
            other => {
                return Err(format!(
                    "graph '{graph_name}': `http_method` must be GET, POST, PUT, PATCH, DELETE, HEAD or OPTIONS (got \"{other}\")"
                ))
            }
        },
        Some(_) => {
            return Err(format!(
                "graph '{graph_name}': `http_method` must be a string (GET, POST, PUT, PATCH, DELETE, HEAD or OPTIONS)"
            ))
        }
    };

    let public = match obj.get("http_public") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        // DSL META sözdizimi bare `true`/`false` kabul etmediğinden
        // string olarak da saklanabilir ("true" / "1" / "yes").
        Some(serde_json::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" | "" => false,
            other => {
                return Err(format!(
                    "graph '{graph_name}': `http_public` must be a boolean (got \"{other}\")"
                ))
            }
        },
        Some(_) => {
            return Err(format!(
                "graph '{graph_name}': `http_public` must be a boolean"
            ))
        }
    };

    let timeout_ms = match obj.get("http_timeout_ms") {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::Number(n)) => {
            let v = n.as_u64().ok_or_else(|| {
                format!("graph '{graph_name}': `http_timeout_ms` must be a non-negative integer")
            })?;
            if v > u32::MAX as u64 {
                return Err(format!(
                    "graph '{graph_name}': `http_timeout_ms` too large (max {})",
                    u32::MAX
                ));
            }
            v as u32
        }
        Some(serde_json::Value::String(s)) => {
            let v: u64 = s.trim().parse().map_err(|_| {
                format!(
                    "graph '{graph_name}': `http_timeout_ms` must be an integer (got \"{s}\")"
                )
            })?;
            if v > u32::MAX as u64 {
                return Err(format!(
                    "graph '{graph_name}': `http_timeout_ms` too large (max {})",
                    u32::MAX
                ));
            }
            v as u32
        }
        Some(_) => {
            return Err(format!(
                "graph '{graph_name}': `http_timeout_ms` must be an integer (ms)"
            ))
        }
    };

    let cache_ttl = match obj.get("http_cache_ttl") {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::Number(n)) => {
            let v = n.as_u64().ok_or_else(|| {
                format!("graph '{graph_name}': `http_cache_ttl` must be a non-negative integer")
            })?;
            if v > u32::MAX as u64 {
                return Err(format!(
                    "graph '{graph_name}': `http_cache_ttl` too large (max {})",
                    u32::MAX
                ));
            }
            v as u32
        }
        Some(serde_json::Value::String(s)) => {
            let v: u64 = s.trim().parse().map_err(|_| {
                format!(
                    "graph '{graph_name}': `http_cache_ttl` must be an integer (got \"{s}\")"
                )
            })?;
            if v > u32::MAX as u64 {
                return Err(format!(
                    "graph '{graph_name}': `http_cache_ttl` too large (max {})",
                    u32::MAX
                ));
            }
            v as u32
        }
        Some(_) => {
            return Err(format!(
                "graph '{graph_name}': `http_cache_ttl` must be an integer (seconds)"
            ))
        }
    };

    Ok(Some(RouteConfig {
        graph_id: String::new(), // çağıran doldurur
        path,
        method,
        public,
        timeout_ms,
        cache_ttl,
    }))
}
