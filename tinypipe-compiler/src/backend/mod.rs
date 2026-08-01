//! `tinypipe-compiler` Backend — optimize + codegen.
//!
//! Frontend (parse → sanitize → transform → validate) bir `ExecutionPlan` üretir.
//! Backend bu plan'ı alır, optimize eder ve `CompiledPlan` (binary bincode) olarak kodlar.
//!
//! # Akış
//!
//! ```ignore
//! ExecutionPlan (frontend çıktısı)
//!     │
//!     ▼
//! optimize::optimize_all()  ──▶ constant folding, dead node elimination, fusion
//!     │
//!     ▼
//! codegen::codegen()         ──▶ CompiledPlan (uint32 index'ler, binary)
//!     │
//!     ▼
//! bincode::serialize()       ──▶ Vec<u8> (storage'a yazılır)
//! ```

pub mod codegen;
pub mod optimize;

pub use codegen::codegen;
pub use optimize::optimize_all;
