//! Shared error types and execution result for tinypipe-vm.

use tinypipe_api::types::{Context, Value};

// ── IR version constants ──────────────────────────────────────────

/// Minimum supported IR version.
pub const MIN_SUPPORTED_VERSION: u16 = 2;

/// Maximum supported IR version (current).
pub const MAX_SUPPORTED_VERSION: u16 = 4;

/// Check if an IR version is compatible with this VM.
/// Returns Ok(()) if compatible, Err with message if not.
pub fn check_version_compatibility(plan_version: u16) -> Result<(), String> {
    if plan_version < MIN_SUPPORTED_VERSION {
        return Err(format!(
            "IR version {} is too old. Minimum supported: {}. Recompile the plan.",
            plan_version, MIN_SUPPORTED_VERSION
        ));
    }
    if plan_version > MAX_SUPPORTED_VERSION {
        return Err(format!(
            "IR version {} is too new. Maximum supported: {}. Upgrade the VM.",
            plan_version, MAX_SUPPORTED_VERSION
        ));
    }
    Ok(())
}

// ── Error types ─────────────────────────────────────────────────────

/// Execution errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ExecutionError {
    #[error("Plan contains a cycle")]
    CycleDetected,
    #[error("Node '{0}' not found")]
    NodeNotFound(String),
    #[error("Variable '{0}' not found in context")]
    VariableNotFound(String),
    #[error("CALL dispatch failed for '{0}': {1}")]
    CallFailed(String, String),
    #[error("Expression evaluation failed: {0}")]
    EvalError(String),
    #[error("Branch condition evaluation failed: {0}")]
    ConditionError(String),
    #[error("LOOP iteration limit ({0}) exceeded")]
    LoopLimitExceeded(u32),
    #[error("Execution time limit ({0}ms) exceeded")]
    TimeLimitExceeded(u64),
    #[error("Context memory limit ({0} bytes) exceeded (approx {1} bytes)")]
    MemoryLimitExceeded(u64, u64),
    #[error("Node execution count ({0}) exceeded plan budget ({1})")]
    NodeBudgetExceeded(u32, u32),
    #[error("Subgraph recursion depth ({0}) exceeded max ({1})")]
    RecursionDepthExceeded(u32, u32),
    #[error("Subgraph recursion limit exceeded for '{0}'")]
    RecursionLimitExceeded(String),
    #[error("{0}")] // Wrapper for general errors (for ERROR opcode)
    Custom(String),
    #[error("IR version mismatch: {0}")]
    VersionMismatch(String),
    #[error("Schema drift detected for tool '{0}': compiled hash '{1}' != registry hash '{2}'")]
    SchemaDriftDetected(String, String, String),
}

/// Successful execution result.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Final context after execution.
    pub context: Context,
    /// Node IDs in execution order (only actually-executed nodes).
    pub execution_order: Vec<String>,
    /// Per-node wall-clock durations (node_id, microseconds).
    /// Yalnızca ana passta çalıştırılan node'ları kapsar; paralel branch
    /// thread'leri ve loop gövde iterasyonları LOOP/PARALLEL node'unun
    /// toplam süresi içinde değerlendirilir.
    pub node_durations: Vec<(String, u64)>,
    /// Total nodes executed.
    pub node_count: u32,
    /// Wall-clock duration in microseconds.
    pub duration_us: u64,
    /// Output value (from ACT `return` or last CALC/CALL).
    pub output: Option<Value>,
}
