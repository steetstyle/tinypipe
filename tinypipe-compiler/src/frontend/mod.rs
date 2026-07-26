//! `tinypipe-compiler` Frontend — parse + sanitize + transform + validate.
//!
//! Restricted Python kodu alır, `ExecutionPlan` (Opcode AST) üretir.
//!
//! # Akış
//!
//! 1. `rustpython_parser` ile Python AST'ye parse et
//! 2. `sanitizer::sanitize()` ile güvenlik kontrolü (import, class, eval yasak)
//! 3. `transform::transform()` ile Python AST → Opcode AST (ExecutionPlan)
//! 4. `validator::validate()` ile statik validasyon (cycle, tool, terminal)
//!
//! Çıktı: `ExecutionPlan` — Backend'e (optimize + codegen) girdi olur.

// Re-exports from the top-level modules (they'll be moved here in future refactoring)
pub use crate::sanitizer;
pub use crate::transform;
pub use crate::validator;
