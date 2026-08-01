//! `ToolRegistry` trait'i — CALL dispatch için soyut arayüz.
//!
//! TinyOS tarafından implemente edilir (`TinyOsToolRegistry`),
//! tinypipe-vm tarafından tüketilir.
//! Test'lerde `MockToolRegistry` kullanılır.
//!
//! Env: her dispatch'e çözülmüş `Env` görünümü iletilir (executor'ın ortamı —
//! subgraph çağrılarında çocuğa scope'lu görünüm verilir). Bu sayede
//! tool'lar OS env, `.env` dosyası, ileride Vault vb. kaynaklardan
//! soyut bir arayüzle okur.

use crate::types::{CallTarget, Context, DispatchError, RegistryError, ToolSpec, Value};
use tinypipe_env::Env;

/// `execute_subgraph` sonucu: çocuk grafiğin context'i (çağıran ctx'e merge
/// edilir) ve çocuk grafiğin `return` değeri (call ifadesinin değeri olur).
#[derive(Debug, Clone)]
pub struct SubgraphResult {
    pub context: Context,
    pub output: Value,
}

/// Tool dispatch ve schema sorgulama için ana trait.
pub trait ToolRegistry: Send + Sync {
    /// Bir tool'un metadata'sını (schema, version, pure flag) döndürür.
    fn resolve(&self, name: &str, version: &str) -> Result<ToolSpec, RegistryError>;

    /// Bir CALL action'ını dispatch eder, sonucu `Value` olarak döndürür.
    /// `env`: çağrı anındaki ortam görünümü (modül scope'lu olabilir).
    fn dispatch(&self, call: &CallTarget, context: &Context, env: &Env)
        -> Result<Value, DispatchError>;

    /// Execute a subgraph by name with given input context.
    /// `env`: çağıranın ortamı — çocuk, scope'lu görünümle çalıştırılır.
    fn execute_subgraph(
        &self,
        name: &str,
        input: Context,
        env: &Env,
    ) -> Result<SubgraphResult, DispatchError>;

    /// Tool'un güncel schema hash'ini döndürür (runtime schema drift detection için).
    fn latest_schema_hash(&self, name: &str) -> Result<String, RegistryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    struct NoopRegistry;

    impl ToolRegistry for NoopRegistry {
        fn resolve(&self, name: &str, _version: &str) -> Result<ToolSpec, RegistryError> {
            Err(RegistryError::NotFound(name.into()))
        }

        fn dispatch(
            &self,
            _call: &CallTarget,
            _context: &Context,
            _env: &Env,
        ) -> Result<Value, DispatchError> {
            Err(DispatchError::Internal("noop".into()))
        }

        fn execute_subgraph(
            &self,
            _name: &str,
            _input: Context,
            _env: &Env,
        ) -> Result<SubgraphResult, DispatchError> {
            Err(DispatchError::Internal("noop".into()))
        }

        fn latest_schema_hash(&self, _name: &str) -> Result<String, RegistryError> {
            Err(RegistryError::NotFound("noop".into()))
        }
    }

    #[test]
    fn test_noop_registry() {
        let reg = NoopRegistry;
        let env = Env::empty();
        assert!(reg.resolve("foo", "1.0").is_err());
        assert!(reg
            .dispatch(&CallTarget::new("foo"), &Context::new(), &env)
            .is_err());
        assert!(reg.execute_subgraph("foo", Context::new(), &env).is_err());
        assert!(reg.latest_schema_hash("foo").is_err());
    }
}
