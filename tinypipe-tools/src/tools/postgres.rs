//! `postgres` — PostgreSQL sorgusu çalıştırır (blocking, NoTLS).
//!
//! kwargs: `connection_string` (zorunlu; `dsn`/`conn` alias), `query` (zorunlu),
//! `params` (opsiyonel array: int/float/bool/string/null).
//! Sonuç: satır array'i — her satır `{column: value}` object'i.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;
use crate::tools::obj;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema("postgres", input_schema(), output_schema(), false, postgres);
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "connection_string": { "type": "string", "description": "PostgreSQL connection string (postgres://user:pass@host:port/db)" },
            "query": { "type": "string", "description": "SQL query" },
            "params": { "type": "array", "description": "Query parameters (int/float/bool/string/null)" }
        },
        "required": ["connection_string", "query"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "Rows as array of objects (column name → value)"
    })
}

/// `Value` → `postgres::ToSql` dinamik dönüşümü.
fn value_to_sql(v: &Value) -> Result<Box<dyn postgres::types::ToSql + Sync>, String> {
    use postgres::types::ToSql;
    match v {
        Value::Int(i) => Ok(Box::new(*i) as Box<dyn ToSql + Sync>),
        Value::Float(f) => Ok(Box::new(*f) as Box<dyn ToSql + Sync>),
        Value::Bool(b) => Ok(Box::new(*b) as Box<dyn ToSql + Sync>),
        Value::String(s) => Ok(Box::new(s.clone()) as Box<dyn ToSql + Sync>),
        Value::Null => Ok(Box::new(None::<i64>) as Box<dyn ToSql + Sync>),
        other => Err(format!(
            "postgres: unsupported param value type: {:?}",
            other
        )),
    }
}

/// Satır değerini `Value`'ya çevirir (NULL → Null).
fn row_value(row: &postgres::Row, i: usize) -> Value {
    if let Ok(Some(v)) = row.try_get::<_, Option<i64>>(i) {
        return Value::Int(v);
    }
    if let Ok(Some(v)) = row.try_get::<_, Option<f64>>(i) {
        return Value::Float(v);
    }
    if let Ok(Some(v)) = row.try_get::<_, Option<bool>>(i) {
        return Value::Bool(v);
    }
    if let Ok(Some(v)) = row.try_get::<_, Option<String>>(i) {
        return Value::String(v);
    }
    if let Ok(Some(v)) = row.try_get::<_, Option<Vec<u8>>>(i) {
        return Value::String(format!(
            "0x{}",
            v.iter().map(|b| format!("{:02x}", b)).collect::<String>()
        ));
    }
    if row.try_get::<_, Option<String>>(i).is_ok() {
        return Value::Null;
    }
    Value::String(format!("<{}>", row.columns()[i].type_()))
}

/// SQL sorgusu çalıştırır. Sonuç: satır array'i.
pub fn postgres(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let get_str = |k: &str| kwargs.get(k).and_then(|v| v.as_str()).map(String::from);
    let conn_str = ["connection_string", "dsn", "conn"]
        .iter()
        .find_map(|k| get_str(k))
        .or_else(|| args.first().and_then(|v| v.as_str()).map(String::from))
        .ok_or_else(|| "postgres: missing 'connection_string' (string)".to_string())?;
    let query = get_str("query").ok_or_else(|| "postgres: missing 'query' (string)".to_string())?;

    let params: Vec<Box<dyn postgres::types::ToSql + Sync>> = match kwargs.get("params") {
        Some(Value::Array(items)) => items
            .iter()
            .map(value_to_sql)
            .collect::<Result<Vec<_>, String>>()?,
        _ => Vec::new(),
    };

    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .map_err(|e| format!("postgres: connect failed: {}", e))?;
    let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
        params.iter().map(|b| b.as_ref()).collect();
    let rows = client
        .query(&query, &param_refs)
        .map_err(|e| format!("postgres: query failed: {}", e))?;

    let columns: Vec<String> = rows
        .first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut record = HashMap::new();
        for (i, col) in columns.iter().enumerate() {
            record.insert(col.clone(), row_value(row, i));
        }
        out.push(obj(record));
    }
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_postgres_missing_args() {
        let err = postgres(&[], &HashMap::new(), &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("connection_string"));

        let mut kwargs = HashMap::new();
        kwargs.insert(
            "connection_string".into(),
            Value::String("postgres://u:p@localhost:5432/db".into()),
        );
        let err = postgres(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("query"));
    }

    #[test]
    fn test_postgres_connect_failure_reports_error() {
        let mut kwargs = HashMap::new();
        kwargs.insert(
            "connection_string".into(),
            Value::String("postgres://invalid@127.0.0.1:1/nope".into()),
        );
        kwargs.insert("query".into(), Value::String("SELECT 1".into()));
        let err = postgres(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("connect failed") || err.contains("failed"));
    }

    #[test]
    fn test_value_to_sql_types() {
        assert!(value_to_sql(&Value::Int(5)).is_ok());
        assert!(value_to_sql(&Value::Float(1.5)).is_ok());
        assert!(value_to_sql(&Value::Bool(true)).is_ok());
        assert!(value_to_sql(&Value::String("s".into())).is_ok());
        assert!(value_to_sql(&Value::Null).is_ok());
        let unsupported = value_to_sql(&Value::Object(HashMap::new()));
        assert!(unsupported.is_err());
        let _ = value_to_sql(&Value::Int(7)).unwrap();
    }
}
