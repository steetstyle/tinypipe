//! `tinypipe-vm` — DAG interpreter (zero-copy FlatBuffers, budget, context, CALL dispatch).
//!
//! # Modüller
//! - `executor`: Core DAG interpreter for `ExecutionPlan` (JSON-based, string IDs)
//! - `compiled_executor`: Zero-copy DAG interpreter for `CompiledPlan` (binary, uint32 indices)
//! - `mocks`: Mock ToolRegistry ve test yardımcıları (tüm test/bench'lerde kullanılır)

pub mod compiled_executor;
pub mod error;
pub mod mocks;
pub mod pause;

pub use compiled_executor::CompiledExecutor;
pub use error::{ExecutionError, ExecutionResult};
pub use mocks::{mock_tools, MockToolRegistry};
pub use pause::{Checkpoint, ExecutionOutcome, LoopState, NoopObserver, PausePolicy, StepObserver};
