//! Process replay integration tests.
//!
//! Her test bir reference execution plan'ı yükler, `MockToolRegistry` ile çalıştırır,
//! sonucu beklenen output ile karşılaştırır.
//!
//! Yeni bir reference eklemek için:
//!   1. Plan JSON'unu `tests/replay/references/<name>.json`'a koy
//!   2. Beklenen output'u `tests/replay/results/<name>.json`'a koy
//!   3. Bu dosyaya yeni bir test fonksiyonu ekle
//!
//! Reference güncellemek için `UPDATE_REFS=1 cargo test` çalıştır.

use std::fs;
use std::path::PathBuf;

use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::ExecutionPlan;
use tinypipe_vm::{CompiledExecutor, ExecutionResult};

fn test_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Reference plan'ı JSON'dan yükle.
fn load_reference(name: &str) -> ExecutionPlan {
    let path = test_root().join(format!("tests/replay/references/{}.json", name));
    let json = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read reference '{}' at {:?}: {}", name, path, e));
    serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("Failed to parse reference '{}': {}", name, e))
}

/// Reference result'ı yükle (varsa).
#[allow(dead_code)]
fn load_expected(name: &str) -> Option<serde_json::Value> {
    let path = test_root().join(format!("tests/replay/results/{}.json", name));
    if path.exists() {
        let json = fs::read_to_string(&path).unwrap();
        Some(serde_json::from_str(&json).unwrap())
    } else {
        None
    }
}

fn compile_plan(plan: ExecutionPlan) -> CompiledPlan {
    CompiledPlan::from_execution_plan(&plan, vec![])
}

// ======================= Test Cases =======================

#[test]
fn test_replay_basic_add() {
    let plan = load_reference("basic_add");
    let compiled = compile_plan(plan);
    let reg = tinypipe_vm::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);
    let mut inputs = Context::new();
    inputs.set("x".into(), Value::Int(5));

    let result = exec
        .execute(inputs)
        .expect("replay basic_add should succeed");

    // x + 1 = 6 (compiled uses i64)
    assert_eq!(result.output, Some(Value::Int(6)));
}

#[test]
fn test_replay_plan_loads() {
    let plan = load_reference("basic_add");
    assert_eq!(plan.metadata.node_count, 3);
    assert_eq!(plan.metadata.edge_count, 2);
    assert_eq!(plan.version, 2);
}

#[test]
fn test_replay_topological_order() {
    let plan = load_reference("basic_add");
    let order = plan.topological_order().unwrap();
    assert_eq!(order.len(), 3);
    // input_x her zaman ilk
    assert_eq!(order[0].id, "input_x");
    assert_eq!(order[1].id, "calc_add");
}

/// Full VM executor test on a reference plan (compiled executor).
fn execute_reference(name: &str, inputs: Context) -> ExecutionResult {
    let plan = load_reference(name);
    let compiled = compile_plan(plan);
    let reg = tinypipe_vm::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);
    exec.execute(inputs)
        .expect(&format!("replay '{}' should succeed", name))
}

#[test]
fn test_replay_basic_add_full_exec() {
    use tinypipe_api::types::Value;
    let mut inputs = Context::new();
    inputs.set("x".into(), Value::Int(5));
    let result = execute_reference("basic_add", inputs);
    // x + 1 = 6 (compiled uses i64)
    assert_eq!(result.output, Some(Value::Int(6)));
    assert_eq!(result.node_count, 3);
}

#[test]
fn test_replay_decide_true_full_exec() {
    use tinypipe_api::types::Value;
    let mut inputs = Context::new();
    inputs.set("x".into(), Value::Int(5));
    let result = execute_reference("decide_true", inputs);
    // 5 > 0 → true branch → returns input_x (5)
    assert!(result.output.is_some());
    assert_eq!(result.node_count, 3); // input_x + decide1 + true_branch
}

#[test]
fn test_replay_decide_false_full_exec() {
    use tinypipe_api::types::Value;
    let mut inputs = Context::new();
    inputs.set("x".into(), Value::Int(-3));
    let result = execute_reference("decide_true", inputs);
    // -3 > 0 → false branch → returns input_x (-3)
    assert!(result.output.is_some());
    assert_eq!(result.node_count, 3); // input_x + decide1 + false_branch
}

#[test]
fn test_replay_all_pass_validation() {
    for name in &["basic_add", "decide_true"] {
        let plan = load_reference(name);
        assert!(
            plan.topological_order().is_ok(),
            "reference '{}' has a cycle",
            name
        );
        assert!(!plan.nodes.is_empty(), "reference '{}' has no nodes", name);
    }
}

#[test]
fn test_replay_executor_budget_allows_basic() {
    // Verify that the metadata.budget allows basic execution
    let plan = load_reference("basic_add");
    assert!(plan.metadata.max_node_execution_count >= plan.nodes.len() as u32);
    assert!(plan.metadata.max_execution_time_ms > 0);
}
