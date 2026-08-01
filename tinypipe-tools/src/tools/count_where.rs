//! `array.count_where` — object elemanlarında `elem[key] == value` sayar.
//!
//! kwargs: `array` (zorunlu, array of object), `key` (zorunlu, string),
//! `value` (zorunlu). Sonuç: Int.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema(
        "array.count_where",
        input_schema(),
        output_schema(),
        true,
        count_where,
    );
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "array": { "type": "array", "description": "Array of objects to inspect" },
            "key": { "type": "string", "description": "Field name to compare" },
            "value": { "description": "Value to match" }
        },
        "required": ["array", "key", "value"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": "integer" })
}

pub fn count_where(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let arr = kwargs
        .get("array")
        .or_else(|| args.first())
        .and_then(Value::as_array)
        .ok_or_else(|| "array.count_where: missing 'array' (array)".to_string())?;
    let key = kwargs
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| "array.count_where: missing 'key' (string)".to_string())?;
    let want = kwargs
        .get("value")
        .cloned()
        .ok_or_else(|| "array.count_where: missing 'value'".to_string())?;
    let count = arr
        .iter()
        .filter(|item| match item {
            Value::Object(m) => m.get(key) == Some(&want),
            _ => false,
        })
        .count();
    Ok(Value::Int(count as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;
    use tinypipe_api::types::Context;

    fn item(completed: bool) -> Value {
        let mut m = HashMap::new();
        m.insert("completed".into(), Value::Bool(completed));
        Value::Object(m)
    }

    #[test]
    fn test_count_where() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("array.count_where");
        ct.kwargs.insert(
            "array".into(),
            Value::Array(vec![
                item(true),
                item(false),
                item(true),
                Value::Int(42),
            ]),
        );
        ct.kwargs.insert("key".into(), Value::String("completed".into()));
        ct.kwargs.insert("value".into(), Value::Bool(true));
        let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn test_count_where_no_match() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("array.count_where");
        ct.kwargs.insert("array".into(), Value::Array(vec![item(false)]));
        ct.kwargs.insert("key".into(), Value::String("completed".into()));
        ct.kwargs.insert("value".into(), Value::Bool(true));
        let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(result, Value::Int(0));
    }
}
