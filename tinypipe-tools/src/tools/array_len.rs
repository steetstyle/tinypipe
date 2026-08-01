//! `array.len` — dizinin uzunluğunu döndürür.
//!
//! kwargs: `array` (zorunlu, array). Sonuç: Int.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema(
        "array.len",
        input_schema(),
        output_schema(),
        true,
        array_len,
    );
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "array": { "type": "array", "description": "Array to measure" }
        },
        "required": ["array"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": "integer" })
}

pub fn array_len(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let arr = kwargs
        .get("array")
        .or_else(|| args.first())
        .and_then(Value::as_array)
        .ok_or_else(|| "array.len: missing 'array' (array)".to_string())?;
    Ok(Value::Int(arr.len() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_array_len() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("array.len");
        ct.kwargs.insert(
            "array".into(),
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn test_array_len_empty() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("array.len");
        ct.kwargs.insert("array".into(), Value::Array(vec![]));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::Int(0));
    }
}
