//! `tinypipe-env` — ortam değişkeni soyutlaması.
//!
//! HashiCorp-tarzı entegrasyonlara (Vault, Consul, cloud secret manager)
//! hazır bir mimari: her şey bir `EnvProvider` trait'i arkasında. Şu an
//! dosya/OS tabanlı provider'lar var; ileride `VaultEnvProvider` vb. aynı
//! trait'i implemente edip `Env`'e eklenebilir — tüketici kod değişmez.
//!
//! Katmanlar: `Env` sıralı provider listesi tutar, **ilk kazanan** (first-wins)
//! önceliğiyle çözer. Örnek zincir: CLI override'ları → `.env` dosyası → OS env.
//!
//! Modül scope'u: `env.scoped("seed_users")` ile bir grafiğe/modüle özel görünüm
//! alınır — `get("URL")` önce `SEED_USERS_URL`'yi, bulamazsa `URL`'yi arar.
//! Nested scope'lar `PARENT_CHILD_KEY` gibi birleşir ve kademeli geri döner.

pub mod dotenv;
pub mod os;
pub mod provider;
pub mod static_provider;
pub mod template;

pub use provider::EnvProvider;

use std::collections::HashSet;
use std::sync::Arc;

/// Ortam çözümleme hataları.
#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    /// `${KEY:?}` gibi zorunlu referans eksikse.
    #[error("required environment variable not found: {0}")]
    MissingRequired(String),
    /// Template çözümleme hatası.
    #[error("template resolution failed: {0}")]
    Template(String),
}

/// Anahtar normalizasyonu: trim + büyük harf + `-`/`.`/boşluk → `_`.
/// (Terraform/Vault-tarzı ortam anahtarlarıyla uyumlu.)
pub fn normalize_key(key: &str) -> String {
    key.trim()
        .to_uppercase()
        .replace('-', "_")
        .replace('.', "_")
        .replace(' ', "_")
}

/// Çözülmüş ortam görünümü: sıralı provider katmanları + modül scope zinciri.
#[derive(Clone)]
pub struct Env {
    providers: Arc<Vec<Arc<dyn EnvProvider>>>,
    /// Modül scope zinciri, en içteki son sırada: `["SEED", "COMMENTS"]`.
    scopes: Arc<Vec<String>>,
}

impl Default for Env {
    fn default() -> Self {
        Self::empty()
    }
}

impl Env {
    /// Boş ortam (provider yok).
    pub fn empty() -> Self {
        Self::new(vec![])
    }

    /// Sıralı provider'lardan ortam kurar. İlk provider kazanır.
    pub fn new(providers: Vec<Arc<dyn EnvProvider>>) -> Self {
        Env {
            providers: Arc::new(providers),
            scopes: Arc::new(Vec::new()),
        }
    }

    /// OS process env'inden ortam kurar.
    pub fn from_process_env() -> Self {
        Self::new(vec![Arc::new(os::OsEnvProvider)])
    }

    /// `.env` dosyasından ortam kurar (dosya yoksa boş katman).
    pub fn from_file(path: impl Into<String>) -> Self {
        Self::new(vec![Arc::new(dotenv::DotEnvFileProvider::new(path))])
    }

    /// Bir modüle scope'lu görünüm döndürür.
    /// `get("URL")` → `SEED_USERS_URL` → `URL` sırasıyla aranır.
    pub fn scoped(&self, module: &str) -> Env {
        let mut scopes = (*self.scopes).clone();
        scopes.push(normalize_key(module));
        Env {
            providers: self.providers.clone(),
            scopes: Arc::new(scopes),
        }
    }

    /// Değişken okur (scope zinciri + normalizasyon ile).
    pub fn get(&self, key: &str) -> Option<String> {
        let normalized = normalize_key(key);
        // En içteki scope'tan dışarıya doğru dene:
        // ["SEED","COMMENTS"] için SEED_COMMENTS_URL → SEED_URL → URL
        for i in (0..=self.scopes.len()).rev() {
            let candidate = join_scopes(&self.scopes[..i], &normalized);
            if let Some(v) = self.lookup(&candidate) {
                return Some(v);
            }
        }
        None
    }

    fn lookup(&self, key: &str) -> Option<String> {
        self.providers.iter().find_map(|p| p.get(key))
    }

