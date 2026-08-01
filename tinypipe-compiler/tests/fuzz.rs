//! Graph fuzz — random Restricted Python code generation + compile.
//!
//! Run: cargo test -p tinypipe-compiler --test fuzz -- --nocapture
//!
//! Generates random graph definitions and verifies the compiler
//! never panics (returns either a valid ExecutionPlan or a well-formed error).

use tinypipe_compiler::{backend, sanitizer, transform, validator};

/// Simple xorshift64 RNG (stateful).
struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, max: usize) -> usize {
        (self.next() as usize) % max
    }
}

/// Generate a random identifier name.
fn rand_name(rng: &mut SimpleRng) -> String {
    let names = [
        "x", "y", "z", "a", "b", "c", "val", "data", "total", "count", "result", "item",
    ];
    let idx = rng.range(names.len());
    names[idx].to_owned()
}

/// Generate a random integer literal string.
fn rand_int(rng: &mut SimpleRng) -> String {
    let v = (rng.next() % 100) as i64;
    v.to_string()
}

/// Generate a simple expression string (non-recursive to avoid stack overflow).
fn rand_expr(rng: &mut SimpleRng, _depth: u32) -> String {
    match rng.range(5) {
        0 => rand_int(rng),
        1 => rand_name(rng),
        2 => format!("{} + {}", rand_name(rng), rand_int(rng)),
        3 => format!("{} * {}", rand_name(rng), rand_int(rng)),
        _ => rand_name(rng),
    }
}

/// Generate a random statement.
fn rand_stmt(rng: &mut SimpleRng, indent: &str) -> String {
    match rng.range(10) {
        0 => format!("{}pass", indent),
        1 => format!("{}{} = {}", indent, rand_name(rng), rand_expr(rng, 1)),
        2 => format!(
            "{}{} = call(\"math.add\", a={}, b={})",
            indent,
            rand_name(rng),
            rand_int(rng),
            rand_int(rng)
        ),
        3 => format!("{}{} += 1", indent, rand_name(rng)),
        4 => format!("{}act(\"LOG\", msg=\"test\")", indent),
        5 => {
            let cond = format!("{} > {}", rand_name(rng), rand_int(rng));
            let body = rand_stmt(rng, &format!("{}    ", indent));
            format!("{}if {}:\n{}", indent, cond, body)
        }
        6 => {
            let cond = format!("{} > {}", rand_name(rng), rand_int(rng));
            let body = rand_stmt(rng, &format!("{}    ", indent));
            let else_body = rand_stmt(rng, &format!("{}    ", indent));
            format!(
                "{}if {}:\n{}{}else:\n{}",
                indent, cond, body, indent, else_body
            )
        }
        7 => format!(
            "{}for {} in range({}):\n{}    pass",
            indent,
            rand_name(rng),
            (rng.next() % 10) + 1,
            indent
        ),
        8 => format!("{}return {}", indent, rand_expr(rng, 1)),
        _ => format!("{}x = {}", indent, rand_int(rng)),
    }
}

/// Generate a complete Restricted Python graph.
fn rand_graph(rng: &mut SimpleRng, num_stmts: usize) -> String {
    let params: Vec<String> = (0..(rng.next() as usize % 3 + 1))
        .map(|_| format!("{}: int", rand_name(rng)))
        .collect();
    let params_str = params.join(", ");

    let mut body = String::new();
    for _ in 0..num_stmts {
        body.push_str(&rand_stmt(rng, "    "));
        body.push('\n');
    }

    format!("def graph({}):\n{}", params_str, body)
}

// ─── Fuzz tests ────────────────────────────────────────────────────

fn run_fuzz(seed: u64, num_graphs: usize, stmts_per_graph: usize) {
    let mut rng = SimpleRng::new(seed);

    let mut ok_count = 0;
    let mut err_count = 0;
    let mut panic_count = 0;

    for _ in 0..num_graphs {
        let code = rand_graph(&mut rng, stmts_per_graph);

        // Test sanitizer
        let sanitize_result = std::panic::catch_unwind(|| sanitizer::sanitize(&code));
        match sanitize_result {
            Ok(_) => {}
            Err(_) => {
                panic_count += 1;
                continue;
            }
        }

        // Test transform (includes sanitize internally)
        let transform_result = std::panic::catch_unwind(|| transform::transform(&code));
        match transform_result {
            Ok(Ok(plan)) => {
                // Test validator
                let validate_result = std::panic::catch_unwind(|| validator::validate(&plan));
                match validate_result {
                    Ok(_) => {
                        // Test codegen
                        let codegen_result =
                            std::panic::catch_unwind(|| backend::codegen::codegen(plan));
                        match codegen_result {
                            Ok(_) => ok_count += 1,
                            Err(_) => panic_count += 1,
                        }
                    }
                    Err(_) => panic_count += 1,
                }
            }
            Ok(Err(_)) => err_count += 1, // Expected: code may be invalid
            Err(_) => panic_count += 1,
        }
    }

    let total = ok_count + err_count + panic_count;
    println!(
        "  seed {}: {} ok, {} errors, {} panics (total {})",
        seed, ok_count, err_count, panic_count, total
    );

    assert_eq!(
        panic_count, 0,
        "Fuzz seed {} produced {} panics!",
        seed, panic_count
    );
}

#[test]
fn test_fuzz_small() {
    run_fuzz(42, 100, 3); // seed 42, 100 graphs, 3 stmts each
}

#[test]
fn test_fuzz_medium() {
    run_fuzz(123, 50, 5);
}

#[test]
fn test_fuzz_large() {
    run_fuzz(456, 25, 8);
}
