//! `list.get` — diziden index'e göre eleman döndürür.
//!
//! kwargs: `array` (zorunlu, array), `index` (zorunlu, int). Sonuç: eleman.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema(
        "list.get",
        input_schema(),
        output_schema(),
        true,
        list_get,
    );
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "array": { "type": "array", "description": "Array to index" },
            "index": { "type": "integer", "description": "Zero-based index" }
        },
        "required": ["array", "index"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": ["object", "array", "integer", "number", "string", "boolean", "null"] })
}

pub fn list_get(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let arr = kwargs
        .get("array")
        .or_else(|| args.first())
        .and_then(Value::as_array)
        .ok_or_else(|| "list.get: missing 'array' (array)".to_string())?;
    let index = kwargs
        .get("index")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .ok_or_else(|| "list.get: missing 'index' (int)".to_string())?;
    arr.get(index)
        .cloned()
        .ok_or_else(|| format!("list.get: index {index} out of bounds (len {})", arr.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_list_get() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("list.get");
        ct.kwargs.insert(
            "array".into(),
            Value::Array(vec![Value::Int(10), Value::Int(20), Value::Int(30)]),
        );
        ct.kwargs.insert("index".into(), Value::Int(1));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn test_list_get_out_of_bounds() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("list.get");
        ct.kwargs.insert("array".into(), Value::Array(vec![Value::Int(1)]));
        ct.kwargs.insert("index".into(), Value::Int(5));
        assert!(reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty()).is_err());
    }
}
