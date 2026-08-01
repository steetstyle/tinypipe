//! `dict.get` — object'ten key'e göre değer döndürür (opsiyonel default).
//!
//! kwargs: `obj` (zorunlu, object), `key` (zorunlu, string|int),
//! `default` (opsiyonel). Sonuç: değer; key yoksa `default`; default yoksa null.
//!
//! Subscript'lerin aksine bu tool lenient'tir: eksik anahtar hata değil,
//! default/null döner — opsiyonel alan okumaları için tasarlanmıştır.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema("dict.get", input_schema(), output_schema(), false, dict_get);
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "obj": { "type": "object", "description": "Object to read from" },
            "key": { "type": ["string", "integer"], "description": "Key to look up" },
            "default": { "description": "Value returned when key is missing (default: null)" }
        },
        "required": ["obj", "key"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": ["object", "array", "integer", "number", "string", "boolean", "null"] })
}

pub fn dict_get(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let obj = kwargs
        .get("obj")
        .or_else(|| args.first())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .ok_or_else(|| "dict.get: missing 'obj' (object)".to_string())?;
    let key = kwargs
        .get("key")
        .or_else(|| args.get(1))
        .ok_or_else(|| "dict.get: missing 'key' (string|int)".to_string())?;
    let key_str = match key {
        Value::String(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        _ => return Err("dict.get: 'key' must be a string or integer".to_string()),
    };
    let default = kwargs.get("default").cloned();
    Ok(obj
        .get(&key_str)
        .cloned()
        .or(default)
        .unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    #[test]
    fn test_dict_get_missing_key_returns_default() {
        let mut kwargs = HashMap::new();
        kwargs.insert("obj".into(), obj(&[("a", Value::Int(1))]));
        kwargs.insert("key".into(), Value::String("b".into()));
        kwargs.insert("default".into(), Value::Int(0));
        let v = dict_get(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(v, Value::Int(0));
    }

    #[test]
    fn test_dict_get_missing_key_no_default_returns_null() {
        let mut kwargs = HashMap::new();
        kwargs.insert("obj".into(), obj(&[]));
        kwargs.insert("key".into(), Value::String("x".into()));
        let v = dict_get(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn test_dict_get_int_key() {
        let mut kwargs = HashMap::new();
        kwargs.insert("obj".into(), obj(&[("5", Value::String("five".into()))]));
        kwargs.insert("key".into(), Value::Int(5));
        let v = dict_get(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(v, Value::String("five".into()));
    }

    #[test]
    fn test_dict_get_missing_obj_errors() {
        let mut kwargs = HashMap::new();
        kwargs.insert("key".into(), Value::String("x".into()));
        let err = dict_get(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("obj"));
    }
}
