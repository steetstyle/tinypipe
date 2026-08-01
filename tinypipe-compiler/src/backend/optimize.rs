//! Optimization passes for the compiler backend.
//!
//! These transform a validated `ExecutionPlan` to produce more efficient code:
//!
//! - **Constant folding**: Evaluate CALC nodes with only constant inputs at compile time.
//! - **Dead node elimination**: Remove pure nodes whose outputs are never referenced.
//! - **Calc fusion**: Merge consecutive CALC nodes into one (v2).
//! - **Multi-branch fusion**: Merge identical CALL nodes from parallel branches (v2).

use std::collections::{HashMap, HashSet};

use tinypipe_ir::plan::{ArgValue, ExecutionPlan, Node, Opcode};

/// List of optimization names applied.
#[derive(Debug, Clone)]
pub struct OptimizationResult {
    pub plan: ExecutionPlan,
    pub optimizations_applied: Vec<String>,
}

/// Run all optimization passes in order.
pub fn optimize_all(plan: ExecutionPlan) -> OptimizationResult {
    let mut optimizations = Vec::new();

    // Pass 1: Constant folding
    let (plan, folded) = constant_folding(plan);
    if folded > 0 {
        optimizations.push(format!("constant_folding:{}", folded));
    }

    // Pass 2: Dead node elimination
    let (plan, dead) = dead_node_elimination(plan);
    if dead > 0 {
        optimizations.push(format!("dead_node_elimination:{}", dead));
    }

    // Pass 3: Calc fusion — inline consecutive CALC nodes
    let (plan, fused) = calc_fusion(plan);
    if fused > 0 {
        optimizations.push(format!("calc_fusion:{}", fused));
    }

    // Pass 4: Multi-branch fusion — merge identical CALLs in parallel branches
    let (plan, merged) = multi_branch_fusion(plan);
    if merged > 0 {
        optimizations.push(format!("multi_branch_fusion:{}", merged));
    }

    OptimizationResult {
        plan,
        optimizations_applied: optimizations,
    }
}

// ─── Constant Folding ───────────────────────────────────────────────

/// Evaluate CALC nodes whose expression references only constant values.
///
/// A CALC node like `a = 3 + 5` (where 3 and 5 are ArgValue::Int literals
/// and not context variables) can be folded to `a = 8`.
///
/// Returns (optimized_plan, number_of_folded_nodes).
fn constant_folding(plan: ExecutionPlan) -> (ExecutionPlan, usize) {
    let mut folded = 0usize;
    let mut nodes: Vec<Node> = plan.nodes;
    let edges = plan.edges;

    // Phase 1: Find all INPUT nodes that HAVE a constant default value
    let mut constant_values: HashMap<String, ArgValue> = HashMap::new();

    for node in &nodes {
        if node.op == Opcode::Input {
            // INPUT with a constant "default" arg can be folded
            if let Some(default) = node.args.iter().find(|a| a.key == "default") {
                constant_values.insert(node.id.clone(), default.value.clone());
            }
        }
    }

    // Phase 2: Find CALC nodes whose expression refers only to constants
    // Simple heuristic: an expression is "constant" if it doesn't reference
    // any context variable (no `$` prefix in v2 — in v1 just look for
    // pure-literal expressions).
    for node in &mut nodes {
        if node.op == Opcode::Calc {
            let expr = node.args.iter().find(|a| a.key == "expr").map(|a| &a.value);

            if let Some(ArgValue::String(expr_str)) = expr {
                // Simple constant detection: if the expression contains
                // only digits, operators, and whitespace → it's constant
                if is_constant_expression(expr_str) {
                    // Try to evaluate simple integer expressions
                    if let Some(result) = eval_simple_int_expr(expr_str) {
                        // Use Arg::new instead of .into() tuple
                        let arg = tinypipe_ir::plan::Arg::new(
                            "expr",
                            ArgValue::String(format!("{}", result)),
                        );
                        node.args = vec![arg];
                        folded += 1;
                    }
                }
            }
        }
    }

    (
        ExecutionPlan {
            version: plan.version,
            nodes,
            edges,
            metadata: plan.metadata,
        },
        folded,
    )
}

/// Check if an expression string contains only constants.
fn is_constant_expression(expr: &str) -> bool {
    // Constants: digits, operators, whitespace, parens
    // No alphabetic characters (those are variable references)
    !expr.contains(|c: char| c.is_ascii_alphabetic())
}

