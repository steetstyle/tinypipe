//! In-memory static provider (CLI `--env K=V` override'ları, test'ler).

use std::collections::HashMap;

use crate::provider::EnvProvider;
use crate::normalize_key;

/// Sabit anahtar→değer haritası (anahtarlar kurulumda normalize edilir).
#[derive(Debug, Clone, Default)]
pub struct StaticEnvProvider {
    vars: HashMap<String, String>,
}

impl StaticEnvProvider {
    pub fn new(vars: HashMap<String, String>) -> Self {
        StaticEnvProvider {
            vars: vars
                .into_iter()
                .map(|(k, v)| (normalize_key(&k), v))
                .collect(),
        }
    }
}

impl EnvProvider for StaticEnvProvider {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(&normalize_key(key)).cloned()
    }

    fn list(&self) -> Vec<(String, String)> {
        self.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}
