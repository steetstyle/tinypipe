//! `for` içinde `if/elif/else` + `break`/`continue` entegrasyon testleri.
//!
//! Her test: kodu transform'dan geçirip (end-to-end), VM'de çalıştırır ve
//! sonucu doğrular. Plan yapısını elle kurmaz — transform değişirse testler
//! de doğal olarak takip eder.

use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_vm::CompiledExecutor;

fn run(code: &str, json: &str) -> Result<Value, String> {
    let plan = tinypipe_compiler::transform::transform(code).map_err(|errs| {
        errs.iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
    let reg = tinypipe_tools::tools::default_tools();
    let exec = CompiledExecutor::new(&compiled, &reg);
    let vars: std::collections::HashMap<String, Value> =
        serde_json::from_value(serde_json::Value::Object(
            serde_json::from_str(json).map_err(|e| e.to_string())?,
        ))
        .map_err(|e| e.to_string())?;
    let mut ctx = Context::new();
    for (k, v) in vars {
        ctx.set(k, v);
    }
    let res = exec.execute(ctx).map_err(|e| format!("{e:?}"))?;
    res.output.ok_or_else(|| "no output".to_string())
}

fn items(json: &str) -> String {
    json.to_string()
}

#[test]
fn if_else_in_loop() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] > 0:
            total = total + items[i]
        else:
            total = total + 1
    return total"#;
    let out = run(code, &items(r#"{"items": [1, -2, 3, 4]}"#)).unwrap();
    assert_eq!(out, Value::Int(9));
}

#[test]
fn else_less_if_in_loop() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] > 0:
            total = total + items[i]
    return total"#;
    let out = run(code, &items(r#"{"items": [1, -2, 3, 4]}"#)).unwrap();
    assert_eq!(out, Value::Int(8));
}

#[test]
fn break_in_loop() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] < 0:
            break
        total = total + items[i]
    return total"#;
    assert_eq!(run(code, &items(r#"{"items": [1, 2, -5, 3]}"#)).unwrap(), Value::Int(3));
    assert_eq!(run(code, &items(r#"{"items": [-1, 2]}"#)).unwrap(), Value::Int(0));
}

#[test]
fn continue_in_loop() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] < 0:
            continue
        total = total + items[i]
    return total"#;
    let out = run(code, &items(r#"{"items": [1, -5, 2, -9, 3]}"#)).unwrap();
    assert_eq!(out, Value::Int(6));
}

#[test]
fn elif_in_loop() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] > 10:
            total = total + 2
        elif items[i] > 0:
            total = total + 1
        else:
            total = total
    return total"#;
    assert_eq!(run(code, &items(r#"{"items": [20, 5, -3, 11]}"#)).unwrap(), Value::Int(5));
    assert_eq!(run(code, &items(r#"{"items": [20]}"#)).unwrap(), Value::Int(2));
    assert_eq!(run(code, &items(r#"{"items": [5]}"#)).unwrap(), Value::Int(1));
    assert_eq!(run(code, &items(r#"{"items": [-3]}"#)).unwrap(), Value::Int(0));
}

#[test]
fn all_terminal_elif_branches() {
    let code = r#"def graph(x):
    if x > 10:
        return "big"
    elif x > 0:
        return "mid"
    else:
        return "small""#;
    assert_eq!(run(code, r#"{"x": 85}"#).unwrap(), Value::String("big".into()));
    assert_eq!(run(code, r#"{"x": 5}"#).unwrap(), Value::String("mid".into()));
    assert_eq!(run(code, r#"{"x": -5}"#).unwrap(), Value::String("small".into()));
}

#[test]
fn top_level_else_less_if() {
    let code = r#"def graph(x):
    y = 10
    if x > 5:
        y = x * 2
    return y"#;
    assert_eq!(run(code, r#"{"x": 3}"#).unwrap(), Value::Int(10));
    assert_eq!(run(code, r#"{"x": 8}"#).unwrap(), Value::Int(16));
}

#[test]
fn empty_loop_still_runs_return() {
    let code = r#"def graph(items):
    total = 0
    for i in range(len(items)):
        if items[i] > 0:
            total = total + items[i]
    return total"#;
    assert_eq!(run(code, &items(r#"{"items": []}"#)).unwrap(), Value::Int(0));
}
