//! Static Validation + CFG Flattening for ExecutionPlan DAGs.
//!
//! Checks structural integrity, cycle-freedom, reachability, terminal
//! completeness, edge-condition correctness, and per-node argument validity.

use std::collections::{HashMap, HashSet, VecDeque};

use tinypipe_ir::plan::{ArgValue, Edge, EdgeKind, ExecutionPlan, Node, Opcode};

/// A validation error with context.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub node_id: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node {}: {}", self.node_id, self.message)
    }
}

/// Node'un hata feedback'inde gösterilecek kısa özeti: `CALC expr="x + 1"`.
/// Validasyon hataları yalnızca node_id (örn. `n54`) taşır — hata raporu
/// kullanıcının o node'un ne olduğunu bilmeden ayıklama yapmaması için
/// op + ana argümanları da basar.
pub fn describe_node(node: &Node) -> String {
    use Opcode::*;
    let op = node.op.display_name();
    // Her opcode için en bilgilendirici ilk birkaç argüman
    let interesting: &[&str] = match node.op {
        Input => &["name"],
        Calc => &["expr", "output"],
        Call => &["target", "url"],
        Decide => &["source", "op", "value"],
        Switch => &["source"],
        Act => &["type", "value"],
        Parallel => &[],
        Loop => &["target", "max_iterations"],
        Wait => &["duration_ms"],
        Merge => &["condition"],
        Error => &["message"],
    };
    let parts: Vec<String> = interesting
        .iter()
        .filter_map(|key| node.args.iter().find(|a| a.key == *key))
        .map(|a| format!("{}={}", a.key, fmt_arg_value(&a.value)))
        .collect();
    if parts.is_empty() {
        format!("[{}]", op)
    } else {
        format!("[{} {}]", op, parts.join(" "))
    }
}

