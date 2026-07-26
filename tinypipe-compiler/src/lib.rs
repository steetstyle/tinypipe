//! `tinypipe-compiler` — Frontend (parse/sanitize/transform/validate) + Backend (optimize/codegen).
//!
//! # Pipeline
//!
//! ```ignore
//! Python code → [Frontend] → ExecutionPlan → [Backend: optimize + codegen] → CompiledPlan (binary)
//! ```
//!
//! ## Frontend
//! - `sanitizer`: AST güvenlik katmanı (Restricted Python kuralları)
//! - `transform`: Python AST → Opcode AST (ExecutionPlan)
//! - `validator`: Static validation (cycle, tool varlığı, terminal)
//!
//! ## Backend
//! - `optimize`: Constant folding, dead node elimination, fusion
//! - `codegen`: ExecutionPlan → CompiledPlan (uint32 index, binary bincode)

pub mod auto_repair;
pub mod frontend;
pub mod backend;
pub mod sanitizer;
pub mod transform;
pub mod type_check;
pub mod validator;

#[cfg(feature = "llm")]
pub mod llm;

// Re-export main entry points
pub use backend::codegen::{codegen, codegen_json, CodegenOutput};
pub use backend::optimize;

/// Full pipeline: parse + sanitize + transform + validate + optimize + codegen.
///
/// Returns the compiled output ready for storage and execution.
pub fn compile(code: &str) -> Result<CodegenOutput, String> {
    let plan = transform::transform(code)
        .map_err(|errors| {
            errors.iter()
                .map(|e| format!("{}:{} — {}", e.line, e.column, e.message))
                .collect::<Vec<_>>()
                .join("\n")
        })?;

    validator::validate(&plan)
        .map_err(|errors| {
            errors.iter()
                .map(|e| format!("{}: {}", e.node_id, e.message))
                .collect::<Vec<_>>()
                .join("\n")
        })?;

    // Type checking (non-fatal warnings for now)
    let type_errors = type_check::check_types(&plan.nodes);
    if !type_errors.is_empty() {
        tracing::info!(
            "Type checking found {} issue(s): {:?}",
            type_errors.len(),
            type_errors
        );
    }

    codegen(plan).map_err(|e| e.message)
}
