//! Codegen — Opcode AST (ExecutionPlan) → CompiledPlan (binary FlatBuffers/bincode, uint32 index'ler).
//!
//! v1 codegen: JSON-based `ExecutionPlan` (string ID'ler, O(n) linear scan).
//! v2 codegen: `CompiledPlan` (uint32 index'ler, O(1) random access, dual-format).
//!
//! # Akış
//!
//! 1. Optimize: `optimize::optimize_all(plan)` — constant folding, dead node elimination
//! 2. Compile: `CompiledPlan::from_execution_plan(plan, optimizations)` — string→uint32 mapping
//! 3. Serialize:
//!    - `bincode::serialize(&compiled)` → `binary` (legacy)
//!    - `compiled.to_fb_bytes()` → `fb_binary` (canonical, cross-language)

use std::collections::HashMap;
use std::time::SystemTime;

use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::{ExecutionPlan, ToolDep};

use super::optimize;

/// Errors produced during codegen.
#[derive(Debug, Clone, PartialEq)]
pub struct CodegenError {
    pub message: String,
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "codegen error: {}", self.message)
    }
}

/// Codegen output — the final compilate.
#[derive(Debug, Clone, PartialEq)]
pub struct CodegenOutput {
    /// The finalized compiled plan.
    pub compiled: CompiledPlan,
    /// Binary serialization (bincode).
    pub binary: Vec<u8>,
    /// FlatBuffers serialization (canonical format).
    pub fb_binary: Vec<u8>,
    /// Number of bytes in the bincode binary output.
    pub binary_size_bytes: usize,
    /// Topological node order (string IDs, for backward compat).
    pub execution_order: Vec<String>,
    /// Optimizations applied.
    pub optimizations: Vec<String>,
}

/// Compile a validated ExecutionPlan into a CompiledPlan.
///
/// Steps:
/// 1. Run optimization passes (constant folding, dead node elimination)
/// 2. Fill in compilation metadata
/// 3. Convert string IDs → uint32 indices
/// 4. Serialize to bincode binary
///
/// `schema_hashes` is an optional map from tool name → schema hash,
/// populated at compile time from the tool registry. When empty,
/// schema drift detection is disabled at runtime (the executor check
/// skips entries with empty hashes).
pub fn codegen(plan: ExecutionPlan) -> Result<CodegenOutput, CodegenError> {
    codegen_with_schema_hashes(plan, HashMap::new())
}

