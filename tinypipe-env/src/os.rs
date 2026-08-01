//! OS process environment provider (`std::env`).

use crate::provider::EnvProvider;

/// `std::env::vars()`'ı okuyan provider.
pub struct OsEnvProvider;

impl EnvProvider for OsEnvProvider {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn list(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_os_provider_sees_process_env() {
        std::env::set_var("TINYPIPE_ENV_TEST_VAR", "yes");
        let p = OsEnvProvider;
        assert_eq!(p.get("TINYPIPE_ENV_TEST_VAR").as_deref(), Some("yes"));
        assert!(p.list().iter().any(|(k, v)| {
            k == "TINYPIPE_ENV_TEST_VAR" && v == "yes"
        }));
        std::env::remove_var("TINYPIPE_ENV_TEST_VAR");
    }
}