/// Evaluate simple integer arithmetic expressions.
/// Supports: +, -, *, / with integer operands.
fn eval_simple_int_expr(expr: &str) -> Option<i64> {
    // Tokenize: split by operators
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    // Try to evaluate as a simple expression
    // Strategy: try to parse as a single number first
    if let Ok(n) = expr.parse::<i64>() {
        return Some(n);
    }

    // Simple binary operations: "a + b", "a - b", "a * b", "a / b"
    let ops: Vec<(char, Box<dyn Fn(i64, i64) -> Option<i64>>)> = vec![
        ('+', Box::new(|a, b| a.checked_add(b))),
        ('-', Box::new(|a, b| a.checked_sub(b))),
        ('*', Box::new(|a, b| a.checked_mul(b))),
        (
            '/',
            Box::new(|a, b| if b != 0 { a.checked_div(b) } else { None }),
        ),
    ];
    for (op_char, op_fn) in &ops {
        if let Some(pos) = expr.find(*op_char) {
            if pos == 0 || pos == expr.len() - 1 {
                continue; // unary or trailing
            }
            let left = expr[..pos].trim();
            let right = expr[pos + 1..].trim();
            if let (Ok(a), Ok(b)) = (left.parse::<i64>(), right.parse::<i64>()) {
                if let Some(result) = op_fn(a, b) {
                    return Some(result);
                }
            }
        }
    }

    None
}

// ─── Dead Node Elimination ──────────────────────────────────────────

/// Remove nodes that are:
/// - `pure = true` (no side effects)
/// - Not referenced by any edge
/// - Not an INPUT node
///
/// Returns (optimized_plan, number_of_removed_nodes).
fn dead_node_elimination(plan: ExecutionPlan) -> (ExecutionPlan, usize) {
    // Build a set of referenced node IDs (from edges)
    let mut referenced: HashSet<&str> = HashSet::new();
    for edge in &plan.edges {
        referenced.insert(edge.from.as_str());
        referenced.insert(edge.to.as_str());
    }

    // Also, INPUT nodes and IMPURE nodes are always kept
    let mut removed = 0usize;
    let mut kept_nodes: Vec<Node> = Vec::with_capacity(plan.nodes.len());

    for node in plan.nodes.into_iter() {
        let is_impure = !node.op.is_pure();
        let is_input = node.op == Opcode::Input;
        let is_referenced = referenced.contains(node.id.as_str());

        let live = is_input || is_impure || is_referenced;
        if live {
            kept_nodes.push(node);
        } else {
            removed += 1;
        }
    }

    // If no nodes were removed, return unchanged
    if removed == 0 {
        return (
            ExecutionPlan {
                version: plan.version,
                nodes: kept_nodes,
                edges: plan.edges,
                metadata: plan.metadata,
            },
            0,
        );
    }

    // Rebuild edges, filtering out references to removed nodes
    let kept_ids: HashSet<&str> = kept_nodes.iter().map(|n| n.id.as_str()).collect();
    let kept_edges: Vec<_> = plan
        .edges
        .into_iter()
        .filter(|e| kept_ids.contains(e.from.as_str()) && kept_ids.contains(e.to.as_str()))
        .collect();

    let new_plan = ExecutionPlan {
        version: plan.version,
        nodes: kept_nodes,
        edges: kept_edges,
        metadata: plan.metadata,
    };

    (new_plan, removed)
}

// ─── Calc Fusion ─────────────────────────────────────────────────