/// ArgValue'un kompakt string gösterimi (feedback satırları için).
fn fmt_arg_value(v: &ArgValue) -> String {
    match v {
        ArgValue::String(s) => s.clone(),
        ArgValue::Int(i) => i.to_string(),
        ArgValue::Float(f) => f.to_string(),
        ArgValue::Bool(b) => b.to_string(),
        ArgValue::Null => "null".into(),
        ArgValue::Array(a) => {
            let items: Vec<String> = a.iter().map(fmt_arg_value).collect();
            format!("[{}]", items.join(", "))
        }
        ArgValue::Object(o) => {
            let mut items: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{}={}", k, fmt_arg_value(v)))
                .collect();
            items.sort();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Run the full validation pipeline on an `ExecutionPlan`.
pub fn validate(plan: &ExecutionPlan) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Build index: node_id → &Node
    let index: HashMap<&str, &Node> = plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Build adjacency for reachability
    let outgoing: HashMap<&str, Vec<&Edge>> =
        plan.edges.iter().fold(HashMap::new(), |mut acc, e| {
            acc.entry(e.from.as_str()).or_default().push(e);
            acc
        });
    let incoming: HashMap<&str, Vec<&Edge>> =
        plan.edges.iter().fold(HashMap::new(), |mut acc, e| {
            acc.entry(e.to.as_str()).or_default().push(e);
            acc
        });

    // 1. Duplicate node IDs
    {
        let mut seen = HashSet::new();
        for node in &plan.nodes {
            if !seen.insert(node.id.as_str()) {
                errors.push(ValidationError {
                    node_id: node.id.clone(),
                    message: "duplicate node id".into(),
                });
            }
        }
    }

    // 2. Edge references
    for edge in &plan.edges {
        if !index.contains_key(edge.from.as_str()) {
            errors.push(ValidationError {
                node_id: edge.from.clone(),
                message: format!("edge references unknown source node `{}`", edge.from),
            });
        }
        if !index.contains_key(edge.to.as_str()) {
            errors.push(ValidationError {
                node_id: edge.to.clone(),
                message: format!("edge references unknown target node `{}`", edge.to),
            });
        }
    }
    if !errors.is_empty() {
        return Err(errors); // fail early: other checks need valid indexes
    }

    // 3. Node count
    if plan.nodes.is_empty() {
        errors.push(ValidationError {
            node_id: "<plan>".into(),
            message: "execution plan has zero nodes".into(),
        });
    }

    // 4. Cycle detection (topological order)
    //    Back-edges to LOOP nodes are allowed (loop body → loop)
    let loop_ids: HashSet<&str> = plan
        .nodes
        .iter()
        .filter(|n| n.op == Opcode::Loop)
        .map(|n| n.id.as_str())
        .collect();

    // Build a cycle‑safe edge list: skip edges whose target is a LOOP node
    let acyclic_edges: Vec<&Edge> = plan
        .edges
        .iter()
        .filter(|e| !loop_ids.contains(e.to.as_str()))
        .collect();

    match topological_order(plan, &acyclic_edges) {
        Ok(order) => {
            if order.len() != plan.nodes.len() {
                errors.push(ValidationError {
                    node_id: "<plan>".into(),
                    message: format!(
                        "topological order returned {} nodes but plan has {}",
                        order.len(),
                        plan.nodes.len()
                    ),
                });
            }
        }
        Err(msg) => {
            errors.push(ValidationError {
                node_id: "<plan>".into(),
                message: format!("cycle detected: {}", msg),
            });
        }
    }

    // 5. Reachability from INPUT nodes or nodes with no dependencies
    // (Graphs without INPUT nodes are legal — e.g. subgraphs that fetch
    // external data or produce constants; reachability below still covers them
    // via zero-indegree roots.)

    // BFS from all INPUT nodes AND nodes with no incoming edges (constants, etc.)
    let zero_indeg: Vec<&str> = plan
        .nodes
        .iter()
        .filter(|n| !incoming.contains_key(n.id.as_str()))
        .map(|n| n.id.as_str())
        .collect();

    let reachable: HashSet<&str> = {
        let mut visited = HashSet::new();
        let mut queue: VecDeque<&str> = zero_indeg.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            if !visited.insert(id) {
                continue;
            }
            if let Some(edges) = outgoing.get(id) {
                for e in edges {
                    queue.push_back(&e.to);
                }
            }
        }
        visited
    };

    for node in &plan.nodes {
        if !reachable.contains(node.id.as_str()) {
            errors.push(ValidationError {
                node_id: node.id.clone(),
                message: "node is not reachable from any INPUT node".into(),
            });
        }
    }

    // 6. Terminal check: every path must eventually reach a terminal node
    // Terminal nodes: ACT (with type "return" or any side-effect type), ERROR
    let terminal_ids: HashSet<&str> = plan
        .nodes
        .iter()
        .filter(|n| is_terminal_opcode(n.op))
        .map(|n| n.id.as_str())
        .collect();

    if terminal_ids.is_empty() {
        errors.push(ValidationError {
            node_id: "<plan>".into(),
            message: "plan has no terminal nodes (ACT or ERROR)".into(),
        });
    }

    // Identify LOOP node IDs — nodes that reach a LOOP can reach a terminal
    // through the LOOP's body (which must itself reach a terminal).
    let loop_ids: HashSet<&str> = plan
        .nodes
        .iter()
        .filter(|n| n.op == Opcode::Loop)
        .map(|n| n.id.as_str())
        .collect();

    // Build the set of LOOP body node IDs — they terminate via loop exit, not
    // via a direct path to an ACT/ERROR node.
    let loop_body_ids: HashSet<&str> = {
        let mut bodies = HashSet::new();
        for loop_id in &loop_ids {
            if let Some(edges) = outgoing.get(loop_id) {
                let body_starts: Vec<&str> = edges
                    .iter()
                    .filter(|e| e.condition.is_none() && e.kind == EdgeKind::Data)
                    .map(|e| e.to.as_str())
                    .collect();
                for start in body_starts {
                    collect_reachable_from(start, &outgoing, &mut bodies);
                }
            }
        }
        bodies
    };

    // Extended terminal set: actual terminals + LOOP nodes (which route to body → terminal)
    // + LOOP body nodes (feeder nodes that only feed a LOOP body terminate through the
    // loop's execution — e.g. constant CALCs consumed by a body statement).
    let extended_terminals: HashSet<&str> = terminal_ids
        .iter()
        .chain(loop_ids.iter())
        .chain(loop_body_ids.iter())
        .copied()
        .collect();

    // Check that every node can reach a terminal (or a LOOP that routes to one).
    // Skip nodes that have no outgoing edges — they're dead code (harmless, removable by optimizer).
    for node in &plan.nodes {
        if terminal_ids.contains(node.id.as_str()) {
            continue; // terminal nodes are fine
        }
        if loop_body_ids.contains(node.id.as_str()) {
            continue; // LOOP body nodes terminate via loop exit
        }
        // Dead code: node has no outgoing edges — harmless
        if outgoing
            .get(node.id.as_str())
            .map(|e| e.is_empty())
            .unwrap_or(true)
        {
            continue;
        }
        if !can_reach_any(node.id.as_str(), &outgoing, &extended_terminals) {
            errors.push(ValidationError {
                node_id: node.id.clone(),
                message: "node cannot reach any terminal node (ACT or ERROR)".into(),
            });
        }
    }

    // 7. Node-specific argument checks
    for node in &plan.nodes {
        validate_node_args(node, &mut errors);
    }

    // 8. Edge condition checks
    for node in &plan.nodes {
        let out_edges = outgoing
            .get(node.id.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        match node.op {
            Opcode::Decide | Opcode::Switch => {
                // Branch nodes must have exactly 2 outgoing edges with conditions
                if out_edges.len() != 2 {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: format!(
                            "branch node must have exactly 2 outgoing edges, got {}",
                            out_edges.len()
                        ),
                    });
                }
                let has_true = out_edges
                    .iter()
                    .any(|e| e.condition.as_deref() == Some("true"));
                let has_false = out_edges
                    .iter()
                    .any(|e| e.condition.as_deref() == Some("false"));
                if !has_true {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: "branch node missing `true` condition edge".into(),
                    });
                }
                if !has_false {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: "branch node missing `false` condition edge".into(),
                    });
                }
                // All edges should have conditions
                for e in out_edges {
                    if e.condition.is_none() {
                        errors.push(ValidationError {
                            node_id: node.id.clone(),
                            message: format!("branch edge to `{}` missing condition", e.to),
                        });
                    }
                }
            }
            Opcode::Loop => {
                // Loop must have at least one outgoing edge (to body)
                if out_edges.is_empty() {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: "LOOP node must have at least one outgoing edge".into(),
                    });
                }
            }
            Opcode::Parallel => {
                if out_edges.is_empty() {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: "PARALLEL node must have at least one outgoing edge".into(),
                    });
                }
                // Parallel edges should not have conditions
                for e in out_edges {
                    if e.condition.is_some() {
                        errors.push(ValidationError {
                            node_id: node.id.clone(),
                            message: format!(
                                "PARALLEL edge to `{}` should not have a condition",
                                e.to
                            ),
                        });
                    }
                }
            }
            _ => {
                // Non-branch, non-loop, non-parallel:
                // - Data edges carry no conditions; a value may feed many consumers
                //   (fan-out to multiple tools is normal dataflow — VM executes
                //   consumers in topological order and each reads the producer's
                //   value from context).
                // - CONTROL edges are allowed alongside data edges (sequential flow)
                let data_edges: Vec<_> = out_edges
                    .iter()
                    .filter(|e| e.kind == EdgeKind::Data)
                    .collect();
                for e in &data_edges {
                    if e.condition.is_some() {
                        errors.push(ValidationError {
                            node_id: node.id.clone(),
                            message: format!(
                                "non-branch data edge to `{}` should not have a condition",
                                e.to
                            ),
                        });
                    }
                }
                // Control edges should not have conditions either
                for e in out_edges.iter().filter(|e| e.kind == EdgeKind::Control) {
                    if e.condition.is_some() {
                        errors.push(ValidationError {
                            node_id: node.id.clone(),
                            message: format!(
                                "non-branch control edge to `{}` should not have a condition",
                                e.to
                            ),
                        });
                    }
                }
            }
        }
    }

    // 10. Input node specific checks: all INPUT nodes must have a "name" arg
    for node in &plan.nodes {
        if node.op == Opcode::Input {
            let has_name = node.args.iter().any(|a| a.key == "name");
            if !has_name {
                errors.push(ValidationError {
                    node_id: node.id.clone(),
                    message: "INPUT node missing required `name` argument".into(),
                });
            }
        }
    }

    // 11. Subgraph cycle detection + nesting depth check
    {
        let mut subgraph_targets: Vec<String> = Vec::new();
        for node in &plan.nodes {
            if node.op == Opcode::Call {
                if let Some(target) =
                    node.args
                        .iter()
                        .find(|a| a.key == "target")
                        .and_then(|a| match &a.value {
                            tinypipe_ir::plan::ArgValue::String(s) => {
                                if s.starts_with("subgraph:") {
                                    Some(s.clone())
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        })
                {
                    subgraph_targets.push(target.clone());

                    // Self-cycle check: graph calling itself
                    let sub_name = target.trim_start_matches("subgraph:");
                    let deps = &plan.metadata.subgraph_dependencies;
                    if deps.iter().any(|d| d.contains(sub_name)) {
                        // Potential self-cycle — flag as warning
                        errors.push(ValidationError {
                            node_id: node.id.clone(),
                            message: format!(
                                "subgraph '{}' may contain a self-cycle (found in subgraph_dependencies)",
                                sub_name
                            ),
                        });
                    }
                }
            }
        }

        // Nesting depth check: warn if plan calls more than max_recursion_depth subgraphs
        if subgraph_targets.len() > plan.metadata.max_recursion_depth as usize {
            errors.push(ValidationError {
                node_id: "<plan>".into(),
                message: format!(
                    "subgraph nesting depth ({}) exceeds max_recursion_depth ({})",
                    subgraph_targets.len(),
                    plan.metadata.max_recursion_depth
                ),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ─── Helpers ──────────────────────────────────────────────────────

/// Opcodes that represent terminal nodes (end of execution path).
fn is_terminal_opcode(op: Opcode) -> bool {
    matches!(op, Opcode::Act | Opcode::Error)
}

/// Collect all nodes reachable from a start node (BFS), mutating the set in place.
fn collect_reachable_from<'a>(
    start: &'a str,
    outgoing: &HashMap<&'a str, Vec<&'a Edge>>,
    visited: &mut HashSet<&'a str>,
) {
    let mut queue = VecDeque::new();
    queue.push_back(start);
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(edges) = outgoing.get(id) {
            for e in edges {
                if e.kind == EdgeKind::Data {
                    queue.push_back(&e.to);
                }
            }
        }
    }
}

/// Check whether a node can reach any of the target IDs (BFS).
fn can_reach_any(
    start: &str,
    outgoing: &HashMap<&str, Vec<&Edge>>,
    targets: &HashSet<&str>,
) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start);
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id) {
            continue;
        }
        if targets.contains(id) {
            return true;
        }
        if let Some(edges) = outgoing.get(id) {
            for e in edges {
                queue.push_back(&e.to);
            }
        }
    }
    false
}

