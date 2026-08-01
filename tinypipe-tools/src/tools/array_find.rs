//! `array.find` — `item[key] == value` olan ilk elemanı döndürür, yoksa null.
//!
//! kwargs: `array` (zorunlu, array of object), `key` (zorunlu, string),
//! `value` (zorunlu). Sonuç: ilk eşleşen item ya da null.
//!
//! Subscript'lerin aksine lenient'tir: eşleşme yoksa hata değil null döner —
//! sepetten ürün bulma gibi "olmayabilir" aramalar için tasarlanmıştır.
//! Karşılaştırma cross-type'tır (int 100 == float 100.0).

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema("array.find", input_schema(), output_schema(), false, array_find);
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "array": { "type": "array", "description": "Array of objects to search" },
            "key": { "type": "string", "description": "Field name to compare" },
            "value": { "description": "Value to match (string/int/float/bool/null)" }
        },
        "required": ["array", "key", "value"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({ "type": ["object", "array", "integer", "number", "string", "boolean", "null"] })
}

/// Cross-type eşitlik (values_equal semantiğiyle aynı): int/float karşılaştırılabilir.
fn eq_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => (x - y).abs() < f64::EPSILON,
        (Value::Int(x), Value::Float(y)) => (*x as f64 - y).abs() < f64::EPSILON,
        (Value::Float(x), Value::Int(y)) => (x - *y as f64).abs() < f64::EPSILON,
        (Value::String(x), Value::String(y)) => x == y,
        _ => false,
    }
}

pub fn array_find(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let arr = kwargs
        .get("array")
        .or_else(|| args.first())
        .and_then(Value::as_array)
        .ok_or_else(|| "array.find: missing 'array' (array)".to_string())?;
    let key = kwargs
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| "array.find: missing 'key' (string)".to_string())?;
    let value = kwargs
        .get("value")
        .cloned()
        .ok_or_else(|| "array.find: missing 'value'".to_string())?;
    for item in arr {
        if let Value::Object(m) = item {
            if let Some(v) = m.get(key) {
                if eq_value(v, &value) {
                    return Ok(item.clone());
                }
            }
        }
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cart() -> Value {
        let mut o1 = HashMap::new();
        o1.insert("product_id".into(), Value::Int(101));
        o1.insert("name".into(), Value::String("elma".into()));
        let mut o2 = HashMap::new();
        o2.insert("product_id".into(), Value::Int(202));
        o2.insert("name".into(), Value::String("armut".into()));
        Value::Array(vec![Value::Object(o1), Value::Object(o2)])
    }

    #[test]
    fn test_array_find_hit() {
        let mut kwargs = HashMap::new();
        kwargs.insert("array".into(), cart());
        kwargs.insert("key".into(), Value::String("product_id".into()));
        kwargs.insert("value".into(), Value::Int(202));
        let v = array_find(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        let Value::Object(m) = v else {
            panic!("expected object");
        };
        assert_eq!(m.get("name"), Some(&Value::String("armut".into())));
    }

    #[test]
    fn test_array_find_miss_returns_null() {
        let mut kwargs = HashMap::new();
        kwargs.insert("array".into(), cart());
        kwargs.insert("key".into(), Value::String("product_id".into()));
        kwargs.insert("value".into(), Value::Int(999));
        let v = array_find(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn test_array_find_cross_type_number() {
        let mut kwargs = HashMap::new();
        kwargs.insert("array".into(), cart());
        kwargs.insert("key".into(), Value::String("product_id".into()));
        kwargs.insert("value".into(), Value::Float(101.0));
        let v = array_find(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        let Value::Object(m) = v else {
            panic!("expected object");
        };
        assert_eq!(m.get("name"), Some(&Value::String("elma".into())));
    }

    #[test]
    fn test_array_find_missing_args() {
        let err = array_find(&[], &HashMap::new(), &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("array"));
    }
}
