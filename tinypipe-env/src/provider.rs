//! `EnvProvider` — ortam kaynağı soyutlaması.
//!
//! Herhangi bir kaynak bir provider olabilir: OS env, `.env` dosyası,
//! gelecekte Vault/Consul/cloud secret manager. Tüketici yalnızca bu trait'i
//! görür — yeni kaynaklar `Env::new(vec![...])`'e eklenir, başka kod değişmez.

/// Ortam değişkeni kaynağı.
pub trait EnvProvider: Send + Sync {
    /// Anahtarı (normalize edilmiş, büyük harf) çözer.
    fn get(&self, key: &str) -> Option<String>;

    /// Kaynağın bildiği tüm değişkenler.
    fn list(&self) -> Vec<(String, String)>;

    /// Anahtar secret olarak işaretlenmişse `true` (log redaction vb. için).
    /// Vault-tarzı provider'lar burada `true` döndürür.
    fn is_secret(&self, _key: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_provider::StaticEnvProvider;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn test_provider_as_trait_object() {
        let mut vars = HashMap::new();
        vars.insert("K".into(), "v".into());
        let p: Arc<dyn EnvProvider> = Arc::new(StaticEnvProvider::new(vars));
        assert_eq!(p.get("K").as_deref(), Some("v"));
        assert!(!p.is_secret("K"));
    }
}
