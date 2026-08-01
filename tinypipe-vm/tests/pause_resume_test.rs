//! Pause/resume integration tests.
//!
//! Her test: aynı plan'ı (a) tek seferde, (b) pause → resume zinciriyle çalıştırır
//! ve sonuçların birebir eşit olduğunu doğrular.

use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::{Edge, ExecutionPlan, Node, Opcode};
use tinypipe_vm::{
    CompiledExecutor, ExecutionOutcome, NoopObserver, PausePolicy, StepObserver,
};

fn compile_plan(plan: ExecutionPlan) -> CompiledPlan {
    CompiledPlan::from_execution_plan(&plan, vec![])
}

/// Basit lineer graf: x + 1 → x * 2 → return
fn linear_plan() -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input_x", Opcode::Input).with_arg("name", "x".into()),
            Node::new("calc_a", Opcode::Calc)
                .with_arg("expr", "x + 1".into())
                .with_arg("output", "a".into()),
            Node::new("calc_b", Opcode::Calc)
                .with_arg("expr", "a * 2".into())
                .with_arg("output", "b".into()),
            Node::new("calc_c", Opcode::Calc)
                .with_arg("expr", "b - 3".into())
                .with_arg("output", "c".into()),
            Node::new("output", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("value", "c".into()),
        ],
        vec![
            Edge::new("input_x", "calc_a"),
            Edge::new("calc_a", "calc_b"),
            Edge::new("calc_b", "calc_c"),
            Edge::new("calc_c", "output"),
        ],
    )
}

fn inputs(x: i64) -> Context {
    let mut c = Context::new();
    c.set("x".into(), Value::Int(x));
    c
}