/// Merge consecutive CALC nodes by inlining definitions.
///
/// If `calc1` produces variable `x` (via `output: "x"`), and `calc2`
/// references `x` in its expression, and `calc1`'s output is only used
/// by `calc2`, we inline `calc1`'s expression into `calc2`'s expression
/// and remove `calc1`.
///
/// Returns (optimized_plan, number_of_fused_pairs).
fn calc_fusion(plan: ExecutionPlan) -> (ExecutionPlan, usize) {
    let mut fused = 0usize;
    let mut nodes = plan.nodes;
    let edges = plan.edges;
    let mut changed = true;

    // Iterative inlining: after one inlining, variable references may change
    while changed {
        changed = false;

        // Rebuild maps from current nodes
        let mut var_to_node: HashMap<String, &Node> = HashMap::new();
        for node in &nodes {
            if node.op == Opcode::Calc {
                if let Some(output) = node.args.iter().find(|a| a.key == "output") {
                    if let ArgValue::String(name) = &output.value {
                        var_to_node.insert(name.clone(), node);
                    }
                }
            }
        }

        // Build consumer counts from edges (considering only kept nodes)
        let kept_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let mut consumer_count: HashMap<&str, usize> = HashMap::new();
        for edge in &edges {
            if kept_ids.contains(edge.from.as_str()) && kept_ids.contains(edge.to.as_str()) {
                *consumer_count.entry(edge.from.as_str()).or_insert(0) += 1;
            }
        }

        let mut to_remove: Vec<String> = Vec::new();
        let mut new_nodes: Vec<Node> = Vec::new();

        for node in &nodes {
            if to_remove.contains(&node.id) {
                continue;
            }

            if node.op != Opcode::Calc {
                new_nodes.push(node.clone());
                continue;
            }

            let expr = node.args.iter().find(|a| a.key == "expr").map(|a| &a.value);

            let (should_update, new_expr_val) = match expr {
                Some(ArgValue::String(expr_str))
                    if !expr_str.is_empty() && !is_constant_expression(expr_str) =>
                {
                    let vars = extract_variables(expr_str);
                    let mut inlined = expr_str.clone();
                    let mut local_changed = false;

                    for var in &vars {
                        if let Some(&def_node) = var_to_node.get(var) {
                            if def_node.op != Opcode::Calc {
                                continue;
                            }
                            let consumer_val = consumer_count
                                .get(def_node.id.as_str())
                                .copied()
                                .unwrap_or(0);
                            if consumer_val != 1 {
                                continue;
                            }
                            let is_consumer = edges
                                .iter()
                                .any(|e| e.from == def_node.id && e.to == node.id);
                            if !is_consumer {
                                continue;
                            }
                            let def_expr = def_node
                                .args
                                .iter()
                                .find(|a| a.key == "expr")
                                .map(|a| &a.value);
                            if let Some(ArgValue::String(def_str)) = def_expr {
                                let def_vars = extract_variables(def_str);
                                if def_vars.contains(var) {
                                    continue;
                                }
                                // Replace var with (def_expr) — careful replacement
                                let new_inlined = replace_var_in_expr(&inlined, var, def_str);
                                if new_inlined != inlined {
                                    inlined = new_inlined;
                                    local_changed = true;
                                    if !to_remove.contains(&def_node.id) {
                                        to_remove.push(def_node.id.clone());
                                    }
                                }
                            }
                        }
                    }

                    (local_changed, inlined)
                }
                _ => (false, String::new()),
            };

            if should_update {
                let mut new_node = node.clone();
                if let Some(arg) = new_node.args.iter_mut().find(|a| a.key == "expr") {
                    arg.value = ArgValue::String(new_expr_val);
                }
                new_nodes.push(new_node);
                changed = true;
            } else {
                new_nodes.push(node.clone());
            }
        }

        if changed {
            nodes = new_nodes
                .into_iter()
                .filter(|n| !to_remove.contains(&n.id))
                .collect();
            fused += to_remove.len();
        }
    }

    // Filter edges to remove references to fused-away nodes
    let kept_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let kept_edges: Vec<_> = edges
        .into_iter()
        .filter(|e| kept_ids.contains(e.from.as_str()) && kept_ids.contains(e.to.as_str()))
        .collect();

    (
        ExecutionPlan {
            version: plan.version,
            nodes,
            edges: kept_edges,
            metadata: plan.metadata,
        },
        fused,
    )
}

/// Replace a variable reference with a parenthesized expression.
/// Handles word-boundary replacement to avoid partial matches.
fn replace_var_in_expr(expr: &str, var: &str, replacement: &str) -> String {
    // Try full match (expression is just the variable)
    if expr.trim() == var {
        return format!("({})", replacement);
    }

    let mut result = String::new();
    let mut rest = expr;

    loop {
        // Find the variable as a whole word
        let before;
        let after;
        let start = rest.find(var);
        match start {
            None => {
                result.push_str(rest);
                break;
            }
            Some(pos) => {
                // Check word boundary before
                if pos > 0 {
                    let prev = rest.as_bytes()[pos - 1];
                    if prev.is_ascii_alphanumeric() || prev == b'_' {
                        // Not a word boundary — skip
                        result.push_str(&rest[..=pos]);
                        rest = &rest[pos + 1..];
                        continue;
                    }
                }
                // Check word boundary after
                let end = pos + var.len();
                let is_word_boundary = if end >= rest.len() {
                    true
                } else {
                    let next = rest.as_bytes()[end];
                    !(next.is_ascii_alphanumeric() || next == b'_')
                };

                if is_word_boundary {
                    before = &rest[..pos];
                    after = &rest[end..];
                    result.push_str(before);
                    result.push('(');
                    result.push_str(replacement);
                    result.push(')');
                    rest = after;
                } else {
                    result.push_str(&rest[..=pos]);
                    rest = &rest[pos + 1..];
                }
            }
        }
    }

    result
}

