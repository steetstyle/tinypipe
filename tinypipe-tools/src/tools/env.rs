//! `env.*` tool'ları — grafik içinden ortam değişkeni okuma.
//!
//! - `env.get(key, default?)` — değeri string olarak döndürür; yoksa `default`
//!   (verilmemişse hata).
//! - `env.list` — scope'a görünür tüm değişkenleri JSON object olarak döndürür.
//! - `env.template(value)` — `$ {KEY}` / `$ {KEY:-def}` / `{{KEY}}` çözer.
//!
//! Env kaynağı (OS, `.env` dosyası, CLI override'ları, ileride Vault) executor'ın
//! ortam görünümünden gelir; subgraph çağrılarında modül adıyla scope'lanır.

use std::collections::HashMap;

use tinypipe_api::types::Value;
use tinypipe_env::Env;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("env.get", env_get);
    reg.add("env.list", env_list);
    reg.add("env.template", env_template);
}

fn env_get(args: &[Value], kwargs: &HashMap<String, Value>, env: &Env) -> Result<Value, String> {
    let key = kwargs
        .get("key")
        .or_else(|| args.first())
        .and_then(Value::as_str)
        .ok_or_else(|| "env.get requires 'key' (kwarg or positional)".to_string())?;
    match env.get(key) {
        Some(v) => Ok(Value::String(v)),
        None => kwargs
            .get("default")
            .cloned()
            .ok_or_else(|| format!("env var '{key}' not found")),
    }
}

fn env_list(_args: &[Value], _kwargs: &HashMap<String, Value>, env: &Env) -> Result<Value, String> {
    let map: HashMap<String, Value> = env
        .list()
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    Ok(Value::Object(map))
}

fn env_template(
    args: &[Value],
    kwargs: &HashMap<String, Value>,
    env: &Env,
) -> Result<Value, String> {
    let value = kwargs
        .get("value")
        .or_else(|| args.first())
        .and_then(Value::as_str)
        .ok_or_else(|| "env.template requires 'value'".to_string())?;
    env.resolve_str(value)
        .map(Value::String)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;
    use tinypipe_env::static_provider::StaticEnvProvider;

    fn registry_with(vars: &[(&str, &str)]) -> (MockToolRegistry, Env) {
        let reg = MockToolRegistry::new();
        register(&reg);
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let env = Env::new(vec![std::sync::Arc::new(StaticEnvProvider::new(map))]);
        (reg, env)
    }

    #[test]
    fn test_env_get_kwarg_and_default() {
        let (reg, env) = registry_with(&[("DB_URL", "postgres://x")]);
        let mut ct = CallTarget::new("env.get");
        ct.kwargs.insert("key".into(), Value::String("DB_URL".into()));
        let v = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &env).unwrap();
        assert_eq!(v, Value::String("postgres://x".into()));

        let mut ct2 = CallTarget::new("env.get");
        ct2.kwargs.insert("key".into(), Value::String("MISSING".into()));
        ct2.kwargs.insert("default".into(), Value::String("fallback".into()));
        let v = reg.dispatch(&ct2, &tinypipe_api::types::Context::new(), &env).unwrap();
        assert_eq!(v, Value::String("fallback".into()));
    }

    #[test]
    fn test_env_get_missing_without_default_errors() {
        let (reg, env) = registry_with(&[]);
        let mut ct = CallTarget::new("env.get");
        ct.kwargs.insert("key".into(), Value::String("NOPE".into()));
        let err = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &env).unwrap_err();
        assert!(err.to_string().contains("NOPE"));
    }

    #[test]
    fn test_env_template_tool() {
        let (reg, env) = registry_with(&[("HOST", "db.example.com")]);
        let mut ct = CallTarget::new("env.template");
        ct.kwargs.insert("value".into(), Value::String("postgres://${HOST}:5432".into()));
        let v = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &env).unwrap();
        assert_eq!(v, Value::String("postgres://db.example.com:5432".into()));
    }

    #[test]
    fn test_env_list_tool() {
        let (reg, env) = registry_with(&[("A", "1")]);
        let ct = CallTarget::new("env.list");
        let v = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &env).unwrap();
        let Value::Object(map) = v else { panic!("expected object") };
        assert_eq!(map.get("A"), Some(&Value::String("1".into())));
    }

    #[test]
    fn test_scoped_env_get() {
        let (reg, base) = registry_with(&[("SEED_USERS_URL", "scoped-url")]);
        let env = base.scoped("seed_users");
        let mut ct = CallTarget::new("env.get");
        ct.kwargs.insert("key".into(), Value::String("URL".into()));
        let v = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &env).unwrap();
        assert_eq!(v, Value::String("scoped-url".into()));
    }
}
