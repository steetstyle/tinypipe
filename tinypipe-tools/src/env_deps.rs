//! Env bağımlılığı taraması — `tinypipe-ir::env_deps`'e ince re-export.
//!
//! Geçmişte tarama burada yaşardı; artık IR'de (pure fonksiyonlar, tools'a
//! bağımlılık yok). insight/raporlama katmanları ağır tool bağımlılıkları
//! çekmeden `tinypipe_ir::env_deps`'i kullanır. Bu modül registry ve eski
//! çağrıların kırılmaması için korunur.

pub use tinypipe_ir::env_deps::{
    extract_template_keys, scan_plan_env_deps, subgraph_targets, EnvDep, EnvDepReport,
};
