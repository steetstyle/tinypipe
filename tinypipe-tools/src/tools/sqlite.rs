//! `sqlite.query` — SQLite sorgusu çalıştırır (blocking, bundled SQLite).
//!
//! kwargs: `db` (opsiyonel; dosya yolu veya `:memory:` — varsayılan `:memory:`),
//! `query` (zorunlu), `params` (opsiyonel array: int/float/bool/string/null).
//!
//! Sonuç:
//! - SELECT → satır array'i — her satır `{column: value}` object'i.
//! - DDL/DML (INSERT/UPDATE/DELETE/CREATE) → `{changes: N, last_insert_rowid: M}`.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;
use crate::tools::obj;

pub fn register(reg: &MockToolRegistry) {
    reg.add_with_schema("sqlite.query", input_schema(), output_schema(), false, sqlite_query);
}

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "db": { "type": "string", "description": "SQLite database path or :memory: (default)" },
            "query": { "type": "string", "description": "SQL statement (single statement)" },
            "params": { "type": "array", "description": "Query parameters (int/float/bool/string/null)" }
        },
        "required": ["query"]
    })
}

pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "SELECT → array of row objects; DDL/DML → {changes, last_insert_rowid}"
    })
}

/// `Value` → `rusqlite::ToSql` dinamik dönüşümü.
fn value_to_sql(v: &Value) -> Result<Box<dyn rusqlite::ToSql>, String> {
    use rusqlite::types::ToSql;
    match v {
        Value::Int(i) => Ok(Box::new(*i) as Box<dyn ToSql>),
        Value::Float(f) => Ok(Box::new(*f) as Box<dyn ToSql>),
        Value::Bool(b) => Ok(Box::new(*b) as Box<dyn ToSql>),
        Value::String(s) => Ok(Box::new(s.clone()) as Box<dyn ToSql>),
        Value::Null => Ok(Box::new(None::<i64>) as Box<dyn ToSql>),
        other => Err(format!(
            "sqlite.query: unsupported param value type: {:?}",
            other
        )),
    }
}

/// Satır değerini `Value`'ya çevirir (NULL → Null).
fn row_value(row: &rusqlite::Row, i: usize) -> Value {
    if let Ok(Some(v)) = row.get::<_, Option<i64>>(i) {
        return Value::Int(v);
    }
    if let Ok(Some(v)) = row.get::<_, Option<f64>>(i) {
        return Value::Float(v);
    }
    if let Ok(Some(v)) = row.get::<_, Option<String>>(i) {
        return Value::String(v);
    }
    if let Ok(Some(v)) = row.get::<_, Option<Vec<u8>>>(i) {
        return Value::String(format!(
            "0x{}",
            v.iter().map(|b| format!("{:02x}", b)).collect::<String>()
        ));
    }
    Value::Null
}