#[test]
fn test_pause_resume_linear_equiv() {
    let compiled = compile_plan(linear_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let full = exec.execute(inputs(5)).expect("full should succeed");

    // max_nodes=2 ile duraklat → resume
    let policy = PausePolicy {
        max_nodes: Some(2),
        ..Default::default()
    };
    let cp = match exec.execute_with(inputs(5), &policy, None).unwrap() {
        ExecutionOutcome::Paused(cp) => cp,
        ExecutionOutcome::Completed(_) => panic!("expected Paused"),
    };

    let resumed = match exec.resume(&cp, &PausePolicy::default(), None).unwrap() {
        ExecutionOutcome::Completed(r) => r,
        ExecutionOutcome::Paused(_) => panic!("expected Completed after resume"),
    };

    assert_eq!(resumed.output, full.output);
    assert_eq!(resumed.context.variables, full.context.variables);
    assert_eq!(resumed.node_count, full.node_count);
    assert_eq!(resumed.execution_order, full.execution_order);
    // x=5: a=6, b=12, c=9
    assert_eq!(resumed.output, Some(Value::Int(9)));
}

#[test]
fn test_pause_resume_multiple_segments() {
    let compiled = compile_plan(linear_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let full = exec.execute(inputs(7)).unwrap();

    let policy = PausePolicy {
        max_nodes: Some(1),
        ..Default::default()
    };
    let mut segments = 0;
    let mut cp = match exec.execute_with(inputs(7), &policy, None).unwrap() {
        ExecutionOutcome::Paused(cp) => cp,
        ExecutionOutcome::Completed(_) => panic!("expected Paused"),
    };
    segments += 1;
    loop {
        let res = exec.resume(&cp, &policy, None).unwrap();
        eprintln!(
            "SEG: cp.pos={} cp.ls={:?} -> {}",
            cp.position,
            cp.loop_state,
            match &res {
                ExecutionOutcome::Completed(r) => format!(
                    "Completed out={:?} cnt={} vars={:?}",
                    r.output, r.node_count, r.context.variables
                ),
                ExecutionOutcome::Paused(p) => format!(
                    "Paused pos={} ls={:?} cnt={} vars={:?}",
                    p.position, p.loop_state, p.node_count, p.context.variables
                ),
            }
        );
        match res {
            ExecutionOutcome::Completed(r) => {
                assert_eq!(r.output, full.output);
                assert_eq!(r.node_count, full.node_count);
                assert_eq!(r.execution_order, full.execution_order);
                assert_eq!(segments + 1, 5, "4 pauses + 1 completion");
                break;
            }
            ExecutionOutcome::Paused(cp2) => {
                segments += 1;
                cp = cp2;
            }
        }
    }
}

#[test]
fn test_pause_at_node_id() {
    let compiled = compile_plan(linear_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let full = exec.execute(inputs(3)).unwrap();

    // calc_b çalıştıktan hemen sonra duraklat
    let policy = PausePolicy {
        pause_at_node_ids: Some(vec!["calc_b".into()]),
        ..Default::default()
    };
    let cp = match exec.execute_with(inputs(3), &policy, None).unwrap() {
        ExecutionOutcome::Paused(cp) => cp,
        ExecutionOutcome::Completed(_) => panic!("expected Paused"),
    };
    assert!(cp.execution_order.contains(&"calc_b".to_string()));
    assert!(!cp.execution_order.contains(&"calc_c".to_string()));

    let resumed = match exec.resume(&cp, &PausePolicy::default(), None).unwrap() {
        ExecutionOutcome::Completed(r) => r,
        ExecutionOutcome::Paused(_) => panic!("expected Completed"),
    };
    assert_eq!(resumed.output, full.output);
}

fn loop_plan() -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input_x", Opcode::Input).with_arg("name", "x".into()),
            Node::new("loop1", Opcode::Loop).with_arg("max_iterations", 5i64.into()),
            Node::new("body_calc", Opcode::Calc)
                .with_arg("expr", "x + 1".into())
                .with_arg("output", "x".into()),
            Node::new("body_decide", Opcode::Decide)
                .with_arg("source", "x".into())
                .with_arg("op", "lt".into())
                .with_arg("value", 5i64.into()),
            Node::new("output", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("value", "x".into()),
        ],
        vec![
            Edge::new("input_x", "loop1"),
            Edge::new("loop1", "body_calc"),
            Edge::new("body_calc", "body_decide"),
            Edge::control("loop1", "output"),
        ],
    )
}

#[test]
fn test_pause_resume_mid_loop() {
    let compiled = compile_plan(loop_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let full = exec.execute(inputs(0)).unwrap();
    // x=0 → x<5 iken devam: iter0: 1, iter1: 2 ... iter4: 5, 5<5 false → break
    assert_eq!(full.output, Some(Value::Int(5)));

    // Loop gövdesi ortasında duraklat: body_node sayımı dahil toplam 3. node'da pause
    let policy = PausePolicy {
        max_nodes: Some(3),
        ..Default::default()
    };
    let cp = match exec.execute_with(inputs(0), &policy, None).unwrap() {
        ExecutionOutcome::Paused(cp) => cp,
        ExecutionOutcome::Completed(_) => panic!("expected Paused mid-loop"),
    };
    assert!(
        cp.loop_state.is_some(),
        "pause inside loop body must capture loop_state, got {:?}",
        cp.loop_state
    );

    let resumed = match exec.resume(&cp, &PausePolicy::default(), None).unwrap() {
        ExecutionOutcome::Completed(r) => r,
        ExecutionOutcome::Paused(_) => panic!("expected Completed"),
    };
    assert_eq!(resumed.output, full.output);
    assert_eq!(resumed.context.variables, full.context.variables);
    assert_eq!(resumed.node_count, full.node_count);
}

#[test]
fn test_pause_resume_loop_state_exact() {
    let compiled = compile_plan(loop_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let full = exec.execute(inputs(2)).unwrap();
    assert_eq!(full.output, Some(Value::Int(5)));

    // Her segment 2 node çalıştırsın → muhtemelen loop içinde çok sayıda pause
    let policy = PausePolicy {
        max_nodes: Some(2),
        ..Default::default()
    };
    let mut cp = match exec.execute_with(inputs(2), &policy, None).unwrap() {
        ExecutionOutcome::Paused(cp) => cp,
        ExecutionOutcome::Completed(_) => panic!("expected Paused"),
    };
    let mut iterations = 0;
    loop {
        iterations += 1;
        assert!(iterations < 100, "resume should converge");
        match exec.resume(&cp, &policy, None).unwrap() {
            ExecutionOutcome::Completed(r) => {
                assert_eq!(r.output, full.output);
                assert_eq!(r.node_count, full.node_count);
                break;
            }
            ExecutionOutcome::Paused(cp2) => cp = cp2,
        }
    }
}

#[derive(Default)]
struct RecordingObserver {
    started: Vec<String>,
    ended: Vec<String>,
}

impl StepObserver for RecordingObserver {
    fn on_node_start(&mut self, node_id: &str) {
        self.started.push(node_id.to_string());
    }
    fn on_node_end(&mut self, node_id: &str) {
        self.ended.push(node_id.to_string());
    }
}

#[test]
fn test_observer_records_all_nodes() {
    let compiled = compile_plan(linear_plan());
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);

    let mut obs = RecordingObserver::default();
    let outcome = exec
        .execute_with(inputs(4), &PausePolicy::default(), Some(&mut obs))
        .unwrap();
    let result = match outcome {
        ExecutionOutcome::Completed(r) => r,
        ExecutionOutcome::Paused(_) => panic!("expected Completed"),
    };
    assert_eq!(obs.started.len(), result.execution_order.len());
    assert_eq!(obs.ended.len(), result.execution_order.len());
    assert_eq!(obs.started, result.execution_order);
    assert_eq!(obs.ended, result.execution_order);

    let _ = NoopObserver;
}
