//! `tinypipe-tools` — Tool registry ve gömülü tool'lar.
//!
//! Ağır bağımlılıklar (ureq, postgres) sadece burada yaşar; diğer
//! paketler (vm, api, ir, storage, compiler) lightweight kalır.

pub mod env_deps;
pub mod mock;
pub mod registry;
pub mod tools;

pub use mock::MockToolRegistry;
pub use registry::SubgraphToolRegistry;
pub use tools::array_len::array_len;
pub use tools::count_where::count_where;
pub use tools::default_tools;
pub use tools::http_request::http_request;
pub use tools::json_parse::json_parse;
pub use tools::list_get::list_get;
pub use tools::mock_tools;
pub use tools::postgres::postgres;
pub use tools::sqlite::sqlite_query;