/// Extract variable names from an expression string.
/// Variables are alphabetic sequences (not purely numeric).
fn extract_variables(expr: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut current = String::new();
    for ch in expr.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            if !current.is_empty() && !current.chars().all(|c| c.is_ascii_digit()) {
                vars.push(current.clone());
            }
            current.clear();
        }
    }
    if !current.is_empty() && !current.chars().all(|c| c.is_ascii_digit()) {
        vars.push(current);
    }
    vars
}

// ─── Multi-Branch Fusion ─────────────────────────────────────────

/// Merge identical CALL nodes within parallel branches.
///
/// If a PARALLEL node has multiple branches that each call the same
/// tool with the same arguments, remove duplicates and redirect edges.
///
/// Returns (optimized_plan, number_of_merged_calls).
fn multi_branch_fusion(plan: ExecutionPlan) -> (ExecutionPlan, usize) {
    let mut nodes = plan.nodes;
    let edges = plan.edges;

    let mut merged = 0usize;
    let mut to_remove: Vec<String> = Vec::new();
    let mut edge_redirects: Vec<(String, String)> = Vec::new();

    // Find all PARALLEL nodes
    let parallel_ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.op == Opcode::Parallel)
        .map(|n| n.id.clone())
        .collect();

    for par_id in &parallel_ids {
        // Find children of this PARALLEL
        let children: Vec<&str> = edges
            .iter()
            .filter(|e| e.from == *par_id)
            .map(|e| e.to.as_str())
            .collect();

        // Among children, find CALL nodes and group by signature
        let mut call_signatures: HashMap<String, Vec<String>> = HashMap::new();
        for child_id in &children {
            if let Some(child) = nodes.iter().find(|n| n.id == *child_id) {
                if child.op != Opcode::Call {
                    continue;
                }
                let mut sig_parts: Vec<String> = Vec::new();
                for arg in &child.args {
                    sig_parts.push(format!("{}:{}", arg.key, arg_value_str(&arg.value)));
                }
                sig_parts.sort();
                let sig = sig_parts.join("|");
                call_signatures
                    .entry(sig)
                    .or_default()
                    .push(child_id.to_string());
            }
        }

        // Merge duplicates (keep first, remove rest)
        for (_sig, ids) in &call_signatures {
            if ids.len() <= 1 {
                continue;
            }
            let keep = &ids[0];
            for remove in &ids[1..] {
                to_remove.push(remove.clone());
                edge_redirects.push((remove.clone(), keep.clone()));
                merged += 1;
            }
        }
    }

    if merged == 0 {
        return (
            plan_from_parts(plan.version, nodes, edges, plan.metadata),
            0,
        );
    }

    // Remove merged nodes
    nodes.retain(|n| !to_remove.contains(&n.id));

    // Redirect edges to kept node
    let kept_edges: Vec<_> = edges
        .into_iter()
        .map(|e| {
            let from = redirect_id(&e.from, &edge_redirects);
            let to = redirect_id(&e.to, &edge_redirects);
            tinypipe_ir::plan::Edge {
                from,
                to,
                condition: e.condition,
                mapping: e.mapping,
                priority: e.priority,
                label: e.label,
                kind: e.kind,
            }
        })
        .collect();

    (
        plan_from_parts(plan.version, nodes, kept_edges, plan.metadata),
        merged,
    )
}

fn redirect_id(id: &str, redirects: &[(String, String)]) -> String {
    for (old, new) in redirects {
        if id == old {
            return new.clone();
        }
    }
    id.to_string()
}

fn plan_from_parts(
    version: u16,
    nodes: Vec<Node>,
    edges: Vec<tinypipe_ir::plan::Edge>,
    metadata: tinypipe_ir::plan::Metadata,
) -> ExecutionPlan {
    ExecutionPlan {
        version,
        nodes,
        edges,
        metadata,
    }
}