/// Like `codegen()` but accepts known schema hashes from the tool registry.
/// The hash is used at runtime for schema drift detection: if a tool's schema
/// changed between compile-time and execution-time, the executor can detect
/// the mismatch and return a `SchemaDriftDetected` error.
pub fn codegen_with_schema_hashes(
    plan: ExecutionPlan,
    schema_hashes: HashMap<String, String>,
) -> Result<CodegenOutput, CodegenError> {
    // Step 1: Optimize
    let opt_result = optimize::optimize_all(plan);
    let mut plan = opt_result.plan;
    let optimizations = opt_result.optimizations_applied;

    // Step 2: Fill compilation metadata
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    plan.metadata.compiled_at = format!("{}", now);
    plan.metadata.node_count = plan.nodes.len() as u32;
    plan.metadata.edge_count = plan.edges.len() as u32;
    plan.metadata.optimizations = optimizations.clone();

    // Compute topological order for execution scheduling
    let execution_order: Vec<String> = plan
        .topological_order()
        .map_err(|e| CodegenError {
            message: format!("topological order failed: {}", e),
        })?
        .into_iter()
        .map(|n| n.id.clone())
        .collect();

    // Extract subgraph dependencies from CALL targets
    plan.metadata.subgraph_dependencies = plan.nodes.iter()
        .filter(|n| n.op == tinypipe_ir::plan::Opcode::Call)
        .filter_map(|n| n.args.iter()
            .find(|a| a.key == "target")
            .and_then(|a| match &a.value {
                tinypipe_ir::plan::ArgValue::String(s) if s.starts_with("subgraph:") => Some(s.clone()),
                _ => None,
            })
        )
        .collect();

    // Extract tool dependencies from CALL targets, populating schema_hash
    // from the provided map (empty hash = drift detection disabled).
    plan.metadata.tool_deps = plan.nodes.iter()
        .filter(|n| n.op == tinypipe_ir::plan::Opcode::Call)
        .filter_map(|n| {
            n.args.iter()
                .find(|a| a.key == "target")
                .and_then(|a| match &a.value {
                    tinypipe_ir::plan::ArgValue::String(s) => {
                        // Skip subgraph targets (those are subgraph deps, not tool deps)
                        if s.starts_with("subgraph:") {
                            return None;
                        }
                        // Parse "tool:name@version" or just "tool:name"
                        let name = s.trim_start_matches("tool:");
                        let (tool_name, version) = if let Some(at_pos) = name.find('@') {
                            (&name[..at_pos], &name[at_pos + 1..])
                        } else {
                            (name, "^0.0.0")
                        };
                        let schema_hash = schema_hashes.get(tool_name)
                            .cloned()
                            .unwrap_or_default();
                        Some(ToolDep {
                            name: tool_name.to_string(),
                            version: version.to_string(),
                            pure: n.op.is_pure(),
                            schema_hash,
                        })
                    }
                    _ => None,
                })
        })
        .collect();

    // Step 3: Convert to CompiledPlan
    let compiled = CompiledPlan::from_execution_plan(&plan, optimizations.clone());

    // Step 4: Serialize to bincode + FlatBuffers binary
    let binary = bincode::serialize(&compiled).map_err(|e| CodegenError {
        message: format!("bincode serialisation failed: {}", e),
    })?;
    let binary_size_bytes = binary.len();
    let fb_binary = compiled.to_fb_bytes().map_err(|e| CodegenError {
        message: format!("FlatBuffers serialisation failed: {}", e),
    })?;

    Ok(CodegenOutput {
        compiled,
        binary,
        fb_binary,
        binary_size_bytes,
        execution_order,
        optimizations,
    })
}

