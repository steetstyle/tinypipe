//! Codegen integration tests — end-to-end: Restricted Python → parse → sanitize → transform → validate → codegen → CompiledPlan.
//!
//! These tests verify the full compiler pipeline produces valid, executable plans.

use tinypipe_compiler::{backend, transform, validator};

/// Helper: transform + validate + codegen a Python snippet, returning the codegen output.
fn compile(code: &str) -> Result<backend::codegen::CodegenOutput, String> {
    // Transform (parse + sanitize + transform)
    let plan = transform::transform(code).map_err(|e| format!("transform errors: {:?}", e))?;

    // Validate
    validator::validate(&plan).map_err(|e| format!("validation errors: {:?}", e))?;

    // Codegen (v2: CompiledPlan + FlatBuffers binary)
    backend::codegen::codegen(plan).map_err(|e| e.message)
}

#[test]
fn test_e2e_simple_graph() {
    let code = "def graph(x: int):\n    return x";
    let output = compile(code).expect("simple graph should compile");
    assert_eq!(output.execution_order.len(), 3);
    assert!(output.binary_size_bytes > 0);
    assert!(!output.fb_binary.is_empty());
}

#[test]
fn test_e2e_arithmetic_graph() {
    let code = "def graph(a: int, b: int):\n    c = a + b\n    return c";
    let output = compile(code).expect("arithmetic graph should compile");
    assert!(!output.execution_order.is_empty());
    assert!(output.binary_size_bytes > 0);
    // Verify the FlatBuffers binary round-trips
    let fb_roundtrip = tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&output.fb_binary)
        .expect("FlatBuffers should deserialize");
    assert_eq!(output.compiled, fb_roundtrip);
}

#[test]
fn test_e2e_if_else_graph() {
    let code =
        "def graph(x: int):\n    if x > 0:\n        y = 1\n    else:\n        y = 2\n    return y";
    let output = compile(code).expect("if-else graph should compile");
    assert!(output.execution_order.len() >= 5);
    assert!(output.compiled.metadata.node_count >= 5);
}

#[test]
fn test_e2e_fb_roundtrip() {
    let code = "def graph(x: int):\n    return x";
    let output = compile(code).expect("should compile");
    let fb_deser = tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&output.fb_binary).unwrap();
    assert_eq!(output.compiled, fb_deser);
}

#[test]
fn test_e2e_fb_binary_produced() {
    let code = "def graph(x: int):\n    return x";
    let output = compile(code).expect("should compile");
    assert!(!output.fb_binary.is_empty(), "FB binary should be produced");
}

#[test]
fn test_e2e_rejects_invalid_code() {
    let code = "def graph():\n    import os\n    return 1";
    let result = compile(code);
    assert!(result.is_err(), "should reject code with import");
}

#[test]
fn test_e2e_call_tool_graph() {
    let code = r#"def graph(x: int):
    result = call("my_tool", arg=x)
    return result"#;
    let output = compile(code).expect("tool call graph should compile");
    assert!(output.execution_order.len() >= 4);
    assert!(output.compiled.metadata.node_count >= 4);
}