/// Format an ArgValue as a string for signature computation.
fn arg_value_str(val: &ArgValue) -> String {
    match val {
        ArgValue::String(s) => format!("\"{}\"", s),
        ArgValue::Int(i) => format!("{}", i),
        ArgValue::Bool(b) => format!("{}", b),
        ArgValue::Float(f) => format!("{}", f),
        ArgValue::Null => "null".to_string(),
        ArgValue::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(arg_value_str).collect();
            format!("[{}]", inner.join(","))
        }
        ArgValue::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}:{}", k, arg_value_str(v)))
                .collect();
            let mut sorted = inner;
            sorted.sort();
            format!("{{{}}}", sorted.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_ir::plan::{Edge, Node};

    #[test]
    fn test_constant_folding_simple() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc).with_arg("expr", "3 + 5".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![Edge::new("input1", "calc1"), Edge::new("calc1", "act1")],
        );
        let result = optimize_all(plan);
        assert!(result
            .optimizations_applied
            .iter()
            .any(|o| o.starts_with("constant_folding")));
        // The folded calc should now have expression "8"
        let calc_node = result.plan.nodes.iter().find(|n| n.id == "calc1").unwrap();
        let expr = calc_node.args.iter().find(|a| a.key == "expr").unwrap();
        assert_eq!(expr.value, ArgValue::String("8".into()));
    }

    #[test]
    fn test_constant_folding_variable_expression() {
        // Expression with variable "x + 5" should NOT be folded
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 5".into()),
            ],
            vec![Edge::new("input1", "calc1")],
        );
        let result = optimize_all(plan);
        // No constant folding should be applied
        assert!(!result
            .optimizations_applied
            .iter()
            .any(|o| o.starts_with("constant_folding")));
    }

    #[test]
    fn test_dead_node_elimination() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                // calc1 is used
                Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 1".into()),
                // calc2 is pure and NOT referenced by any edge — should be removed
                Node::new("calc2", Opcode::Calc).with_arg("expr", "x + 2".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "calc1"),
                Edge::new("calc1", "act1"),
                // NOTE: calc2 has no edges to or from it
            ],
        );
        let result = optimize_all(plan);
        assert!(result
            .optimizations_applied
            .iter()
            .any(|o| o.starts_with("dead_node_elimination")));
        // Should have 3 nodes (input1, calc1, act1) — calc2 removed
        assert_eq!(result.plan.nodes.len(), 3);
        assert!(result.plan.nodes.iter().all(|n| n.id != "calc2"));
    }

    #[test]
    fn test_dead_node_preserves_impure() {
        // Impure nodes (ACT) should NOT be removed even if unreferenced
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                // This ACT is not referenced by any edge, but should be kept (impure)
                Node::new("act1", Opcode::Act).with_arg("type", "notify".into()),
            ],
            vec![
                // Only edge: input1 has no downstream target
                // Actually let's make it so act1 has no incoming edge
            ],
        );
        let result = optimize_all(plan);
        // act1 should still be present (impure)
        assert!(result.plan.nodes.iter().any(|n| n.id == "act1"));
    }

    #[test]
    fn test_dead_node_preserves_input() {
        // INPUT nodes should never be removed
        let plan = ExecutionPlan::new(
            vec![Node::new("input1", Opcode::Input).with_arg("name", "x".into())],
            vec![],
        );
        let result = optimize_all(plan);
        assert_eq!(result.plan.nodes.len(), 1);
        assert_eq!(result.plan.nodes[0].id, "input1");
    }

    #[test]
    fn test_eval_simple_int() {
        assert_eq!(eval_simple_int_expr("42"), Some(42));
        assert_eq!(eval_simple_int_expr("3 + 5"), Some(8));
        assert_eq!(eval_simple_int_expr("10 - 3"), Some(7));
        assert_eq!(eval_simple_int_expr("4 * 5"), Some(20));
        assert_eq!(eval_simple_int_expr("10 / 2"), Some(5));
        assert_eq!(eval_simple_int_expr("x + 1"), None); // variable
    }

    #[test]
    fn test_is_constant_expression() {
        assert!(is_constant_expression("3 + 5"));
        assert!(is_constant_expression("42"));
        assert!(is_constant_expression("(1 + 2) * 3"));
        assert!(!is_constant_expression("x + 1"));
        assert!(!is_constant_expression("a + b"));
    }

    #[test]
    fn test_optimize_all_chained() {
        // Test both passes work together
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc_used", Opcode::Calc).with_arg("expr", "3 + 5".into()),
                Node::new("calc_dead", Opcode::Calc).with_arg("expr", "1 + 2".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "calc_used"),
                Edge::new("calc_used", "act1"),
                // calc_dead has no edges
            ],
        );
        let result = optimize_all(plan);
        // Should have both optimizations
        assert!(result.optimizations_applied.len() >= 2);
        // Constant folding: calc_used expr should be "8"
        let calc = result
            .plan
            .nodes
            .iter()
            .find(|n| n.id == "calc_used")
            .unwrap();
        let expr = calc.args.iter().find(|a| a.key == "expr").unwrap();
        assert_eq!(expr.value, ArgValue::String("8".into()));
        // Dead node elimination: calc_dead removed
        assert!(!result.plan.nodes.iter().any(|n| n.id == "calc_dead"));
        // Still 3 nodes
        assert_eq!(result.plan.nodes.len(), 3);
    }
}
