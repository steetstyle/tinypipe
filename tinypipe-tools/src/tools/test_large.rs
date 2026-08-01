//! `test.large` — N uzunluğunda string döndürür (bellek/context limit testi).

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("test.large", test_large);
}

fn test_large(args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let n = args[0].as_u64().ok_or("arg0 not a u64")? as usize;
    Ok(Value::String("x".repeat(n)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_large_tool_memory() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("test.large");
        ct.args.push(Value::Int(100));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        if let Value::String(s) = result {
            assert_eq!(s.len(), 100);
        } else {
            panic!("expected string");
        }
    }
}