/// SQL ifadesini çalıştırır. SELECT → satır array'i; DDL/DML → {changes, last_insert_rowid}.
pub fn sqlite_query(args: &[Value], kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let get_str = |k: &str| kwargs.get(k).and_then(|v| v.as_str()).map(String::from);
    let db_path = get_str("db").unwrap_or_else(|| ":memory:".to_string());
    let query = get_str("query")
        .or_else(|| args.first().and_then(|v| v.as_str()).map(String::from))
        .ok_or_else(|| "sqlite.query: missing 'query' (string)".to_string())?;

    let conn = if db_path == ":memory:" || db_path.is_empty() {
        rusqlite::Connection::open_in_memory()
    } else {
        rusqlite::Connection::open(&db_path)
    }
    .map_err(|e| format!("sqlite.query: open '{}' failed: {}", db_path, e))?;

    let params: Vec<Box<dyn rusqlite::ToSql>> = match kwargs.get("params") {
        Some(Value::Array(items)) => items
            .iter()
            .map(value_to_sql)
            .collect::<Result<Vec<_>, String>>()?,
        _ => Vec::new(),
    };

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("sqlite.query: prepare failed: {}", e))?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    if stmt.column_count() == 0 {
        // DDL/DML: satır yok → değişen satır sayısı + son insert id
        let changes = stmt
            .execute(param_refs.as_slice())
            .map_err(|e| format!("sqlite.query: execute failed: {}", e))?;
        let mut record = HashMap::new();
        record.insert("changes".into(), Value::Int(changes as i64));
        record.insert(
            "last_insert_rowid".into(),
            Value::Int(conn.last_insert_rowid()),
        );
        return Ok(obj(record));
    }

    let columns: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let mut rows = stmt
        .query(param_refs.as_slice())
        .map_err(|e| format!("sqlite.query: query failed: {}", e))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("sqlite.query: row read failed: {}", e))?
    {
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
    fn test_sqlite_missing_query() {
        let err = sqlite_query(&[], &HashMap::new(), &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("query"));
    }

    #[test]
    fn test_sqlite_memory_roundtrip() {
        // `:memory:` her çağrıda yeni bağlantı açar — state paylaşımı için dosya DB
        let db = std::env::temp_dir().join(format!("tinypipe_sqlite_test_{}.db", std::process::id()));
        let mut kwargs = HashMap::new();
        kwargs.insert("db".into(), Value::String(db.to_string_lossy().into()));
        kwargs.insert(
            "query".into(),
            Value::String("CREATE TABLE t (id INTEGER, name TEXT)".into()),
        );
        let v = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        assert!(matches!(v, Value::Object(_)));

        kwargs.insert(
            "query".into(),
            Value::String("INSERT INTO t VALUES (1, 'a'), (2, 'b')".into()),
        );
        if let Value::Object(m) = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap() {
            assert_eq!(m.get("changes"), Some(&Value::Int(2)));
        } else {
            panic!("expected changes object");
        }

        kwargs.insert("query".into(), Value::String("SELECT * FROM t ORDER BY id".into()));
        if let Value::Array(rows) = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap() {
            assert_eq!(rows.len(), 2);
            if let Value::Object(first) = &rows[0] {
                assert_eq!(first.get("id"), Some(&Value::Int(1)));
                assert_eq!(first.get("name"), Some(&Value::String("a".into())));
            } else {
                panic!("expected row object");
            }
        } else {
            panic!("expected rows array");
        }
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_sqlite_params() {
        let db = std::env::temp_dir().join(format!("tinypipe_sqlite_params_{}.db", std::process::id()));
        let mut kwargs = HashMap::new();
        kwargs.insert("db".into(), Value::String(db.to_string_lossy().into()));
        kwargs.insert(
            "query".into(),
            Value::String("CREATE TABLE t (id INTEGER, name TEXT)".into()),
        );
        let _ = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        kwargs.insert(
            "query".into(),
            Value::String("INSERT INTO t VALUES (?, ?)".into()),
        );
        kwargs.insert(
            "params".into(),
            Value::Array(vec![Value::Int(7), Value::String("x".into())]),
        );
        let _ = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap();
        kwargs.remove("params");
        kwargs.insert(
            "query".into(),
            Value::String("SELECT name FROM t WHERE id = ?".into()),
        );
        kwargs.insert(
            "params".into(),
            Value::Array(vec![Value::Int(7)]),
        );
        if let Value::Array(rows) = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap() {
            assert_eq!(rows.len(), 1);
        } else {
            panic!("expected rows array");
        }
    }

    #[test]
    fn test_sqlite_bad_path_reports_error() {
        let mut kwargs = HashMap::new();
        kwargs.insert("db".into(), Value::String("/nonexistent_dir/x/db.sqlite".into()));
        kwargs.insert("query".into(), Value::String("SELECT 1".into()));
        let err = sqlite_query(&[], &kwargs, &tinypipe_env::Env::empty()).unwrap_err();
        assert!(err.contains("failed"));
    }
}
