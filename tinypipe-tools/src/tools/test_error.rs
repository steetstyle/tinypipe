//! `test.error` — her zaman hata döndürür (hata yolu testi).

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("test.error", test_error);
}

fn test_error(_args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    Err("simulated tool error".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_error_tool() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let ct = CallTarget::new("test.error");
        let result = reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty());
        assert!(result.is_err());
    }
}
