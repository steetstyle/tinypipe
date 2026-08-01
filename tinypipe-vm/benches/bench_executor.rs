//! Executor benchmarks — ExecutionPlan execution latency.
//!
//! Run: cargo bench -p tinypipe-vm --bench bench_executor
//!
//! To update baseline: cargo bench -- --save-baseline
//! Regression detection: compare against baseline JSON in benches/baseline/

use std::time::Instant;

use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::{ArgValue, Edge, ExecutionPlan, Node, Opcode};
use tinypipe_vm::CompiledExecutor;

fn compile(plan: ExecutionPlan) -> CompiledPlan {
    CompiledPlan::from_execution_plan(&plan, vec![])
}

#[allow(dead_code)]
struct BenchStats {
    count: usize,
    mean: f64,
    min: f64,
    max: f64,
    p50: f64,
    p95: f64,
    p99: f64,
}

fn run_bench<F: FnMut()>(name: &str, mut f: F, iterations: usize) -> BenchStats {
    // Warmup
    for _ in 0..10 {
        f();
    }

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        let elapsed = start.elapsed().as_nanos() as f64 / 1000.0; // μs
        samples.push(elapsed);
    }

    samples.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let count = samples.len();
    let mean = samples.iter().sum::<f64>() / count as f64;
    let min = samples[0];
    let max = samples[count - 1];
    let p50 = samples[count / 2];
    let p95 = samples[(count as f64 * 0.95) as usize];
    let p99 = samples[(count as f64 * 0.99) as usize];

    let stats = BenchStats {
        count,
        mean,
        min,
        max,
        p50,
        p95,
        p99,
    };

    println!("  {name:<40}  mean={mean:8.3}µs  p50={p50:8.3}µs  p95={p95:8.3}µs  p99={p99:8.3}µs  min={min:8.3}µs  max={max:8.3}µs");

    stats
}

fn simple_plan() -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
            Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 1".into()),
            Node::new("output1", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("source", "calc1".into()),
        ],
        vec![Edge::new("input1", "calc1"), Edge::new("calc1", "output1")],
    )
}

fn decide_plan() -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
            Node::new("decide1", Opcode::Decide).with_arg("condition", "x > 0".into()),
            Node::new("true_branch", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("source", "input1".into()),
            Node::new("false_branch", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("source", "input1".into()),
        ],
        vec![
            Edge::new("input1", "decide1"),
            Edge::with_condition("decide1", "true_branch", "true"),
            Edge::with_condition("decide1", "false_branch", "false"),
        ],
    )
}

fn chain_plan(node_count: usize) -> ExecutionPlan {
    let mut nodes = vec![Node::new("input1", Opcode::Input).with_arg("name", "x".into())];
    let mut edges = Vec::new();
    for i in 0..node_count {
        let nid = format!("calc{}", i);
        nodes.push(Node::new(&nid, Opcode::Calc).with_arg("expr", "x + 1".into()));
        if i == 0 {
            edges.push(Edge::new("input1", &nid));
        } else {
            edges.push(Edge::new(&format!("calc{}", i - 1), &nid));
        }
    }
    let out_id = "output1";
    nodes.push(
        Node::new(out_id, Opcode::Act)
            .with_arg("type", "return".into())
            .with_arg(
                "source",
                ArgValue::String(format!("calc{}", node_count - 1)),
            ),
    );
    edges.push(Edge::new(&format!("calc{}", node_count - 1), out_id));
    ExecutionPlan::new(nodes, edges)
}

/// Loop plan: `x = 0; for i in range(max_iter): x = x + 1`.
fn loop_plan(max_iter: u32) -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input_x", Opcode::Input).with_arg("name", "x".into()),
            Node::new("loop1", Opcode::Loop)
                .with_arg("target", "i".into())
                .with_arg("max_iterations", (max_iter as i64).into()),
            Node::new("body_calc", Opcode::Calc)
                .with_arg("expr", "x + 1".into())
                .with_arg("output", "x".into()),
            Node::new("body_decide", Opcode::Decide)
                .with_arg("source", "x".into())
                .with_arg("op", "lt".into())
                .with_arg("value", (max_iter as i64).into()),
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

fn main() {
    println!("\n=== Compiled Executor Benchmarks ===\n");
    let reg = tinypipe_tools::mock_tools();

    // 1. Simple 3-node plan
    let plan = compile(simple_plan());
    let exec = CompiledExecutor::new(&plan, &reg);
    let mut inputs = Context::new();
    inputs.set("x".into(), Value::Int(42));
    run_bench(
        "simple plan (3 nodes)",
        || {
            let _result = exec.execute(inputs.clone()).unwrap();
        },
        1000,
    );

    // 2. Decide plan (4 nodes, conditional branch)
    let plan = compile(decide_plan());
    let exec = CompiledExecutor::new(&plan, &reg);
    run_bench(
        "decide plan (4 nodes)",
        || {
            let _result = exec.execute(inputs.clone()).unwrap();
        },
        1000,
    );

    // 3. Chain of 10 CALC nodes
    let plan = compile(chain_plan(10));
    let exec = CompiledExecutor::new(&plan, &reg);
    run_bench(
        "chain 10 nodes",
        || {
            let _result = exec.execute(inputs.clone()).unwrap();
        },
        500,
    );

    // 4. Chain of 100 CALC nodes
    let plan = compile(chain_plan(100));
    let exec = CompiledExecutor::new(&plan, &reg);
    run_bench(
        "chain 100 nodes",
        || {
            let _result = exec.execute(inputs.clone()).unwrap();
        },
        100,
    );

    // 5. Loop: 10k iteration (calc + decide body)
    let mut plan = compile(loop_plan(10_000));
    plan.metadata.max_node_execution_count = 1_000_000;
    let exec = CompiledExecutor::new(&plan, &reg);
    let mut loop_inputs = Context::new();
    loop_inputs.set("x".into(), Value::Int(0));
    run_bench(
        "loop 10k iterations (~20k nodes)",
        || {
            let _result = exec.execute(loop_inputs.clone()).unwrap();
        },
        50,
    );

    // 6. Chain of 1000 CALC nodes
    let plan = compile(chain_plan(1000));
    let exec = CompiledExecutor::new(&plan, &reg);
    run_bench(
        "chain 1000 nodes",
        || {
            let _result = exec.execute(inputs.clone()).unwrap();
        },
        100,
    );

    println!();
}