    /// Tüm değişkenleri listeler (ilk provider kazanır, anahtar normalize).
    pub fn list(&self) -> Vec<(String, String)> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for p in self.providers.iter() {
            for (k, v) in p.list() {
                let nk = normalize_key(&k);
                if seen.insert(nk.clone()) {
                    out.push((nk, v));
                }
            }
        }
        out
    }

    /// Anahtar secret ise `true` (log redaction vb. için; Vault provider'ları
    /// döndürecek).
    pub fn is_secret(&self, key: &str) -> bool {
        let normalized = normalize_key(key);
        self.providers.iter().any(|p| p.is_secret(&normalized))
    }

    /// `resolve_str` — `${KEY}`, `${KEY:-default}`, `${KEY:?}`, `{{KEY}}`
    /// placeholder'larını çözer. Detaylar: `template` modülü.
    pub fn resolve_str(&self, s: &str) -> Result<String, EnvError> {
        template::resolve(self, s)
    }
}

fn join_scopes(scopes: &[String], key: &str) -> String {
    if scopes.is_empty() {
        key.to_string()
    } else {
        format!("{}_{}", scopes.join("_"), key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_provider::StaticEnvProvider;

    #[test]
    fn test_normalize_key() {
        assert_eq!(normalize_key("db-url"), "DB_URL");
        assert_eq!(normalize_key("DB.URL"), "DB_URL");
        assert_eq!(normalize_key("  my key "), "MY_KEY");
    }

    #[test]
    fn test_layer_precedence_first_wins() {
        let mut low = std::collections::HashMap::new();
        low.insert("DB_URL".into(), "low".into());
        let mut high = std::collections::HashMap::new();
        high.insert("db_url".into(), "high".into());
        let env = Env::new(vec![
            Arc::new(StaticEnvProvider::new(high)),
            Arc::new(StaticEnvProvider::new(low)),
        ]);
        assert_eq!(env.get("DB_URL").as_deref(), Some("high"));
    }

    #[test]
    fn test_scoped_lookup_falls_back() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("SEED_USERS_URL".into(), "users-url".into());
        vars.insert("URL".into(), "bare-url".into());
        let env = Env::new(vec![Arc::new(StaticEnvProvider::new(vars))]);

        let scoped = env.scoped("seed_users");
        assert_eq!(scoped.get("URL").as_deref(), Some("users-url"));

        let mut vars2 = std::collections::HashMap::new();
        vars2.insert("URL".into(), "bare-url".into());
        let env2 = Env::new(vec![Arc::new(StaticEnvProvider::new(vars2))]);
        assert_eq!(env2.scoped("seed_users").get("URL").as_deref(), Some("bare-url"));
    }

    #[test]
    fn test_nested_scope_chain() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("SEED_URL".into(), "seed-level".into());
        vars.insert("SEED_COMMENTS_URL".into(), "comments-level".into());
        vars.insert("URL".into(), "bare".into());
        let env = Env::new(vec![Arc::new(StaticEnvProvider::new(vars))]);

        let comments = env.scoped("seed").scoped("comments");
        assert_eq!(comments.get("URL").as_deref(), Some("comments-level"));
        let seed_only = env.scoped("seed");
        assert_eq!(seed_only.get("URL").as_deref(), Some("seed-level"));
        assert_eq!(env.get("URL").as_deref(), Some("bare"));
    }

    #[test]
    fn test_list_merges_and_dedupes() {
        let mut a = std::collections::HashMap::new();
        a.insert("A".into(), "1".into());
        a.insert("B".into(), "2".into());
        let mut b = std::collections::HashMap::new();
        b.insert("b".into(), "overridden".into());
        b.insert("C".into(), "3".into());
        let env = Env::new(vec![
            Arc::new(StaticEnvProvider::new(a)),
            Arc::new(StaticEnvProvider::new(b)),
        ]);
        let list = env.list();
        assert_eq!(list.len(), 3);
        assert_eq!(env.get("B").as_deref(), Some("2"));
    }

    #[test]
    fn test_scoped_is_secret_propagates() {
        struct SecretProvider;
        impl EnvProvider for SecretProvider {
            fn get(&self, _key: &str) -> Option<String> {
                None
            }
            fn list(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn is_secret(&self, key: &str) -> bool {
                key.contains("TOKEN")
            }
        }
        let env = Env::new(vec![Arc::new(SecretProvider)]);
        assert!(env.scoped("x").is_secret("API_TOKEN"));
        assert!(!env.scoped("x").is_secret("API_URL"));
    }

    #[test]
    fn test_resolve_str_end_to_end() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOST".into(), "localhost".into());
        vars.insert("PORT".into(), "5432".into());
        let env = Env::new(vec![Arc::new(StaticEnvProvider::new(vars))]);
        assert_eq!(
            env.resolve_str("postgres://${HOST}:${PORT}/db").unwrap(),
            "postgres://localhost:5432/db"
        );
    }
}
