//! Type checking — expression type inference + type validation.
//!
//! # Pipeline Integration
//!
//! Called from the compile pipeline after transform and before codegen.
//! Errors are collected and reported to the user (or LLM for auto-repair).

use tinypipe_ir::plan::{Node, Opcode, Type};
use tinypipe_ir::ArgValue;

/// Type check result: either OK or a list of type errors.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub node_id: String,
    pub message: String,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node '{}': {}", self.node_id, self.message)
    }
}

/// Run the full type-check pipeline on a list of plan nodes.
///
/// Returns a list of type errors (may be empty).
pub fn check_types(nodes: &[Node]) -> Vec<TypeError> {
    let mut errors = Vec::new();

    // First pass: infer types for all nodes
    let mut types: std::collections::HashMap<&str, Type> = std::collections::HashMap::new();
    for node in nodes {
        let inferred = infer_node_type(node);
        types.insert(&node.id, inferred.clone());
    }

    // Second pass: validate based on inferred types
    for node in nodes {
        if let Err(msg) = validate_node(node, &types) {
            errors.push(TypeError {
                node_id: node.id.clone(),
                message: msg,
            });
        }
    }

    errors
}

/// Infer the output type of a single node.
pub fn infer_node_type(node: &Node) -> Type {
    match node.op {
        Opcode::Input => {
            // INPUT may have a type annotation in args
            node.args
                .iter()
                .find(|a| a.key == "type")
                .and_then(|a| match &a.value {
                    ArgValue::String(s) => parse_type_name(s),
                    _ => None,
                })
                .unwrap_or(Type::Any)
        }
        Opcode::Calc => {
            let expr = node
                .args
                .iter()
                .find(|a| a.key == "expr")
                .and_then(|a| match &a.value {
                    ArgValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            infer_expr_type(expr)
        }
        Opcode::Call => {
            // CALL output type depends on the tool; without schema info we use Any
            // Future: look up tool_deps for output_schema
            Type::Any
        }
        Opcode::Decide | Opcode::Switch => Type::Bool,
        Opcode::Act => Type::Any,
        Opcode::Parallel => Type::Any,
        Opcode::Loop => Type::Any,
        Opcode::Wait => Type::Any,
        Opcode::Merge => Type::Any,
        Opcode::Error => Type::Null,
    }
}

/// Validate a single node's type constraints.
fn validate_node(node: &Node, types: &std::collections::HashMap<&str, Type>) -> Result<(), String> {
    match node.op {
        Opcode::Calc => validate_calc(node),
        Opcode::Decide => validate_decide(node, types),
        Opcode::Switch => validate_switch(node),
        _ => Ok(()),
    }
}

/// Validate a CALC expression for type errors.
fn validate_calc(node: &Node) -> Result<(), String> {
    let expr = match node
        .args
        .iter()
        .find(|a| a.key == "expr")
        .and_then(|a| match &a.value {
            ArgValue::String(s) => Some(s.as_str()),
            _ => None,
        }) {
        Some(e) => e,
        None => return Ok(()),
    };

    // Check for string operations that don't make sense
    let has_string = has_string_literal(expr);
    let has_numeric_op = expr.contains('/') || expr.contains('*') || expr.contains('%');

    if has_string && has_numeric_op && !expr.contains('+') {
        // String / number or string * number without +
        // Note: string + number is potentially OK (concatenation)
        return Err(format!(
            "Type error in expression '{}': string and number operation is not allowed (use + for concatenation)",
            expr
        ));
    }

    // Check for boolean arithmetic
    let has_bool = expr.contains("true") || expr.contains("false");
    if has_bool && (expr.contains('/') || expr.contains('*') || expr.contains('-')) {
        return Err(format!(
            "Type error in expression '{}': arithmetic on boolean values is not allowed",
            expr
        ));
    }

    Ok(())
}

/// Validate a DECIDE node: source and value should be comparable.
fn validate_decide(
    node: &Node,
    _types: &std::collections::HashMap<&str, Type>,
) -> Result<(), String> {
    let source = node.args.iter().find(|a| a.key == "source");
    let value = node.args.iter().find(|a| a.key == "value");

    if let (Some(src), Some(val)) = (source, value) {
        let source_str = match &src.value {
            ArgValue::String(s) => s.as_str(),
            _ => return Ok(()),
        };
        let val_str = match &val.value {
            ArgValue::String(s) => s.as_str(),
            _ => return Ok(()),
        };

        // If source is a string (has quotes) and value is numeric, warn
        let source_is_string = has_string_literal(source_str);
        let val_is_numeric = val_str.chars().all(|c| c.is_ascii_digit() || c == '.');

        if source_is_string && val_is_numeric {
            return Err(format!(
                "Type warning in DECIDE: comparing string '{}' with numeric value '{}'",
                source_str, val_str,
            ));
        }
    }

    Ok(())
}

/// Validate a SWITCH node's case values.
fn validate_switch(_node: &Node) -> Result<(), String> {
    // SWITCH source type should be compatible with case conditions
    // For now, just basic checks
    Ok(())
}

/// Infer the type of a CALC expression string.
fn infer_expr_type(expr: &str) -> Type {
    let trimmed = expr.trim();

    // Empty expression
    if trimmed.is_empty() {
        return Type::Any;
    }

    // String literal (quoted)
    if has_string_literal(trimmed) {
        return Type::String;
    }

    // True/False constants
    if trimmed == "true" || trimmed == "false" {
        return Type::Bool;
    }

    // Null/None constant
    if trimmed == "null" || trimmed == "none" || trimmed == "None" {
        return Type::Null;
    }

    // Numeric: division results in float
    if trimmed.contains('/') {
        return Type::Float;
    }

    // Pure numeric literal
    if trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == '.')
    {
        if trimmed.contains('.') {
            return Type::Float;
        }
        return Type::Int;
    }

    // Contains arithmetic operators
    if trimmed.contains('+') || trimmed.contains('-') || trimmed.contains('*') {
        // If all operands are numeric, return appropriate type
        return Type::Int;
    }

    // Comparison operators → Bool
    if trimmed.contains("==")
        || trimmed.contains("!=")
        || trimmed.contains(">=")
        || trimmed.contains("<=")
        || trimmed.contains(">")
        || trimmed.contains("<")
    {
        return Type::Bool;
    }

    // Fallback
    Type::Any
}

/// Check if an expression contains a string literal.
///
/// Returns true if there's at least one unescaped quote character,
/// indicating the presence of a string literal in the expression.
fn has_string_literal(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            // Check if escaped by preceding backslash
            if i == 0 || bytes[i - 1] != b'\\' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Parse a type name string into a Type enum.
fn parse_type_name(s: &str) -> Option<Type> {
    match s.trim().to_lowercase().as_str() {
        "string" | "str" => Some(Type::String),
        "int" | "integer" => Some(Type::Int),
        "float" | "double" => Some(Type::Float),
        "bool" | "boolean" => Some(Type::Bool),
        "null" | "none" => Some(Type::Null),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc_node(id: &str, expr: &str) -> Node {
        Node::new(id, Opcode::Calc).with_arg("expr", ArgValue::String(expr.into()))
    }

    #[test]
    fn test_check_no_errors() {
        let nodes = vec![
            calc_node("n1", "x + 1"),
            Node::new("n2", Opcode::Act).with_arg("type", ArgValue::String("return".into())),
        ];
        let errors = check_types(&nodes);
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
    }

    #[test]
    fn test_check_string_division() {
        let nodes = vec![calc_node("n1", "\"hello\" / 2")];
        let errors = check_types(&nodes);
        assert!(
            !errors.is_empty(),
            "expected type error for string division"
        );
        assert!(errors[0].message.contains("Type error"));
        assert!(errors[0].message.contains("string"));
    }

    #[test]
    fn test_check_string_multiplication() {
        let nodes = vec![calc_node("n1", "\"hello\" * 3")];
        let errors = check_types(&nodes);
        // String * int is debatable — but our rules disallow it
        assert!(
            !errors.is_empty(),
            "expected type error for string multiplication"
        );
    }

    #[test]
    fn test_check_bool_arithmetic() {
        let nodes = vec![calc_node("n1", "true / false")];
        let errors = check_types(&nodes);
        assert!(
            !errors.is_empty(),
            "expected type error for bool arithmetic"
        );
        assert!(errors[0].message.contains("boolean"));
    }

    #[test]
    fn test_check_string_concat_ok() {
        // string + string is OK
        let nodes = vec![calc_node("n1", "\"hello\" + \" world\"")];
        let errors = check_types(&nodes);
        assert!(
            errors.is_empty(),
            "expected no error for string concat, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_infer_type_calc_int() {
        assert_eq!(infer_node_type(&calc_node("n1", "1 + 2")), Type::Int);
    }

    #[test]
    fn test_infer_type_calc_float() {
        assert_eq!(infer_node_type(&calc_node("n1", "10 / 3")), Type::Float);
    }

    #[test]
    fn test_infer_type_calc_float_literal() {
        assert_eq!(infer_node_type(&calc_node("n1", "3.14")), Type::Float);
    }

    #[test]
    fn test_infer_type_calc_string() {
        assert_eq!(infer_node_type(&calc_node("n1", "\"hello\"")), Type::String);
    }

    #[test]
    fn test_infer_type_calc_bool() {
        assert_eq!(infer_node_type(&calc_node("n1", "true")), Type::Bool);
    }

    #[test]
    fn test_infer_type_calc_comparison() {
        assert_eq!(infer_node_type(&calc_node("n1", "x > 10")), Type::Bool);
    }

    #[test]
    fn test_infer_type_decide() {
        let node = Node::new("d1", Opcode::Decide)
            .with_arg("source", ArgValue::String("$x".into()))
            .with_arg("value", ArgValue::String("10".into()));
        assert_eq!(infer_node_type(&node), Type::Bool);
    }

    #[test]
    fn test_infer_type_input() {
        let node = Node::new("i1", Opcode::Input);
        assert_eq!(infer_node_type(&node), Type::Any);
    }

    #[test]
    fn test_infer_type_input_annotated() {
        let node = Node::new("i1", Opcode::Input)
            .with_arg("name", ArgValue::String("x".into()))
            .with_arg("type", ArgValue::String("int".into()));
        assert_eq!(infer_node_type(&node), Type::Int);
    }

    #[test]
    fn test_infer_type_error() {
        let node = Node::new("e1", Opcode::Error);
        assert_eq!(infer_node_type(&node), Type::Null);
    }

    #[test]
    fn test_has_string_literal() {
        assert!(has_string_literal("\"hello\" + 1"));
        assert!(has_string_literal("x + 'world'"));
        assert!(!has_string_literal("x + 1"));
        assert!(!has_string_literal(""));
    }

    #[test]
    fn test_parse_type_name() {
        assert_eq!(parse_type_name("int"), Some(Type::Int));
        assert_eq!(parse_type_name("string"), Some(Type::String));
        assert_eq!(parse_type_name("bool"), Some(Type::Bool));
        assert_eq!(parse_type_name("float"), Some(Type::Float));
        assert_eq!(parse_type_name("unknown"), None);
    }

    #[test]
    fn test_decide_string_numeric_warning() {
        let nodes = vec![Node::new("d1", Opcode::Decide)
            .with_arg("source", ArgValue::String("\"hello\"".into()))
            .with_arg("value", ArgValue::String("42".into()))
            .with_arg("op", ArgValue::String("eq".into()))];
        let errors = check_types(&nodes);
        assert!(
            !errors.is_empty(),
            "expected warning for string vs numeric compare"
        );
    }

    #[test]
    fn test_decide_numeric_ok() {
        let nodes = vec![Node::new("d1", Opcode::Decide)
            .with_arg("source", ArgValue::String("$x".into()))
            .with_arg("value", ArgValue::String("42".into()))
            .with_arg("op", ArgValue::String("eq".into()))];
        let errors = check_types(&nodes);
        assert!(errors.is_empty(), "expected no error, got: {:?}", errors);
    }
}
