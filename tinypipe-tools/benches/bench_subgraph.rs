//! SubgraphToolRegistry benchmark — subgraph çağrı başına maliyet.
//!
//! Run: cargo bench -p tinypipe-tools --bench bench_subgraph
//!
//! Ölçülen: her subgraph çağrısında registry'nin yaptığı iş
//! (list_all_graphs + load_plan + plan decode + executor kurulumu + çocuk
//! çalıştırma). Storage: gerçek DB'ye benzer ~25 grafik içeren in-memory SQLite.

use std::sync::Arc;
use std::time::Instant;

use tinypipe_api::storage::GraphStorage;
use tinypipe_api::types::{Context, Value, Version};
use tinypipe_compiler::compile;
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::{Edge, ExecutionPlan, Node, Opcode};
use tinypipe_storage::SqliteStorage;
use tinypipe_tools::SubgraphToolRegistry;
use tinypipe_vm::CompiledExecutor;

fn bench<F: FnMut()>(name: &str, mut f: F, iterations: usize) {
    for _ in 0..10 {
        f();
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_nanos() as f64 / 1000.0);
    }
    samples.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() as f64 * 0.95) as usize];
    println!(
        "  {name:<46} mean={mean:10.3}µs  p50={p50:10.3}µs  p95={p95:10.3}µs"
    );
}

/// Parent plan: N ardışık `subgraph:perf_child` çağrısı (zincir).
fn call_chain_plan(n: usize) -> ExecutionPlan {
    let mut nodes = vec![Node::new("input1", Opcode::Input).with_arg("name", "x".into())];
    let mut edges = Vec::new();
    for i in 0..n {
        nodes.push(
            Node::new(&format!("call{i}"), Opcode::Call)
                .with_arg("target", "subgraph:perf_child".into()),
        );
        let prev = if i == 0 { "input1" } else { &format!("call{}", i - 1) };
        edges.push(Edge::new(prev, &format!("call{i}")));
    }
    let out = format!("call{}", n - 1);
    nodes.push(
        Node::new("output1", Opcode::Act)
            .with_arg("type", "return".into())
            .with_arg("source", tinypipe_ir::plan::ArgValue::String(out.clone())),
    );
    edges.push(Edge::new(&out, "output1"));
    ExecutionPlan::new(nodes, edges)
}

/// Parent plan: loop body içinde subgraph çağrısı (dashboard_seeds deseni).
fn loop_subgraph_plan(max_iter: u32) -> ExecutionPlan {
    ExecutionPlan::new(
        vec![
            Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
            Node::new("loop1", Opcode::Loop)
                .with_arg("target", "i".into())
                .with_arg("max_iterations", (max_iter as i64).into()),
            Node::new("body_call", Opcode::Call)
                .with_arg("target", "subgraph:perf_child".into()),
            Node::new("body_decide", Opcode::Decide)
                .with_arg("source", "i".into())
                .with_arg("op", "lt".into())
                .with_arg("value", (max_iter as i64).into()),
            Node::new("output", Opcode::Act)
                .with_arg("type", "return".into())
                .with_arg("value", "0".into()),
        ],
        vec![
            Edge::new("input1", "loop1"),
            Edge::new("loop1", "body_call"),
            Edge::new("body_call", "body_decide"),
            Edge::control("loop1", "output"),
        ],
    )
}

fn main() {
    println!("\n=== Subgraph Dispatch Benchmarks ===\n");

    // Storage: gerçek DB'ye benzer ~25 grafik (1'i çocuk)
    let store = SqliteStorage::in_memory().unwrap();
    let child = compile("def graph():\n    return 42").unwrap();
    let child_id = store
        .create_graph("perf_child", "def graph():\n    return 42")
        .unwrap();
    store
        .save_plan(&child_id, Version(1), &child.fb_binary)
        .unwrap();
    for i in 0..24 {
        let _ = store
            .create_graph(&format!("filler_{i}"), "def graph():\n    return 0")
            .unwrap();
    }

    let child_plan = CompiledPlan::from_fb_bytes(&child.fb_binary).unwrap();

    // ── Parça parça maliyetler ──
    bench(
        "list_all_graphs (25 graphs)",
        || {
            let _ = store.list_all_graphs(None, None).unwrap();
        },
        2000,
    );
    bench(
        "find_graph_by_name (indexed)",
        || {
            let _ = store.find_graph_by_name("perf_child").unwrap();
        },
        2000,
    );
    bench(
        "load_graph (by id, PK)",
        || {
            let _ = store.load_graph(&child_id).unwrap();
        },
        2000,
    );
    bench(
        "load_plan + from_fb_bytes (child)",
        || {
            let _ = store.load_plan(&child_id).unwrap();
            let _ = CompiledPlan::from_fb_bytes(&child.fb_binary).unwrap();
        },
        2000,
    );
    let reg_mock = tinypipe_tools::mock_tools();
    bench(
        "CompiledExecutor::new (child, 4 nodes)",
        || {
            let _ = CompiledExecutor::new(&child_plan, &reg_mock);
        },
        2000,
    );
    bench(
        "child execute (CompiledExecutor)",
        || {
            let exec = CompiledExecutor::new(&child_plan, &reg_mock);
            let mut ctx = Context::new();
            ctx.set("x".into(), Value::Int(1));
            let _ = exec.execute(ctx).unwrap();
        },
        1000,
    );

    // ── Tam subgraph çağrısı ──
    let registry = Arc::new(SubgraphToolRegistry::with_tools(
        store,
        tinypipe_tools::default_tools(),
    ))
    .init();

    let chain = CompiledPlan::from_execution_plan(&call_chain_plan(50), vec![]);
    let exec = CompiledExecutor::new(
        &chain,
        registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
    );
    let inputs = Context::new();
    bench(
        "chain: 50 subgraph calls (per execution)",
        || {
            let _ = exec.execute(inputs.clone()).unwrap();
        },
        100,
    );

    let loop_plan = CompiledPlan::from_execution_plan(&loop_subgraph_plan(50), vec![]);
    let exec = CompiledExecutor::new(
        &loop_plan,
        registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
    );
    bench(
        "loop: 50 subgraph calls (per execution)",
        || {
            let _ = exec.execute(inputs.clone()).unwrap();
        },
        100,
    );

    println!();
}
