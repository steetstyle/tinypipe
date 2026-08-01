//! `.env` dosyası provider'ı.
//!
//! `KEY=VALUE` satırları, `#` yorumlar, `export ` öneki, tek/çift tırnaklı
//! değerler desteklenir. Dosya ilk erişimde okunur ve cache'lenir
//! (dosya yoksa boş katman — hata değil).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::provider::EnvProvider;

/// `.env` dosyasından okuyan provider.
pub struct DotEnvFileProvider {
    path: String,
    cache: Mutex<Option<HashMap<String, String>>>,
}

impl DotEnvFileProvider {
    pub fn new(path: impl Into<String>) -> Self {
        DotEnvFileProvider {
            path: path.into(),
            cache: Mutex::new(None),
        }
    }

    fn load(&self) -> Option<HashMap<String, String>> {
        let mut cache = self.cache.lock().unwrap();
        if cache.is_none() {
            let content = std::fs::read_to_string(&self.path).ok()?;
            *cache = Some(parse_dotenv(&content));
        }
        cache.clone()
    }
}

impl EnvProvider for DotEnvFileProvider {
    fn get(&self, key: &str) -> Option<String> {
        self.load()?.get(key).cloned()
    }

    fn list(&self) -> Vec<(String, String)> {
        self.load()
            .map(|m| m.into_iter().collect())
            .unwrap_or_default()
    }
}

/// `.env` içeriğini ayrıştırır (public: testler ve diğer provider'lar için).
pub fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_uppercase();
        if key.is_empty() {
            continue;
        }
        out.insert(key, unquote(value.trim()));
    }
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 {
        let first = v.as_bytes()[0];
        let last = v.as_bytes()[v.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return v[1..v.len() - 1].to_string();
        }
        if let Some(stripped) = v.strip_prefix('#') {
            // TODO: yorum-ortası işleme — basit durumda değerin tamamı yorum değilse
            let _ = stripped;
        }
    }
    // Satır içi ` # yorum` (tırnaksız değerlerde)
    v.split_once(" #")
        .map(|(val, _)| val.trim().to_string())
        .unwrap_or_else(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dotenv_basic() {
        let content = r#"
# yorum
DB_URL=postgres://localhost/db
export API_TOKEN=abc123
QUOTED="hello world"
SINGLE='x y'
EMPTY=
SPACED = trimmed
INLINE=value # trailing comment
"#;
        let vars = parse_dotenv(content);
        assert_eq!(vars.get("DB_URL").unwrap(), "postgres://localhost/db");
        assert_eq!(vars.get("API_TOKEN").unwrap(), "abc123");
        assert_eq!(vars.get("QUOTED").unwrap(), "hello world");
        assert_eq!(vars.get("SINGLE").unwrap(), "x y");
        assert_eq!(vars.get("EMPTY").unwrap(), "");
        assert_eq!(vars.get("SPACED").unwrap(), "trimmed");
        assert_eq!(vars.get("INLINE").unwrap(), "value");
    }

    #[test]
    fn test_parse_dotenv_missing_file_is_empty_layer() {
        let provider = DotEnvFileProvider::new("/nonexistent/path/.env");
        assert_eq!(provider.get("ANY"), None);
        assert!(provider.list().is_empty());
    }

    #[test]
    fn test_file_provider_reads_real_file() {
        let dir = std::env::temp_dir().join(format!("tinypipe_env_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "FILE_KEY=from_file\n").unwrap();
        let provider = DotEnvFileProvider::new(path.to_string_lossy().to_string());
        assert_eq!(provider.get("FILE_KEY").as_deref(), Some("from_file"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
