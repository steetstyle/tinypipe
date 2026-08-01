//! MockToolRegistry benchmark — dispatch latency, tool resolution, factory overhead.
//!
//! Run: cargo bench -p tinypipe-vm --bench bench_mock

use std::time::Instant;

use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{CallTarget, Context, Value};
use tinypipe_tools::mock_tools;

#[allow(dead_code)]
struct BenchStats {
    count: usize,
    mean: f64, // μs
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

    // Measure
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

fn main() {
    println!("\n=== MockToolRegistry Benchmarks ===\n");

    // 1. Mock registry creation
    run_bench(
        "mock_tools factory",
        || {
            let _reg = mock_tools();
        },
        1000,
    );

    // 2. Resolve a tool
    let reg = mock_tools();
    run_bench(
        "resolve 'math.add'",
        || {
            let _spec = reg.resolve("math.add", "1.0").unwrap();
        },
        10_000,
    );

    // 3. Dispatch math.add
    let mut ct = CallTarget::new("math.add");
    ct.args.push(Value::Float(3.0));
    ct.args.push(Value::Float(4.0));
    let ctx = Context::new();
    run_bench(
        "dispatch 'math.add'",
        || {
            let _result = reg.dispatch(&ct, &ctx, &tinypipe_env::Env::empty()).unwrap();
        },
        10_000,
    );

    // 4. Dispatch echo
    let mut echo_ct = CallTarget::new("echo");
    echo_ct.args.push(Value::String("hello".into()));
    run_bench(
        "dispatch 'echo'",
        || {
            let _result = reg.dispatch(&echo_ct, &ctx, &tinypipe_env::Env::empty()).unwrap();
        },
        10_000,
    );

    // 5. Dispatch error
    let err_ct = CallTarget::new("test.error");
    run_bench(
        "dispatch 'test.error' (error path)",
        || {
            let _result = reg.dispatch(&err_ct, &ctx, &tinypipe_env::Env::empty());
        },
        10_000,
    );

    // 6. Resolve nonexistent (error path)
    run_bench(
        "resolve nonexistent (error path)",
        || {
            let _err = reg.resolve("does.not.exist", "1.0");
        },
        10_000,
    );

    println!();
}