/// Legacy codegen: JSON-based (v1). Useful for debugging and testing.
pub fn codegen_json(plan: ExecutionPlan) -> Result<(tinypipe_ir::plan::ExecutionPlan, String), CodegenError> {
    let mut plan = plan;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    plan.metadata.compiled_at = format!("{}", now);
    plan.metadata.node_count = plan.nodes.len() as u32;
    plan.metadata.edge_count = plan.edges.len() as u32;

    let json = serde_json::to_string_pretty(&plan).map_err(|e| CodegenError {
        message: format!("JSON serialisation failed: {}", e),
    })?;

    Ok((plan, json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_ir::plan::{Edge, Node, Opcode};

    fn sample_plan() -> ExecutionPlan {
        ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 1".into()),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "calc1"),
                Edge::new("calc1", "output1"),
            ],
        )
    }

    #[test]
    fn test_codegen_basic() {
        let output = codegen(sample_plan()).expect("codegen should succeed");
        assert!(output.binary_size_bytes > 0);
        assert_eq!(output.execution_order.len(), 3);
        assert_eq!(output.execution_order[0], "input1");
        assert_eq!(output.execution_order[2], "output1");
        assert!(!output.compiled.metadata.compiled_at.is_empty());
        assert_eq!(output.compiled.metadata.node_count, 3);
        assert_eq!(output.compiled.metadata.edge_count, 2);
    }

    #[test]
    fn test_codegen_binary_roundtrip() {
        let output = codegen(sample_plan()).expect("codegen should succeed");
        let deserialized = CompiledPlan::from_bytes(&output.binary).expect("deserialize");
        assert_eq!(output.compiled, deserialized);
    }

    #[test]
    fn test_codegen_binary_smaller_than_json() {
        let output = codegen(sample_plan()).expect("codegen should succeed");
        let json = serde_json::to_string(&output.compiled).unwrap();
        assert!(
            output.binary.len() < json.len(),
            "binary ({} bytes) should be smaller than JSON ({} bytes)",
            output.binary.len(),
            json.len()
        );
    }

    #[test]
    fn test_codegen_rejects_cycle() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("a", Opcode::Input),
                Node::new("b", Opcode::Calc),
            ],
            vec![
                Edge::new("a", "b"),
                Edge::new("b", "a"),
            ],
        );
        let err = codegen(plan).unwrap_err();
        assert!(err.message.contains("cycle"), "should reject cycle");
    }

    #[test]
    fn test_codegen_empty_plan() {
        let plan = ExecutionPlan::new(vec![], vec![]);
        let output = codegen(plan).expect("empty plan should succeed");
        assert_eq!(output.execution_order.len(), 0);
        assert_eq!(output.compiled.metadata.node_count, 0);
    }

    #[test]
    fn test_codegen_with_optimizations() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                // Constant expression — will be folded
                Node::new("calc1", Opcode::Calc).with_arg("expr", "3 + 5".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "calc1"),
                Edge::new("calc1", "act1"),
            ],
        );
        let output = codegen(plan).expect("codegen should succeed");
        // Should have at least constant_folding optimization
        assert!(
            output.optimizations.iter().any(|o| o.starts_with("constant_folding")),
            "expected constant_folding, got optimizations: {:?}",
            output.optimizations
        );
        // The folded calc should be "8" (JSON-encoded in CompiledArg as "\"8\"")
        let calc_node = output.compiled.nodes.iter().find(|n| n.id == "calc1").unwrap();
        let expr = calc_node.args.iter().find(|a| a.key == "expr").unwrap();
        assert!(expr.value == "\"8\"" || expr.value == "8",
            "expected expr value to be '8', got '{}'", expr.value);
    }

    #[test]
    fn test_codegen_json_v1_legacy() {
        let (plan, json) = codegen_json(sample_plan()).expect("JSON codegen should succeed");
        assert!(!json.is_empty());
        assert_eq!(plan.metadata.node_count, 3);
        assert_eq!(plan.metadata.edge_count, 2);
    }

    #[test]
    fn test_codegen_all_opcodes() {
        // Prove all 11 opcodes pass through codegen generically.
        // Each opcode is handled by CompiledPlan::from_execution_plan() which
        // maps nodes/edges/args without opcode-specific logic.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("n_input", Opcode::Input).with_arg("name", "x".into()),
                Node::new("n_calc", Opcode::Calc).with_arg("expr", "x + 1".into()),
                Node::new("n_call", Opcode::Call).with_arg("target", "tool:echo".into()),
                Node::new("n_decide", Opcode::Decide).with_arg("source", "x".into()),
                Node::new("n_switch", Opcode::Switch).with_arg("source", "x".into()),
                Node::new("n_loop", Opcode::Loop).with_arg("target", "i".into()),
                Node::new("n_parallel", Opcode::Parallel),
                Node::new("n_wait", Opcode::Wait).with_arg("duration_secs", 1i64.into()),
                Node::new("n_merge", Opcode::Merge),
                Node::new("n_act", Opcode::Act).with_arg("type", "return".into()),
                Node::new("n_error", Opcode::Error).with_arg("message", "boom".into()),
            ],
            vec![
                Edge::new("n_input", "n_calc"),
                Edge::new("n_calc", "n_call"),
                Edge::new("n_call", "n_decide"),
                Edge::new("n_call", "n_switch"),
                Edge::new("n_decide", "n_act"),
                Edge::new("n_switch", "n_act"),
                Edge::new("n_loop", "n_merge"),
                Edge::new("n_parallel", "n_merge"),
                Edge::new("n_wait", "n_merge"),
            ],
        );
        let output = codegen(plan).expect("codegen should handle all 11 opcodes");
        assert_eq!(output.compiled.metadata.node_count, 11);
        assert_eq!(output.compiled.metadata.edge_count, 9);
        // Verify all opcodes are in the compiled output
        let ops: Vec<Opcode> = output.compiled.nodes.iter().map(|n| n.op).collect();
        for op in &[Opcode::Input, Opcode::Calc, Opcode::Call, Opcode::Decide,
                    Opcode::Switch, Opcode::Loop, Opcode::Parallel,
                    Opcode::Wait, Opcode::Merge, Opcode::Act, Opcode::Error] {
            assert!(ops.contains(op), "opcode {:?} should be in compiled output", op);
        }
        // Binary serialization roundtrip
        let deserialized = CompiledPlan::from_bytes(&output.binary).expect("binary roundtrip");
        assert_eq!(output.compiled, deserialized);
    }
}