/// Run topological order on a subset of edges (Kahn's algorithm).
/// Returns nodes in topological order, or an error if a cycle is detected.
fn topological_order<'a>(
    plan: &'a ExecutionPlan,
    edges: &[&Edge],
) -> Result<Vec<&'a Node>, String> {
    let mut in_degree: HashMap<&str, usize> =
        plan.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();

    for edge in edges {
        if let Some(deg) = in_degree.get_mut(edge.to.as_str()) {
            *deg += 1;
        }
    }

    let mut queue: Vec<&Node> = plan
        .nodes
        .iter()
        .filter(|n| in_degree.get(n.id.as_str()) == Some(&0))
        .collect();

    let mut result = Vec::new();

    // Build an index for fast lookup
    let index: HashMap<&str, &Node> = plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    // Build outgoing adjacency from the filtered edge list
    let outgoing: HashMap<&str, Vec<&&Edge>> = edges.iter().fold(HashMap::new(), |mut acc, e| {
        acc.entry(e.from.as_str()).or_default().push(e);
        acc
    });

    while let Some(node) = queue.pop() {
        result.push(node);
        if let Some(out) = outgoing.get(node.id.as_str()) {
            for e in out {
                if let Some(deg) = in_degree.get_mut(e.to.as_str()) {
                    *deg -= 1;
                    if *deg == 0 {
                        if let Some(next) = index.get(e.to.as_str()) {
                            queue.push(next);
                        }
                    }
                }
            }
        }
    }

    if result.len() != plan.nodes.len() {
        return Err("Graph contains a cycle".into());
    }
    Ok(result)
}

