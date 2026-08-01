//! `tinypipe-vm` — DAG interpreter (zero-copy FlatBuffers, budget, context, CALL dispatch).
//!
//! # Modüller
//! - `executor`: Core DAG interpreter for `ExecutionPlan` (JSON-based, string IDs)
//! - `compiled_executor`: Zero-copy DAG interpreter for `CompiledPlan` (binary, uint32 indices)
//! - `pause`: Checkpoint/pause/resume altyapısı
//!
//! Tool registry `tinypipe-tools` paketindedir (test'ler ve CLI için).

pub mod compiled_executor;
pub mod error;
pub mod pause;

pub use compiled_executor::CompiledExecutor;
pub use error::{ExecutionError, ExecutionResult};
pub use pause::{Checkpoint, ExecutionOutcome, LoopState, NoopObserver, PausePolicy, StepObserver};
