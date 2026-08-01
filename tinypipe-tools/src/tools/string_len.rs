//! `string.len` — string uzunluğunu döndürür.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("string.len", string_len);
}

fn string_len(args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let s = args[0].as_str().ok_or("arg0 not a string")?;
    Ok(Value::Int(s.len() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_string_len() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("string.len");
        ct.args.push(Value::String("hello".into()));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::Int(5));
    }
}
