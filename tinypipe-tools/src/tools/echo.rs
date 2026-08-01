//! `echo` — ilk argümanı olduğu gibi döndürür.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("echo", echo);
}

fn echo(args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    args.first()
        .cloned()
        .ok_or_else(|| "echo requires 1 arg".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_echo_passthrough() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("echo");
        ct.args.push(Value::String("passthrough".into()));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::String("passthrough".into()));
    }
}
