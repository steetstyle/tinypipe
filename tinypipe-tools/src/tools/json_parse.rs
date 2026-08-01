//! `json.parse` — JSON string'ini yapısal `Value`'ya çevirir.
//!
//! kwargs: `json` (zorunlu, string). Sonuç: Array/Object/Int/String/... Value.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;
use crate::tools::obj;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema(
        "json.parse",
        input_schema(),
        output_schema(),
        true,
        json_parse,
    );
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "json": { "type": "string", "description": "JSON document as string" }
        },
        "required": ["json"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": ["object", "array", "integer", "number", "string", "boolean", "null"] })
}

pub fn json_parse(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let get_str = |k: &str| kwargs.get(k).and_then(|v| v.as_str()).map(String::from);
    let s = get_str("json")
        .or_else(|| args.first().and_then(|v| v.as_str()).map(String::from))
        .ok_or_else(|| "json.parse: missing 'json' (string)".to_string())?;
    let parsed: serde_json::Value =
        serde_json::from_str(&s).map_err(|e| format!("json.parse: invalid JSON: {e}"))?;
    Ok(to_value(parsed))
}

fn to_value(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(items) => Value::Array(items.into_iter().map(to_value).collect()),
        serde_json::Value::Object(m) => {
            let map: HashMap<String, Value> = m.into_iter().map(|(k, v)| (k, to_value(v))).collect();
            obj(map)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::str_v;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_json_parse_object() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("json.parse");
        ct.kwargs
            .insert("json".into(), Value::String(r#"{"a": 1, "b": "x"}"#.into()));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        match result {
            Value::Object(m) => {
                assert_eq!(m.get("a"), Some(&Value::Int(1)));
                assert_eq!(m.get("b"), Some(&str_v("x")));
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn test_json_parse_array() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("json.parse");
        ct.kwargs
            .insert("json".into(), Value::String(r#"[1, 2, 3]"#.into()));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(
            result,
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
    }

    #[test]
    fn test_json_parse_invalid() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("json.parse");
        ct.kwargs.insert("json".into(), Value::String("not json".into()));
        assert!(reg.dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty()).is_err());
    }
}
