//! Gömülü tool'lar — her tool kendi dosyasında.
//!
//! - `mock_tools()`: temel/hafif tool seti (test'ler ve diğer paketler için)
//! - `default_tools()`: tam set — temel + `http_request` + `postgres` + JSON/array yardımcıları
//!
//! Ağır bağımlılıklar (ureq, postgres) yalnızca ilgili tool dosyasında kullanılır.

pub mod array_find;
pub mod array_len;
pub mod count_where;
pub mod dict_get;
pub mod echo;
pub mod env;
pub mod http_request;
pub mod json_parse;
pub mod list_get;
pub mod math_add;
pub mod math_mul;
pub mod postgres;
pub mod sqlite;
pub mod string_len;
pub mod test_error;
pub mod test_large;
pub mod test_sleep;

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub(crate) fn obj(m: HashMap<String, Value>) -> Value {
    Value::Object(m)
}

pub(crate) fn str_v(s: &str) -> Value {
    Value::String(s.into())
}

/// Temel mock tool'ları içeren bir `MockToolRegistry` döndürür.
pub fn mock_tools() -> MockToolRegistry {
    let reg = MockToolRegistry::new();
    math_add::register(&reg);
    math_mul::register(&reg);
    string_len::register(&reg);
    echo::register(&reg);
    env::register(&reg);
    test_sleep::register(&reg);
    test_error::register(&reg);
    test_large::register(&reg);
    reg
}

/// Tam registry: temel tool'lar + gerçek `http_request` + `postgres`
/// + JSON/array yardımcıları (`json.parse`, `array.len`, `list.get`,
/// `array.count_where`, `array.find`, `dict.get`).
pub fn default_tools() -> MockToolRegistry {
    let reg = mock_tools();
    http_request::register(&reg);
    postgres::register(&reg);
    sqlite::register(&reg);
    json_parse::register(&reg);
    array_len::register(&reg);
    list_get::register(&reg);
    count_where::register(&reg);
    array_find::register(&reg);
    dict_get::register(&reg);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_tools_factory() {
        let reg = mock_tools();
        assert_eq!(reg.tool_count(), 10); // 7 + env.get/env.list/env.template
    }

    #[test]
    fn test_default_tools_factory() {
        let reg = default_tools();
        assert_eq!(reg.tool_count(), 19); // 16 + env.*
        assert!(reg.tool_names().contains(&"env.get".to_string()));
        assert!(reg.tool_names().contains(&"http_request".to_string()));
        assert!(reg.tool_names().contains(&"postgres".to_string()));
        assert!(reg.tool_names().contains(&"sqlite.query".to_string()));
        assert!(reg.tool_names().contains(&"json.parse".to_string()));
        assert!(reg.tool_names().contains(&"array.len".to_string()));
        assert!(reg.tool_names().contains(&"list.get".to_string()));
        assert!(reg.tool_names().contains(&"array.count_where".to_string()));
        assert!(reg.tool_names().contains(&"array.find".to_string()));
        assert!(reg.tool_names().contains(&"dict.get".to_string()));
    }
}