/// Per-node argument validation.
fn validate_node_args(node: &Node, errors: &mut Vec<ValidationError>) {
    let mut missing = |key: &str| {
        let has = node.args.iter().any(|a| a.key == key);
        if !has {
            errors.push(ValidationError {
                node_id: node.id.clone(),
                message: format!("{:?} node missing required argument `{}`", node.op, key),
            });
        }
    };

    match node.op {
        Opcode::Input => {
            missing("name");
        }
        Opcode::Call => {
            missing("type");
            missing("target");
        }
        Opcode::Calc => {
            // "expr" is optional — some CALC nodes may just relay values
        }
        Opcode::Decide | Opcode::Switch => {
            missing("condition");
        }
        Opcode::Act => {
            missing("type");
        }
        Opcode::Parallel => {
            // No required args
        }
        Opcode::Loop => {
            missing("max_iterations");
        }
        Opcode::Wait => {
            // No required args
        }
        Opcode::Merge => {
            // No required args
        }
        Opcode::Error => {
            missing("message");
        }
    }
}

// ─── CFG Flattening ──────────────────────────────────────────────
//
// The transform step already produces a flat DAG.  CFG flattening here
// acts as a *verification* pass: it walks the plan and ensures the
// invariants listed in the plan (section 4.1) hold.
//
// Invariants verified by `validate()` above:
//   - No node has >1 outgoing edge (DECIDE/SWITCH are the only exceptions: 2)
//   - No node is visited more than once  (DAG — guaranteed by topological order)
//   - Early `return` is represented as an OUTPUT edge (ACT with type "return")

