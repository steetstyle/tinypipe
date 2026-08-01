use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::{Edge, ExecutionPlan, Node, Opcode};
use tinypipe_vm::CompiledExecutor;

fn main() {
    let plan = ExecutionPlan::new(
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
            Edge::new("loop1", "output"),
        ],
    );
    let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
    let reg = tinypipe_tools::mock_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);
    let mut c = Context::new();
    c.set("x".into(), Value::Int(0));
    let result = exec.execute(c);
    match result {
        Ok(r) => println!(
            "RESULT: output={:?} order={:?} vars={:?}",
            r.output, r.execution_order, r.context.variables
        ),
        Err(e) => println!("ERR: {:?}", e),
    }
}