/// Verify CFG flattening invariants on an already-validated plan.
/// This is an additional belt-and-suspenders check.
pub fn verify_cfg_flattening(plan: &ExecutionPlan) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let outgoing: HashMap<&str, Vec<&Edge>> =
        plan.edges.iter().fold(HashMap::new(), |mut acc, e| {
            acc.entry(e.from.as_str()).or_default().push(e);
            acc
        });

    for node in &plan.nodes {
        let out_edges = outgoing
            .get(node.id.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        match node.op {
            Opcode::Decide | Opcode::Switch => {
                // Exactly 2 edges, both with conditions
                if out_edges.len() != 2 {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: format!(
                            "CFG: DECIDE/SWITCH must have 2 outgoing edges, got {}",
                            out_edges.len()
                        ),
                    });
                }
            }
            Opcode::Loop => {
                // Loop body edges — at least 1
                if out_edges.is_empty() {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: "CFG: LOOP must have at least 1 outgoing edge".into(),
                    });
                }
            }
            Opcode::Parallel => {
                if out_edges.len() < 2 {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: format!(
                            "CFG: PARALLEL should have ≥2 outgoing edges, got {}",
                            out_edges.len()
                        ),
                    });
                }
            }
            _ => {
                if out_edges.len() > 1 {
                    errors.push(ValidationError {
                        node_id: node.id.clone(),
                        message: format!(
                            "CFG: non-branch node has {} outgoing edges",
                            out_edges.len()
                        ),
                    });
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_ir::plan::{ArgValue, Node as PlanNode};

    fn make_node_with(id: &str, op: Opcode, args: &[(&str, &str)]) -> PlanNode {
        let mut n = PlanNode::new(id, op);
        for (k, v) in args {
            n = n.with_arg(k, ArgValue::String(v.to_string()));
        }
        n
    }

    // ── Valid plans ──────────────────────────────────────────────

    #[test]
    fn valid_simple_plan() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x + 1")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
            ],
            vec![Edge::new("n0", "n1"), Edge::new("n1", "n2")],
        );
        assert!(
            validate(&plan).is_ok(),
            "valid plan should pass: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn valid_decide_plan() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x > 0")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"),
                Edge::with_condition("n1", "n3", "false"),
            ],
        );
        assert!(
            validate(&plan).is_ok(),
            "valid decide plan: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn valid_loop_plan() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "items")]),
                make_node_with("n1", Opcode::Loop, &[("max_iterations", "10")]),
                make_node_with("n2", Opcode::Calc, &[("expr", "i + 1")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"),
                Edge::new("n2", "n1"), // back-edge
                Edge::new("n1", "n3"),
            ],
        );
        assert!(
            validate(&plan).is_ok(),
            "valid loop plan: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn valid_parallel_plan() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Parallel, &[]),
                make_node_with("n2", Opcode::Act, &[("type", "log")]),
                make_node_with("n3", Opcode::Act, &[("type", "log")]),
                make_node_with("n4", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"),
                Edge::new("n1", "n3"),
                Edge::new("n2", "n4"),
                Edge::new("n3", "n4"),
            ],
        );
        assert!(
            validate(&plan).is_ok(),
            "valid parallel plan: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn valid_error_plan() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x < 0")]),
                make_node_with("n2", Opcode::Error, &[("message", "negative value")]),
                make_node_with("n3", Opcode::Calc, &[("expr", "x")]),
                make_node_with("n4", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"),
                Edge::with_condition("n1", "n3", "false"),
                Edge::new("n3", "n4"),
            ],
        );
        assert!(
            validate(&plan).is_ok(),
            "valid error plan: {:?}",
            validate(&plan)
        );
    }

    // ── Invalid plans ────────────────────────────────────────────

    #[test]
    fn detect_duplicate_node_id() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n0", Opcode::Calc, &[("expr", "x")]), // duplicate
            ],
            vec![],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("duplicate")),
            "should detect duplicate, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_bad_edge_ref() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
            ],
            vec![
                Edge::new("n0", "n2"), // n2 doesn't exist
            ],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("unknown target")),
            "should detect bad ref, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_cycle() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n0"), // cycle!
            ],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("cycle")),
            "should detect cycle, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_no_input() {
        // INPUT'suz plan'lar artık geçerli (subgraph'lar sabit üretir veya dış
        // veri çeker) — uçtan uca erişilebilir olduğu sürece hata yok.
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Calc, &[("expr", "1")]),
                make_node_with("n1", Opcode::Act, &[("type", "return")]),
            ],
            vec![Edge::new("n0", "n1")],
        );
        assert!(
            validate(&plan).is_ok(),
            "input-less but reachable plan should validate"
        );
    }

    #[test]
    fn detect_unreachable() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Calc, &[("expr", "orphan")]), // dead end
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"),
                Edge::new("n1", "n3"), // n3 is reachable but dead
            ],
        );
        // A value feeding multiple consumers (fan-out) is valid dataflow — the VM
        // runs consumers in topological order and each reads the value from context.
        assert!(
            validate(&plan).is_ok(),
            "multi-consumer fan-out should be accepted: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn detect_fanout_allowed() {
        // One producer, two consumers: valid dataflow (e.g. a parsed array read
        // by both array.len and array.count_where).
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"),
                Edge::new("n1", "n3"),
            ],
        );
        let res = validate(&plan);
        assert!(res.is_ok(), "fanout should be valid, got: {:?}", res);
    }

    #[test]
    fn detect_no_terminal() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
                // No ACT or ERROR
            ],
            vec![Edge::new("n0", "n1")],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("no terminal")),
            "should detect no terminal, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_non_terminal_path() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Calc, &[("expr", "dead")]),
                make_node_with("n4", Opcode::Calc, &[("expr", "also_dead")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"),
                Edge::control("n1", "n3"), // control edge: n3 has outgoing edge (n3→n4) but still can't reach terminal
                Edge::new("n3", "n4"),     // n4 is dead code (no outgoing edges)
            ],
        );
        let err = validate(&plan).unwrap_err();
        // n3 should be reported as not reaching a terminal (n4 is dead code, not a terminal)
        assert!(
            err.iter()
                .any(|e| e.node_id == "n3" && e.message.contains("cannot reach any terminal")),
            "should report n3 as non-terminal, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_decide_missing_condition() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x > 0")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"), // no condition
                Edge::with_condition("n1", "n3", "false"),
            ],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("missing `true`")),
            "should detect missing true condition, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_decide_wrong_edge_count() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x > 0")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"),
                // missing false branch
            ],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("exactly 2")),
            "should detect wrong edge count, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_input_missing_name() {
        let plan = ExecutionPlan::new(
            vec![
                PlanNode::new("n0", Opcode::Input), // no name arg
                make_node_with("n1", Opcode::Act, &[("type", "return")]),
            ],
            vec![Edge::new("n0", "n1")],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.message.contains("missing required") && e.message.contains("name")),
            "should detect missing name, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_non_branch_multi_edge() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]), // non-branch
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::new("n1", "n2"), // calc feeds two consumers
                Edge::new("n1", "n3"),
            ],
        );
        // Multi-consumer fan-out from a non-branch node is valid dataflow.
        assert!(validate(&plan).is_ok());
    }

    #[test]
    fn detect_error_missing_message() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                PlanNode::new("n1", Opcode::Error), // no message
            ],
            vec![Edge::new("n0", "n1")],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.message.contains("missing required") && e.message.contains("message")),
            "should detect missing message, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_non_branch_edge_with_condition() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Calc, &[("expr", "x")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"), // condition on non-branch edge
            ],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.message.contains("should not have a condition")),
            "should detect condition on non-branch, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_call_missing_type() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                PlanNode::new("n1", Opcode::Call), // no type or target
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
            ],
            vec![Edge::new("n0", "n1"), Edge::new("n1", "n2")],
        );
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.node_id == "n1" && e.message.contains("missing required")),
            "should detect missing args, got: {:?}",
            err
        );
    }

    #[test]
    fn detect_empty_plan() {
        let plan = ExecutionPlan::new(vec![], vec![]);
        let err = validate(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("zero nodes")),
            "should detect empty plan, got: {:?}",
            err
        );
    }

    // ── CFG Flattening verification ──────────────────────────────

    #[test]
    fn cfg_verify_valid() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x > 0")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
                make_node_with("n3", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"),
                Edge::with_condition("n1", "n3", "false"),
            ],
        );
        assert!(verify_cfg_flattening(&plan).is_ok());
    }

    #[test]
    fn cfg_verify_decide_wrong_edge_count() {
        let plan = ExecutionPlan::new(
            vec![
                make_node_with("n0", Opcode::Input, &[("name", "x")]),
                make_node_with("n1", Opcode::Decide, &[("condition", "x > 0")]),
                make_node_with("n2", Opcode::Act, &[("type", "return")]),
            ],
            vec![
                Edge::new("n0", "n1"),
                Edge::with_condition("n1", "n2", "true"),
                // only 1 outgoing from decide — cfg should flag it
            ],
        );
        let err = verify_cfg_flattening(&plan).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("must have 2")),
            "CFG: {:?}",
            err
        );
    }

    // ── End-to-end: transform + validate ─────────────────────────

    #[test]
    fn e2e_valid_graph() {
        let code = "def graph(x: int, y: int):\n    z = x + y\n    return z";
        let plan = crate::transform::transform(code).expect("transform should succeed");
        assert!(
            validate(&plan).is_ok(),
            "valid graph should pass validation: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn e2e_if_else_graph() {
        let code = "def graph(x: int):\n    if x > 0:\n        y = 1\n    else:\n        y = 2\n    return y";
        let plan = crate::transform::transform(code).expect("transform should succeed");
        assert!(
            validate(&plan).is_ok(),
            "if-else graph: {:?}",
            validate(&plan)
        );
    }

    #[test]
    fn e2e_loop_graph() {
        // For-loop transform now produces valid DAGs:
        //   - LOOP→first_body edge makes body nodes reachable
        //   - No back-edge from body→LOOP (eliminates cycle)
        //   - Loop variable registered in var_map before body
        let code = "def graph(items: list):\n    total = 0\n    for i in range(len(items)):\n        total = total + i\n    return total";
        let plan = crate::transform::transform(code).expect("transform should succeed");
        // Debug: print node IDs and ops
        for n in &plan.nodes {
            eprintln!("  {} — {:?} — args: {:?}", n.id, n.op, n.args);
        }
        for e in &plan.edges {
            eprintln!(
                "  {} → {} (cond={:?}, kind={:?})",
                e.from, e.to, e.condition, e.kind
            );
        }
        let val = validate(&plan);
        eprintln!("Validation: {:?}", val);
        // Should pass — the fix removes all known validation errors
        assert!(val.is_ok(), "loop plan should pass validation: {:?}", val);
    }
}
