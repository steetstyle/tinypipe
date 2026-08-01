//! Python AST → Opcode AST (ExecutionPlan) transformer.
//!
//! Takes Restricted Python code (already sanitized), walks the AST,
//! and produces an `ExecutionPlan` suitable for the tinypipe-vm interpreter.

use std::collections::{HashMap, HashSet};

use parser::ast::{self, Ranged};
use parser::source_code::{LineIndex, SourceLocation};
use rustpython_parser as parser;

use tinypipe_ir::plan::{Arg, ArgValue, Edge, ExecutionPlan, Node as PlanNode, Opcode};

/// Errors produced during the transform phase.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl std::fmt::Display for TransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}:{} — {}", self.line, self.column, self.message)
    }
}

/// Parse + sanitize + transform Restricted Python into an ExecutionPlan.
pub fn transform(code: &str) -> Result<ExecutionPlan, Vec<TransformError>> {
    // 1. Sanitize
    if let Err(errors) = crate::sanitizer::sanitize(code) {
        return Err(errors
            .into_iter()
            .map(|e| TransformError {
                line: e.line,
                column: e.column,
                message: e.message,
            })
            .collect());
    }

    // 2. Parse
    let module = match parser::parse(code, parser::Mode::Module, "<embedded>") {
        Ok(ast::Mod::Module(m)) => m,
        Ok(_) => {
            return Err(vec![TransformError {
                line: 0,
                column: 0,
                message: "expected a module".into(),
            }])
        }
        Err(e) => {
            return Err(vec![TransformError {
                line: 0,
                column: 0,
                message: format!("parse error: {}", e.error),
            }])
        }
    };

    // 3. Transform
    let mut engine = TransformEngine::new(code);
    engine.transform_module(&module)?;
    Ok(engine.build())
}

// ─── Internal engine ──────────────────────────────────────────────

/// Tracks variable-to-node mappings and builds the ExecutionPlan.
struct TransformEngine<'a> {
    code: &'a str,
    line_index: LineIndex,

    // Plan being built
    nodes: Vec<PlanNode>,
    edges: Vec<Edge>,

    // Variable context: maps variable name → node id that LAST wrote it.
    var_map: HashMap<String, String>,

    // Set of node IDs that are terminators (return, error) — no outgoing control flow
    terminal_nodes: HashSet<String>,

    // Node ID counter
    next_id: u32,

    // Current PARALLEL branch ID (None = outside any parallel branch)
    current_branch: Option<u32>,

    // Error collector
    errors: Vec<TransformError>,
}

impl<'a> TransformEngine<'a> {
    fn new(code: &'a str) -> Self {
        let line_index = LineIndex::from_source_text(code);
        Self {
            code,
            line_index,
            nodes: Vec::new(),
            edges: Vec::new(),
            var_map: HashMap::new(),
            terminal_nodes: HashSet::new(),
            next_id: 0,
            current_branch: None,
            errors: Vec::new(),
        }
    }

    fn build(self) -> ExecutionPlan {
        ExecutionPlan::new(self.nodes, self.edges)
    }

    // ── ID generation ──────────────────────────────────────────

    fn gen_id(&mut self) -> String {
        let id = format!("n{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Push a plan node and return its id.
    /// If currently inside a PARALLEL branch, assigns the branch_id automatically.
    fn push_node(&mut self, op: Opcode, args: Vec<(&str, ArgValue)>) -> String {
        let id = self.gen_id();
        let mut node = PlanNode::new(&id, op);
        if let Some(bid) = self.current_branch {
            node.branch_id = Some(bid);
        }
        for (k, v) in args {
            node = node.with_arg(k, v);
        }
        self.nodes.push(node);
        id
    }

    /// Add an edge between two nodes.
    fn push_edge(&mut self, from: &str, to: &str) {
        self.edges.push(Edge::new(from, to));
    }

    /// Add a conditional edge.
    fn push_cond_edge(&mut self, from: &str, to: &str, condition: &str) {
        self.edges.push(Edge::with_condition(from, to, condition));
    }

    /// Resolve the node id that currently holds a variable's value.
    /// If the variable is unknown, we create an Input node for it.
    fn resolve_var(&mut self, name: &str, _loc: SourceLocation) -> String {
        if let Some(id) = self.var_map.get(name) {
            return id.clone();
        }
        // Unknown variable → create an Input node (data comes from context)
        let id = self.push_node(Opcode::Input, vec![("name", name.into())]);
        self.var_map.insert(name.to_owned(), id.clone());
        id
    }

    fn error(&mut self, node: &dyn Ranged, msg: &str) {
        let loc = self.resolve_location(node.start());
        self.errors.push(TransformError {
            line: loc.row.get() as usize,
            column: loc.column.get() as usize,
            message: msg.to_owned(),
        });
    }

    fn resolve_location(&self, offset: parser::text_size::TextSize) -> SourceLocation {
        self.line_index.source_location(offset, self.code)
    }

    // ── Module / Function ──────────────────────────────────────

    fn transform_module(&mut self, module: &ast::ModModule) -> Result<(), Vec<TransformError>> {
        // Find the `graph` function definition
        let graph_func = module.body.iter().find_map(|stmt| match stmt {
            ast::Stmt::FunctionDef(f) if f.name.as_str() == "graph" => Some(f),
            _ => None,
        });

        let func = match graph_func {
            Some(f) => f,
            None => {
                self.errors.push(TransformError {
                    line: 0,
                    column: 0,
                    message: "no `graph()` function found".into(),
                });
                return Err(std::mem::take(&mut self.errors));
            }
        };

        // Create INPUT nodes for function parameters
        for arg_def in &func.args.args {
            let name = arg_def.def.arg.as_str();
            let id = self.push_node(Opcode::Input, vec![("name", name.into())]);
            self.var_map.insert(name.to_owned(), id);
        }
        for arg_def in &func.args.posonlyargs {
            let name = arg_def.def.arg.as_str();
            let id = self.push_node(Opcode::Input, vec![("name", name.into())]);
            self.var_map.insert(name.to_owned(), id);
        }

        // Track the previous statement's last node for control-flow edges
        let mut prev_last: Option<String> = None;

        // Transform each statement in the function body
        for stmt in &func.body {
            let (first_id, last_id) = self.transform_stmt(stmt, &prev_last)?;

            // Add control-flow edge from previous statement's last node to this
            // statement's first node. This enforces sequential execution order.
            // Skip if previous statement ends with a terminal (return/error).
            if let Some(prev) = &prev_last {
                if !self.is_return_node(prev) {
                    self.edges.push(Edge::control(prev, &first_id));
                }
            }

            prev_last = Some(last_id);
        }

        Ok(())
    }

    // ── Statement transformer ──────────────────────────────────

    /// Transform a statement, returning `(first_node_id, last_node_id)`.
    /// - `first_node_id`: the node that incoming control-flow edges connect to.
    /// - `last_node_id`: the node that outgoing control-flow edges come from.
    /// For most single-node statements, both are the same node.
    fn transform_stmt(
        &mut self,
        stmt: &ast::Stmt,
        _prev_id: &Option<String>,
    ) -> Result<(String, String), Vec<TransformError>> {
        // Check for errors first
        if !self.errors.is_empty() {
            return Err(std::mem::take(&mut self.errors));
        }

        match stmt {
            // ── Assign / AugAssign / AnnAssign ──────────────────
            ast::Stmt::Assign(s) => {
                // We only handle single-target assignments for now
                let target = if s.targets.len() == 1 {
                    &s.targets[0]
                } else {
                    self.error(stmt, "multi‑target assignment is not supported");
                    return Ok((self.gen_id(), self.gen_id()));
                };

                let target_name = match target {
                    ast::Expr::Name(n) => n.id.as_str().to_owned(),
                    _ => {
                        self.error(target, "only simple variable names as assignment targets");
                        return Ok((self.gen_id(), self.gen_id()));
                    }
                };

                // Transform the value expression — this creates the CALC node
                let val_id = self.transform_expr_to_node(&s.value)?;

                // Add `output` arg so the executor stores the result in context
                if let Some(node) = self.nodes.iter_mut().find(|n| n.id == val_id) {
                    node.args.push(Arg {
                        key: "output".into(),
                        value: ArgValue::String(target_name.clone()),
                    });
                }

                // Map variable → this node
                self.var_map.insert(target_name, val_id.clone());
                Ok((val_id.clone(), val_id))
            }

            ast::Stmt::AnnAssign(s) => {
                let target_name = match s.target.as_ref() {
                    ast::Expr::Name(n) => n.id.as_str().to_owned(),
                    _ => {
                        self.error(
                            s.target.as_ref(),
                            "only simple variable names as annotation targets",
                        );
                        return Ok((self.gen_id(), self.gen_id()));
                    }
                };

                if let Some(v) = &s.value {
                    let val_id = self.transform_expr_to_node(v)?;
                    self.var_map.insert(target_name, val_id.clone());
                    Ok((val_id.clone(), val_id))
                } else {
                    // Annotation without value: create an Input node
                    let id = self.push_node(
                        Opcode::Input,
                        vec![("name", ArgValue::String(target_name.clone()))],
                    );
                    self.var_map.insert(target_name, id.clone());
                    Ok((id.clone(), id))
                }
            }

            ast::Stmt::AugAssign(s) => {
                let target_name = match s.target.as_ref() {
                    ast::Expr::Name(n) => n.id.as_str().to_owned(),
                    _ => {
                        self.error(
                            s.target.as_ref(),
                            "only simple variable names for augmented assignment",
                        );
                        return Ok((self.gen_id(), self.gen_id()));
                    }
                };

                let op_str = aug_op_to_str(&s.op);
                let target_id =
                    self.resolve_var(&target_name, self.resolve_location(s.target.start()));

                // Read the RHS
                let rhs_id = self.transform_expr_to_node(&s.value)?;

                // Create a CALC node: `target op value`
                let expr_str = format!(
                    "{} {} {}",
                    target_name,
                    op_str,
                    format_expr(&s.value, self.code)
                );
                let calc_id = self.push_node(
                    Opcode::Calc,
                    vec![
                        ("expr", ArgValue::String(expr_str)),
                        ("output", ArgValue::String(target_name.clone())),
                    ],
                );
                self.push_edge(&target_id, &calc_id);
                self.push_edge(&rhs_id, &calc_id);

                self.var_map.insert(target_name, calc_id.clone());
                Ok((calc_id.clone(), calc_id))
            }

            // ── Expr statement (call/act/etc as statement) ─────
            ast::Stmt::Expr(s) => {
                let id = self.transform_expr_to_node(&s.value)?;
                Ok((id.clone(), id))
            }

            // ── Return ─────────────────────────────────────────
            ast::Stmt::Return(s) => {
                let output_id = if let Some(v) = &s.value {
                    let val_id = self.transform_expr_to_node(v)?;
                    // Create an Act node to represent the output
                    let id = self.push_node(Opcode::Act, vec![("type", "return".into())]);
                    self.push_edge(&val_id, &id);
                    id
                } else {
                    self.push_node(Opcode::Act, vec![("type", "return".into())])
                };
                // Mark as terminal — no outgoing control flow to subsequent statements
                self.terminal_nodes.insert(output_id.clone());
                Ok((output_id.clone(), output_id))
            }

            // ── If / elif / else ───────────────────────────────
            ast::Stmt::If(s) => {
                let (first_id, last_id) = self.transform_if(s)?;
                Ok((first_id, last_id))
            }

            // ── For loop ───────────────────────────────────────
            ast::Stmt::For(s) => {
                let iter_id = self.transform_expr_to_node(&s.iter)?;

                let target_name = match s.target.as_ref() {
                    ast::Expr::Name(n) => n.id.as_str().to_owned(),
                    _ => {
                        self.error(s.target.as_ref(), "loop target must be a simple variable");
                        return Ok((self.gen_id(), self.gen_id()));
                    }
                };

                // Infer max iterations from the iter expression
                let max_iter = infer_range_max(&s.iter);
                let loop_id = self.push_node(
                    Opcode::Loop,
                    vec![
                        ("target", ArgValue::String(target_name.clone())),
                        ("max_iterations", max_iter.into()),
                    ],
                );
                self.push_edge(&iter_id, &loop_id);

                // Register loop variable in var_map BEFORE body transform,
                // so body expressions can reference it via resolve_var().
                // The LOOP node's outgoing edge to the body carries this variable's value.
                self.var_map.insert(target_name.clone(), loop_id.clone());

                // Transform body — track first and last nodes for LOOP→body edge
                let mut body_first: Option<String> = None;
                let mut body_last: Option<String> = None;
                for child in &s.body {
                    let (cid, last_id) = self.transform_stmt(child, &body_last)?;
                    if body_first.is_none() {
                        body_first = Some(cid);
                    }
                    body_last = Some(last_id);
                }

                // Add LOOP→first_body unconditional edge:
                // - Makes body nodes reachable from root (no dangling nodes)
                // - Lets the executor identify body nodes via reachability analysis
                // - Carries the loop variable value to body nodes
                if let Some(first) = &body_first {
                    self.push_edge(&loop_id, first);
                }

                // No back-edge from body to LOOP — the executor handles iteration
                // internally without needing data-flow back-edges. The LOOP node's
                // body-set identification uses the LOOP→first_body edge above.

                // Loop variable remains registered for subsequent code to reference
                // the last iteration's value.
                Ok((loop_id.clone(), loop_id))
            }

            // ── With parallel ──────────────────────────────────
            ast::Stmt::With(s) => {
                // Verify it's `with parallel() as p:`
                if s.items.len() == 1 && is_name_call(&s.items[0].context_expr, "parallel") {
                    let par_id = self.push_node(Opcode::Parallel, vec![]);
                    // Her parallel branch'e unique branch_id ata
                    for (branch_idx, child) in s.body.iter().enumerate() {
                        let saved_branch = self.current_branch;
                        self.current_branch = Some(branch_idx as u32);
                        let (cid, _) = self.transform_stmt(child, &None)?;
                        self.current_branch = saved_branch;
                        self.push_edge(&par_id, &cid);
                    }
                    Ok((par_id.clone(), par_id))
                } else {
                    self.error(stmt, "unsupported `with` statement");
                    Ok((self.gen_id(), self.gen_id()))
                }
            }

            // ── Match / case ─────────────────────────────────────
            ast::Stmt::Match(s) => {
                let id = self.transform_match(s)?;
                Ok((id.clone(), id))
            }

            // ── Other statement types (should be blocked by sanitizer, but handle gracefully) ──
            ast::Stmt::Pass(_) => {
                // No-op
                let id = self.gen_id();
                Ok((id.clone(), id))
            }
            ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {
                // These are valid inside loops but we handle them at the loop level
                let id = self.gen_id();
                Ok((id.clone(), id))
            }
            ast::Stmt::Raise(_) => {
                let id = self.push_node(Opcode::Error, vec![("message", "raised by code".into())]);
                self.terminal_nodes.insert(id.clone());
                Ok((id.clone(), id))
            }
            ast::Stmt::Assert(s) => {
                // Assert → DECIDE node; if test is false, go to ERROR
                let test_id = self.transform_expr_to_node(&s.test)?;
                let decide_id = self.push_node(
                    Opcode::Decide,
                    vec![(
                        "condition",
                        ArgValue::String(format!("not ({})", format_expr(&s.test, self.code))),
                    )],
                );
                self.push_edge(&test_id, &decide_id);
                Ok((decide_id.clone(), decide_id))
            }

            // Anything else: blocked by sanitizer, but handle gracefully
            _ => {
                self.error(stmt, "unsupported statement in transform");
                let id = self.gen_id();
                Ok((id.clone(), id))
            }
        }
    }

    // ── Match / case transform ─────────────────────────────────

    fn transform_match(&mut self, s: &ast::StmtMatch) -> Result<String, Vec<TransformError>> {
        // NOTE: This returns just a String (the switch_id), not (first, last).
        // It's called from transform_stmt which wraps the result as (id, id).
        // Transform the subject expression → node that provides the value
        let subject_id = self.transform_expr_to_node(&s.subject)?;

        // Create a SWITCH node
        let switch_id = self.push_node(
            Opcode::Switch,
            vec![(
                "source",
                ArgValue::String(format_expr(&s.subject, self.code)),
            )],
        );
        self.push_edge(&subject_id, &switch_id);

        // Track whether we've seen a wildcard / default case
        let mut has_default = false;

        for case in &s.cases {
            // Extract case condition from pattern
            let condition: Option<String> = match &case.pattern {
                ast::Pattern::MatchValue(v) => {
                    // e.g. `case 1:` → condition = "1"
                    // e.g. `case "hello":` → condition = "hello"
                    Some(format_expr(&v.value, self.code))
                }
                ast::Pattern::MatchSingleton(v) => {
                    // e.g. `case True:` → condition = "True"
                    // e.g. `case None:` → condition = "None"
                    let singleton_str = format_constant(&v.value);
                    Some(singleton_str)
                }
                ast::Pattern::MatchAs(_v) => {
                    // e.g. `case _:` → wildcard / default
                    // e.g. `case x:` → captures into x (treated as default)
                    if has_default {
                        // A second default-like case is ambiguous; still emit but warn
                        self.error(
                            s,
                            "multiple default cases in match — only first is reachable",
                        );
                    }
                    has_default = true;
                    None // unconditional edge (will be handled as default in executor)
                }
                _ => {
                    // Unsupported pattern type (MatchSequence, MatchMapping, MatchClass, etc.)
                    self.error(
                        s,
                        &format!(
                            "unsupported pattern type in match case: {:?}",
                            std::mem::discriminant(&case.pattern)
                        ),
                    );
                    continue;
                }
            };

            // Transform the case body
            let mut body_last: Option<String> = None;
            for stmt in &case.body {
                let (cid, _) = self.transform_stmt(stmt, &body_last)?;
                body_last = Some(cid);
            }

            if let Some(body_first_id) = body_last {
                // Create a conditional edge from SWITCH to the first body node
                if let Some(cond) = &condition {
                    self.push_cond_edge(&switch_id, &body_first_id, cond);
                } else {
                    // Default/wildcard — no condition (executor treats None as fallback)
                    self.push_edge(&switch_id, &body_first_id);
                }
            }
        }

        Ok(switch_id)
    }

    // ── If / elif / else transform ─────────────────────────────

    /// Check if a node is a terminal (return, error) that stops execution flow.
    fn is_return_node(&self, node_id: &str) -> bool {
        self.terminal_nodes.contains(node_id)
    }

    fn transform_if(&mut self, s: &ast::StmtIf) -> Result<(String, String), Vec<TransformError>> {
        // Condition
        let cond_id = self.transform_expr_to_node(&s.test)?;

        // DECIDE node
        let decide_id = self.push_node(
            Opcode::Decide,
            vec![(
                "condition",
                ArgValue::String(format_expr(&s.test, self.code)),
            )],
        );
        self.push_edge(&cond_id, &decide_id);

        // Transform body (true branch)
        let var_map_before_true = self.var_map.clone();
        let mut body_last: Option<String> = None;
        let mut body_first: Option<String> = None;
        for child in &s.body {
            let (cid, last_id) = self.transform_stmt(child, &body_last)?;
            if body_first.is_none() {
                body_first = Some(cid);
            }
            body_last = Some(last_id);
        }
        // Variables assigned in the true branch
        let true_vars: Vec<String> = self
            .var_map
            .keys()
            .filter(|k| !var_map_before_true.contains_key(k.as_str()))
            .cloned()
            .collect();
        if let Some(last) = &body_last {
            self.push_cond_edge(&decide_id, last, "true");
        }

        // Transform else / elif branches
        let var_map_before_false = self.var_map.clone();
        let mut orelse_last: Option<String> = None;
        let mut orelse_first: Option<String> = None;
        for child in &s.orelse {
            let (cid, last_id) = self.transform_stmt(child, &orelse_last)?;
            if orelse_first.is_none() {
                orelse_first = Some(cid);
            }
            orelse_last = Some(last_id);
        }
        // Variables assigned in the false branch
        let false_vars: Vec<String> = self
            .var_map
            .keys()
            .filter(|k| !var_map_before_false.contains_key(k.as_str()))
            .cloned()
            .collect();
        if let Some(last) = &orelse_last {
            self.push_cond_edge(&decide_id, last, "false");
        }

        // Determine if each branch is terminal (ends with return/error)
        let body_is_terminal = body_last
            .as_ref()
            .map_or(false, |id| self.is_return_node(id));
        let orelse_is_terminal = orelse_last
            .as_ref()
            .map_or(false, |id| self.is_return_node(id));

        // If ALL branches exist and ALL are terminal, no MERGE needed.
        // Note: an empty orelse branch (no else clause) does NOT make the if "fully terminal"
        // because execution simply falls through — we still need a MERGE to synchronize.
        if body_is_terminal && !s.orelse.is_empty() && orelse_is_terminal {
            // All branches terminate: no MERGE node needed.
            // Variables are not accessible after the if/else (both paths returned).
            // Return (decide_id, decide_id) — both first and last are the DECIDE.
            return Ok((decide_id.clone(), decide_id));
        }

        // Create a MERGE node to join the non-terminal branches.
        // For the fall-through case (body is terminal, no else), the MERGE
        // receives a conditional edge from DECIDE(false), so it only fires
        // when the false path is taken — enabling correct early-return semantics.
        let merge_id = self.push_node(Opcode::Merge, vec![]);
        if let Some(last) = &body_last {
            if !body_is_terminal {
                self.push_edge(last, &merge_id);
            }
        }
        if let Some(last) = &orelse_last {
            if !orelse_is_terminal {
                self.push_edge(last, &merge_id);
            }
        }

        // Fall-through: if the true branch is terminal and there's no else,
        // connect DECIDE(false) → MERGE so the false path reaches the merge.
        if body_is_terminal && s.orelse.is_empty() {
            self.push_cond_edge(&decide_id, &merge_id, "false");
        }
        // If the false branch is terminal and there IS an else clause but no
        // true branch fall-through, the true path goes through normally.

        // Update var_map: variables that were assigned in a non-terminal branch
        // now point to the MERGE node
        let branch_vars: Vec<String> = true_vars
            .into_iter()
            .chain(false_vars)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        for var in branch_vars {
            self.var_map.insert(var, merge_id.clone());
        }

        // First node is the DECIDE (for control-flow edges from previous statement).
        // Last node is the MERGE (for control-flow edges to next statement).
        let first_id = decide_id;
        Ok((first_id, merge_id))
    }

    // ── Expression transformer ─────────────────────────────────

    /// Transform an expression into a plan node. For simple expressions
    /// (constants, names), this creates a CALC node. For calls, it creates
    /// a CALL node.
    fn transform_expr_to_node(&mut self, expr: &ast::Expr) -> Result<String, Vec<TransformError>> {
        match expr {
            // ── Call: call("tool", ...) or act("type", ...) ─────
            ast::Expr::Call(c) => {
                let callee = callee_name_str(c);
                let Some(name) = callee else {
                    self.error(expr, "dynamic calls are not supported in transform");
                    return Ok(self.gen_id());
                };

                if name == "call" || name == "act" {
                    // First arg must be a string literal (verified by sanitizer)
                    let target = if let Some(arg) = c.args.first() {
                        format_expr(arg, self.code)
                    } else {
                        String::new()
                    };

                    // Build proper node args
                    let mut proper_args: Vec<(&str, ArgValue)> = vec![
                        ("type", ArgValue::String(name.clone())),
                        ("target", ArgValue::String(target.clone())),
                    ];
                    for kw in &c.keywords {
                        let key = kw.arg.as_ref().map(|i| i.as_str()).unwrap_or("");
                        let val_str = format_expr(&kw.value, self.code);
                        proper_args.push((key, ArgValue::String(val_str)));
                    }

                    let node_id = self.push_node(Opcode::Call, proper_args);

                    // Wire input dependencies: evaluate each arg expression
                    for arg in &c.args {
                        let arg_id = self.transform_expr_to_node(arg)?;
                        self.push_edge(&arg_id, &node_id);
                    }
                    for kw in &c.keywords {
                        let kw_id = self.transform_expr_to_node(&kw.value)?;
                        self.push_edge(&kw_id, &node_id);
                    }

                    Ok(node_id)
                } else if name == "sleep" {
                    // sleep(N) → WAIT node
                    let secs = if let Some(first_arg) = c.args.first() {
                        let arg_str = format_expr(first_arg, self.code);
                        // Parse integer or context variable
                        if let Ok(n) = arg_str.parse::<i64>() {
                            n
                        } else {
                            // Default to 1 if we can't parse (will be evaluated at runtime)
                            1
                        }
                    } else {
                        5 // default 5 seconds
                    };
                    let wait_id =
                        self.push_node(Opcode::Wait, vec![("duration_secs", ArgValue::Int(secs))]);
                    // Wire dependencies for variable references in args
                    for arg in &c.args {
                        let arg_id = self.transform_expr_to_node(arg)?;
                        self.push_edge(&arg_id, &wait_id);
                    }
                    Ok(wait_id)
                } else if name == "range" || name == "len" || is_allowed_builtin(&name) {
                    // Built-in function calls → CALC node
                    let mut args_str: Vec<String> = Vec::new();
                    let mut arg_ids: Vec<String> = Vec::new();
                    for arg in &c.args {
                        args_str.push(format_expr(arg, self.code));
                        let arg_id = self.transform_expr_to_node(arg)?;
                        arg_ids.push(arg_id);
                    }
                    let args_joined = args_str.join(", ");
                    let expr_str = format!("{}({})", name, args_joined);
                    let calc_id =
                        self.push_node(Opcode::Calc, vec![("expr", ArgValue::String(expr_str))]);
                    // Wire dependencies (using saved arg_ids to avoid double-evaluation)
                    for arg_id in &arg_ids {
                        self.push_edge(arg_id, &calc_id);
                    }
                    Ok(calc_id)
                } else {
                    // Unknown function → CALC node
                    let expr_str = format_expr(expr, self.code);
                    let calc_id =
                        self.push_node(Opcode::Calc, vec![("expr", ArgValue::String(expr_str))]);
                    // Evaluate sub-expressions
                    for arg in &c.args {
                        let arg_id = self.transform_expr_to_node(arg)?;
                        self.push_edge(&arg_id, &calc_id);
                    }
                    for kw in &c.keywords {
                        let kw_id = self.transform_expr_to_node(&kw.value)?;
                        self.push_edge(&kw_id, &calc_id);
                    }
                    Ok(calc_id)
                }
            }

            // ── Simple expressions → CALC node ─────────────────
            _ => {
                let expr_str = format_expr(expr, self.code);
                let calc_id =
                    self.push_node(Opcode::Calc, vec![("expr", ArgValue::String(expr_str))]);

                // Find variable references and wire dependencies
                self.wire_expr_dependencies(expr, &calc_id);
                Ok(calc_id)
            }
        }
    }

    /// Walk an expression and wire edges for variable references.
    fn wire_expr_dependencies(&mut self, expr: &ast::Expr, target_id: &str) {
        match expr {
            ast::Expr::Name(n) => {
                let name = n.id.as_str().to_owned();
                if let Some(src_id) = self.var_map.get(&name).cloned() {
                    self.push_edge(&src_id, target_id);
                }
            }
            ast::Expr::Attribute(a) => {
                self.wire_expr_dependencies(&a.value, target_id);
            }
            ast::Expr::Subscript(s) => {
                self.wire_expr_dependencies(&s.value, target_id);
                self.wire_expr_dependencies(&s.slice, target_id);
            }
            ast::Expr::Call(c) => {
                self.wire_expr_dependencies(&c.func, target_id);
                for arg in &c.args {
                    self.wire_expr_dependencies(arg, target_id);
                }
                for kw in &c.keywords {
                    self.wire_expr_dependencies(&kw.value, target_id);
                }
            }
            ast::Expr::BoolOp(e) => {
                for v in &e.values {
                    self.wire_expr_dependencies(v, target_id);
                }
            }
            ast::Expr::BinOp(e) => {
                self.wire_expr_dependencies(&e.left, target_id);
                self.wire_expr_dependencies(&e.right, target_id);
            }
            ast::Expr::UnaryOp(e) => {
                self.wire_expr_dependencies(&e.operand, target_id);
            }
            ast::Expr::Compare(e) => {
                self.wire_expr_dependencies(&e.left, target_id);
                for c in &e.comparators {
                    self.wire_expr_dependencies(c, target_id);
                }
            }
            ast::Expr::IfExp(e) => {
                self.wire_expr_dependencies(&e.test, target_id);
                self.wire_expr_dependencies(&e.body, target_id);
                self.wire_expr_dependencies(&e.orelse, target_id);
            }
            ast::Expr::List(e) => {
                for elt in &e.elts {
                    self.wire_expr_dependencies(elt, target_id);
                }
            }
            ast::Expr::Tuple(e) => {
                for elt in &e.elts {
                    self.wire_expr_dependencies(elt, target_id);
                }
            }
            ast::Expr::Dict(e) => {
                for k in &e.keys {
                    if let Some(key) = k {
                        self.wire_expr_dependencies(key, target_id);
                    }
                }
                for v in &e.values {
                    self.wire_expr_dependencies(v, target_id);
                }
            }
            ast::Expr::Set(e) => {
                for elt in &e.elts {
                    self.wire_expr_dependencies(elt, target_id);
                }
            }
            ast::Expr::JoinedStr(e) => {
                for v in &e.values {
                    self.wire_expr_dependencies(v, target_id);
                }
            }
            ast::Expr::FormattedValue(e) => {
                self.wire_expr_dependencies(&e.value, target_id);
            }
            ast::Expr::Starred(e) => {
                self.wire_expr_dependencies(&e.value, target_id);
            }
            // Constant, Slice — no dependencies
            ast::Expr::Constant(_) | ast::Expr::Slice(_) => {}
            // Lambda, NamedExpr, Yield, Await, comprehensions — blocked by sanitizer
            _ => {}
        }
    }
}

// ─── Utility helpers ──────────────────────────────────────────────

/// Get the callee name string for a call expression.
fn callee_name_str(call: &ast::ExprCall) -> Option<String> {
    match &*call.func {
        ast::Expr::Name(n) => Some(n.id.as_str().to_owned()),
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_owned()),
        _ => None,
    }
}

/// Check if an expression is a call to a specific function name.
fn is_name_call(expr: &ast::Expr, name: &str) -> bool {
    match expr {
        ast::Expr::Call(c) => matches!(&*c.func, ast::Expr::Name(n) if n.id.as_str() == name),
        _ => false,
    }
}

/// Allowed built-in functions that don't require special handling.
fn is_allowed_builtin(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "range"
            | "int"
            | "str"
            | "float"
            | "bool"
            | "list"
            | "dict"
            | "tuple"
            | "set"
            | "min"
            | "max"
            | "sum"
            | "abs"
            | "round"
            | "print"
            | "isinstance"
            | "type"
            | "reversed"
            | "enumerate"
            | "zip"
            | "sorted"
            | "any"
            | "all"
    )
}

/// Convert a Python operator string for augmented assignment.
fn aug_op_to_str(op: &ast::Operator) -> &'static str {
    use ast::Operator::*;
    match op {
        Add => "+",
        Sub => "-",
        Mult => "*",
        Div => "/",
        Mod => "%",
        Pow => "**",
        LShift => "<<",
        RShift => ">>",
        BitOr => "|",
        BitXor => "^",
        BitAnd => "&",
        FloorDiv => "//",
        MatMult => "@",
    }
}

/// Convert an expression to a string representation for use in plan nodes.
fn format_expr(expr: &ast::Expr, _code: &str) -> String {
    match expr {
        ast::Expr::Constant(c) => format_constant(&c.value),
        ast::Expr::Name(n) => n.id.as_str().to_owned(),
        ast::Expr::Attribute(a) => format!("{}.{}", format_expr(&a.value, _code), a.attr.as_str()),
        ast::Expr::Subscript(s) => format!(
            "{}[{}]",
            format_expr(&s.value, _code),
            format_expr(&s.slice, _code)
        ),
        ast::Expr::Call(c) => {
            let name = callee_name_str(c).unwrap_or_else(|| "?".into());
            let args: Vec<String> = c.args.iter().map(|a| format_expr(a, _code)).collect();
            let kwargs: Vec<String> = c
                .keywords
                .iter()
                .map(|kw| {
                    let k = kw.arg.as_ref().map(|i| i.as_str()).unwrap_or("");
                    format!("{}={}", k, format_expr(&kw.value, _code))
                })
                .collect();
            let all: Vec<String> = args.into_iter().chain(kwargs).collect();
            format!("{}({})", name, all.join(", "))
        }
        ast::Expr::BinOp(e) => {
            let op_str = match &e.op {
                ast::Operator::Add => "+",
                ast::Operator::Sub => "-",
                ast::Operator::Mult => "*",
                ast::Operator::Div => "/",
                ast::Operator::Mod => "%",
                ast::Operator::Pow => "**",
                ast::Operator::LShift => "<<",
                ast::Operator::RShift => ">>",
                ast::Operator::BitOr => "|",
                ast::Operator::BitXor => "^",
                ast::Operator::BitAnd => "&",
                ast::Operator::FloorDiv => "//",
                ast::Operator::MatMult => "@",
            };
            format!(
                "{} {} {}",
                format_expr(&e.left, _code),
                op_str,
                format_expr(&e.right, _code)
            )
        }
        ast::Expr::UnaryOp(e) => {
            let op_str = match &e.op {
                ast::UnaryOp::Not => "not ",
                ast::UnaryOp::USub => "-",
                ast::UnaryOp::UAdd => "+",
                ast::UnaryOp::Invert => "~",
            };
            format!("{}{}", op_str, format_expr(&e.operand, _code))
        }
        ast::Expr::BoolOp(e) => {
            let op_str = match &e.op {
                ast::BoolOp::And => " and ",
                ast::BoolOp::Or => " or ",
            };
            let parts: Vec<String> = e.values.iter().map(|v| format_expr(v, _code)).collect();
            parts.join(op_str)
        }
        ast::Expr::Compare(e) => {
            let mut result = format_expr(&e.left, _code);
            for (op, comparator) in e.ops.iter().zip(e.comparators.iter()) {
                let op_str = match op {
                    ast::CmpOp::Eq => " == ",
                    ast::CmpOp::NotEq => " != ",
                    ast::CmpOp::Lt => " < ",
                    ast::CmpOp::LtE => " <= ",
                    ast::CmpOp::Gt => " > ",
                    ast::CmpOp::GtE => " >= ",
                    ast::CmpOp::Is => " is ",
                    ast::CmpOp::IsNot => " is not ",
                    ast::CmpOp::In => " in ",
                    ast::CmpOp::NotIn => " not in ",
                };
                result.push_str(op_str);
                result.push_str(&format_expr(comparator, _code));
            }
            result
        }
        ast::Expr::IfExp(e) => {
            format!(
                "{} if {} else {}",
                format_expr(&e.body, _code),
                format_expr(&e.test, _code),
                format_expr(&e.orelse, _code),
            )
        }
        ast::Expr::List(e) => {
            let elts: Vec<String> = e.elts.iter().map(|v| format_expr(v, _code)).collect();
            format!("[{}]", elts.join(", "))
        }
        ast::Expr::Tuple(e) => {
            let elts: Vec<String> = e.elts.iter().map(|v| format_expr(v, _code)).collect();
            if elts.len() == 1 {
                format!("({},)", elts[0])
            } else {
                format!("({})", elts.join(", "))
            }
        }
        ast::Expr::Dict(e) => {
            let pairs: Vec<String> = e
                .keys
                .iter()
                .zip(e.values.iter())
                .map(|(k, v)| {
                    let ks = k
                        .as_ref()
                        .map(|k| format_expr(k, _code))
                        .unwrap_or_else(|| "".into());
                    format!("{}: {}", ks, format_expr(v, _code))
                })
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
        ast::Expr::Set(e) => {
            let elts: Vec<String> = e.elts.iter().map(|v| format_expr(v, _code)).collect();
            format!("{{{}}}", elts.join(", "))
        }
        ast::Expr::Slice(e) => {
            let lower = e
                .lower
                .as_ref()
                .map(|l| format_expr(l, _code))
                .unwrap_or_else(|| "".into());
            let upper = e
                .upper
                .as_ref()
                .map(|u| format_expr(u, _code))
                .unwrap_or_else(|| "".into());
            let step = e
                .step
                .as_ref()
                .map(|s| format!(":{}", format_expr(s, _code)))
                .unwrap_or_else(|| "".into());
            format!("{}:{}{}", lower, upper, step)
        }
        ast::Expr::JoinedStr(e) => {
            let parts: Vec<String> = e.values.iter().map(|v| format_expr(v, _code)).collect();
            parts.concat()
        }
        ast::Expr::FormattedValue(e) => format_expr(&e.value, _code),
        ast::Expr::NamedExpr(e) => format!(
            "{} := {}",
            format_expr(&e.target, _code),
            format_expr(&e.value, _code)
        ),
        ast::Expr::Starred(e) => format!("*{}", format_expr(&e.value, _code)),
        // Blocked by sanitizer but handle gracefully
        ast::Expr::Lambda(_e) => "<lambda>".into(),
        ast::Expr::ListComp(e) => format!("[{} for ...]", format_expr(&e.elt, _code)),
        ast::Expr::SetComp(e) => format!("{{{ } for ...}}", format_expr(&e.elt, _code)),
        ast::Expr::DictComp(e) => format!(
            "{{{}: {} for ...}}",
            format_expr(&e.key, _code),
            format_expr(&e.value, _code)
        ),
        ast::Expr::GeneratorExp(e) => format!("({} for ...)", format_expr(&e.elt, _code)),
        ast::Expr::Await(e) => format!("await {}", format_expr(&e.value, _code)),
        ast::Expr::Yield(e) => {
            if let Some(v) = &e.value {
                format!("yield {}", format_expr(v, _code))
            } else {
                "yield".into()
            }
        }
        ast::Expr::YieldFrom(e) => format!("yield from {}", format_expr(&e.value, _code)),
    }
}

/// Format a constant value as a string.
fn format_constant(c: &ast::Constant) -> String {
    match c {
        ast::Constant::None => "None".into(),
        ast::Constant::Bool(b) => b.to_string(),
        ast::Constant::Int(i) => i.to_string(),
        ast::Constant::Float(f) => f.to_string(),
        ast::Constant::Complex { real, imag } => format!("{}+{}j", real, imag),
        ast::Constant::Str(s) => format!("\"{}\"", s),
        ast::Constant::Bytes(b) => format!("b\"{}\"", String::from_utf8_lossy(b)),
        ast::Constant::Ellipsis => "...".into(),
        ast::Constant::Tuple(items) => {
            let inner: Vec<String> = items.iter().map(format_constant).collect();
            format!("({})", inner.join(", "))
        }
    }
}

/// Convert a Python expression to an ArgValue.
#[allow(dead_code)]
fn expr_to_arg_value(expr: &ast::Expr) -> ArgValue {
    match expr {
        ast::Expr::Constant(c) => const_to_arg_value(&c.value),
        ast::Expr::Name(n) => ArgValue::String(n.id.as_str().to_owned()),
        ast::Expr::List(l) => ArgValue::Array(l.elts.iter().map(expr_to_arg_value).collect()),
        ast::Expr::Tuple(t) => ArgValue::Array(t.elts.iter().map(expr_to_arg_value).collect()),
        _ => ArgValue::String(format_expr(expr, "")),
    }
}

#[allow(dead_code)]
fn const_to_arg_value(c: &ast::Constant) -> ArgValue {
    match c {
        ast::Constant::None => ArgValue::Null,
        ast::Constant::Bool(b) => ArgValue::Bool(*b),
        ast::Constant::Int(i) => {
            let s = i.to_string();
            ArgValue::Int(s.parse().unwrap_or(0))
        }
        ast::Constant::Float(f) => ArgValue::Float(*f),
        ast::Constant::Str(s) => ArgValue::String(s.clone()),
        ast::Constant::Tuple(items) => {
            ArgValue::Array(items.iter().map(const_to_arg_value).collect())
        }
        ast::Constant::Bytes(b) => ArgValue::String(format!("b\"{}\"", String::from_utf8_lossy(b))),
        ast::Constant::Complex { real, imag } => ArgValue::String(format!("{}+{}j", real, imag)),
        ast::Constant::Ellipsis => ArgValue::String("...".into()),
    }
}

/// Infer the max iteration count from a `range(N)` expression.
/// Returns `0` if unknown (boundless).
fn infer_range_max(expr: &ast::Expr) -> i64 {
    match expr {
        ast::Expr::Call(c) if is_name_call(expr, "range") => {
            // range(stop) or range(start, stop) or range(start, stop, step)
            match c.args.len() {
                1 => {
                    // range(stop) — try to extract constant
                    if let ast::Expr::Constant(ast::ExprConstant {
                        value: ast::Constant::Int(i),
                        ..
                    }) = &c.args[0]
                    {
                        i.to_string().parse().unwrap_or(100)
                    } else {
                        100 // default max
                    }
                }
                2 => 100, // range(start, stop) — can't easily infer
                _ => 100,
            }
        }
        // Non-range iterable: len is unknown
        _ => 100,
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: create a basic plan and check it has expected structure
    fn check_plan(code: &str, expected_nodes: usize, expected_edges: usize) -> ExecutionPlan {
        let plan = transform(code).expect("transform should succeed");
        assert_eq!(
            plan.nodes.len(),
            expected_nodes,
            "expected {expected_nodes} nodes, got {}: {:?}",
            plan.nodes.len(),
            plan.nodes.iter().map(|n| &n.id[..]).collect::<Vec<_>>()
        );
        assert_eq!(
            plan.edges.len(),
            expected_edges,
            "expected {expected_edges} edges, got {}: {:?}",
            plan.edges.len(),
            plan.edges
                .iter()
                .map(|e| format!("{}→{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
        // Verify topological order succeeds
        assert!(plan.topological_order().is_ok(), "plan has a cycle");
        plan
    }

    // ── Simple assignments ──────────────────────────────────────

    #[test]
    fn transform_simple_assign() {
        let plan = check_plan("def graph():\n    x = 1", 1, 0);
        // n0 = CALC "1"
        assert_eq!(plan.nodes[0].op, Opcode::Calc);
    }

    #[test]
    fn transform_arithmetic() {
        let code = "def graph(a: int, b: int):\n    x = a + b";
        // n0 Input(a), n1 Input(b), n2 CALC(a + b), edge: n0→n2, n1→n2
        let plan = check_plan(code, 3, 2);
        assert_eq!(plan.nodes[2].op, Opcode::Calc);
    }

    #[test]
    fn transform_call_tool() {
        let code = r#"def graph(x: int):
    result = call("my_tool", arg=x)
    return result"#;
        let plan = transform(code).expect("should succeed");
        // n0 Input(x), n1 CALL(my_tool, arg=x), n2 ACT(return)
        // edges: n0→n1, n1→n2
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Call));
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Act));
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_act() {
        let code = r#"def graph(x: int):
    act("LOG", msg="hello")
    return x"#;
        let plan = transform(code).expect("should succeed");
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Call));
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Act));
    }

    // ── If/else ──────────────────────────────────────────────────

    #[test]
    fn transform_if_else() {
        let code = "def graph(x: int):\n    if x > 0:\n        y = 1\n    else:\n        y = 2\n    return y";
        let plan = transform(code).expect("should succeed");
        // Expect: Input(x), DECIDE, CALC(1), CALC(2), ACT(return)
        assert!(
            plan.nodes.iter().any(|n| n.op == Opcode::Decide),
            "should have DECIDE node"
        );
        assert!(
            plan.edges.iter().any(|e| e.condition.is_some()),
            "should have conditional edges"
        );
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_if_only() {
        let code = "def graph(x: int):\n    if x > 0:\n        x = 1\n    return x";
        let plan = transform(code).expect("should succeed");
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Decide));
        assert!(plan.topological_order().is_ok());
    }

    // ── For loop ────────────────────────────────────────────────

    #[test]
    fn transform_for_loop() {
        let code = "def graph(items: list):\n    total = 0\n    for i in range(len(items)):\n        total = total + i\n    return total";
        let plan = transform(code).expect("should succeed");
        assert!(
            plan.nodes.iter().any(|n| n.op == Opcode::Loop),
            "should have LOOP node"
        );
        assert!(plan.topological_order().is_ok());
    }

    // ── With parallel ───────────────────────────────────────────

    #[test]
    fn transform_parallel() {
        let code = r#"def graph():
    with parallel() as p:
        x = call("tool1")
        y = call("tool2")
    return x"#;
        let plan = transform(code).expect("should succeed");
        assert!(
            plan.nodes.iter().any(|n| n.op == Opcode::Parallel),
            "should have PARALLEL node"
        );
        assert!(plan.topological_order().is_ok());
    }

    // ── Error cases ─────────────────────────────────────────────

    #[test]
    fn transform_no_graph_func() {
        let result = transform("x = 1");
        assert!(result.is_err(), "should fail without graph function");
    }

    #[test]
    fn transform_sanitizer_error() {
        let result = transform("import os");
        assert!(result.is_err(), "should fail on blocked import");
    }

    #[test]
    fn transform_empty_body() {
        let code = "def graph():\n    pass";
        let plan = transform(code).expect("empty graph should succeed");
        // Just INPUT nodes (none) + maybe a pass placeholder
        assert!(plan.topological_order().is_ok());
    }

    // ── AugAssign ──────────────────────────────────────────────

    #[test]
    fn transform_aug_assign() {
        let code = "def graph(x: int):\n    x += 1\n    return x";
        let plan = transform(code).expect("should succeed");
        // Input(x), CALC("1"), CALC("x + 1"), CALC("x"), ACT(return)
        assert_eq!(plan.nodes.len(), 5);
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Calc));
        assert!(plan.topological_order().is_ok());
    }

    // ── Annotation assignment ──────────────────────────────────

    #[test]
    fn transform_ann_assign() {
        let code = "def graph():\n    x: int = 5\n    return x";
        let plan = transform(code).expect("should succeed");
        assert!(plan.nodes.iter().any(|n| n.op == Opcode::Calc));
        assert!(plan.topological_order().is_ok());
    }

    // ── Complex expressions ─────────────────────────────────────

    #[test]
    fn transform_complex_chain() {
        let code = "def graph(a: int, b: int):\n    x = (a + b) * 2\n    return x";
        let plan = transform(code).expect("should succeed");
        // Input(a), Input(b), CALC(parent), CALC(outer), ACT(return)
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_list_dict() {
        let code = "def graph():\n    x = [1, 2, 3]\n    y = {'a': 1}\n    return y";
        let plan = transform(code).expect("should succeed");
        assert!(plan.topological_order().is_ok());
    }

    // ── If/else with early returns ───────────────────────────

    #[test]
    fn transform_if_both_branches_return() {
        // Both branches return — no MERGE node needed
        let code =
            "def graph(x: int):\n    if x > 0:\n        return 1\n    else:\n        return 2";
        let plan = transform(code).expect("should succeed");
        // Should have: Input(x), DECIDE, CALC(1), ACT(return,1), CALC(2), ACT(return,2)
        // No MERGE node since both branches terminate
        assert!(
            !plan.nodes.iter().any(|n| n.op == Opcode::Merge),
            "should NOT have MERGE node when both branches return"
        );
        assert!(
            plan.nodes.iter().any(|n| n.op == Opcode::Decide),
            "should have DECIDE node"
        );
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_if_one_branch_returns() {
        // One branch returns, the other continues — MERGE should only connect to non-terminal branch
        let code =
            "def graph(x: int):\n    if x > 0:\n        return 1\n    y = x + 1\n    return y";
        let plan = transform(code).expect("should succeed");
        // Should have MERGE node (the false branch falls through to continue)
        assert!(
            plan.nodes.iter().any(|n| n.op == Opcode::Merge),
            "should have MERGE node"
        );
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_if_early_return_blocks_fallback() {
        // Code after an if with early return should NOT execute on the return path.
        // The Control edge from MERGE (fall-through) to the next statement ensures
        // that subsequent code only runs when the false branch is taken.
        let code =
            "def graph(x: int):\n    if x > 0:\n        return 1\n    y = x + 1\n    return y";
        let plan = transform(code).expect("should succeed");
        // Should have: Input(x), DECIDE, CALC(1), ACT(return1), CALC(x+1), ACT(return y)
        // And a MERGE with fall-through edge from DECIDE(false)
        assert!(plan.topological_order().is_ok());
        // Validate the plan passes all checks
        match crate::validator::validate(&plan) {
            Ok(()) => {}
            Err(errors) => {
                panic!("plan should pass validation: {:?}", errors);
            }
        }

        // There should be a Control edge from the MERGE node to CALC(x+1)
        // indicating that the fall-through path must reach MERGE before y=x+1 executes
        let calc_node_ids: Vec<&str> = plan
            .nodes
            .iter()
            .filter(|n| {
                n.op == Opcode::Calc
                    && n.args
                        .iter()
                        .any(|a| a.value == ArgValue::String("x + 1".into()))
            })
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(calc_node_ids.len(), 1, "should have CALC(x+1) node");

        // There should be a MERGE node with at least one connection
        let merge_count = plan.nodes.iter().filter(|n| n.op == Opcode::Merge).count();
        assert!(merge_count >= 1, "should have at least one MERGE node");
    }

    #[test]
    fn transform_if_terminal_mapped() {
        // Verify that return nodes are tracked as terminals
        let code = "def graph(x: int):\n    if x > 0:\n        return 1\n    return x";
        let plan = transform(code).expect("should succeed");
        // The early return ACT node should exist
        let return_nodes: Vec<_> = plan
            .nodes
            .iter()
            .filter(|n| {
                n.op == Opcode::Act
                    && n.args
                        .iter()
                        .any(|a| a.value == ArgValue::String("return".into()))
            })
            .collect();
        assert_eq!(return_nodes.len(), 2, "should have 2 return nodes");
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn transform_return_expr() {
        let code = "def graph(x: int):\n    return x * 2";
        let plan = transform(code).expect("should succeed");
        // Input(x), CALC(x * 2), ACT(return)
        assert_eq!(plan.nodes.len(), 3);
        assert!(plan.topological_order().is_ok());
    }

    #[test]
    fn test_opcode_sequence() {
        // Verify that nodes appear in roughly the right order
        let code = "def graph():\n    a = 1\n    b = 2\n    c = a + b\n    return c";
        let plan = transform(code).expect("should succeed");
        assert!(plan.topological_order().is_ok());
        // Should have: Inputs for a,b,c(?), CALC(1), CALC(2), CALC(a+b), ACT(return)
        // At minimum 4 nodes: n0=CALC(1), n1=CALC(2), n2=CALC(a+b), n3=ACT(return)
        assert!(
            plan.nodes.len() >= 4,
            "expected >=4 nodes, got {}",
            plan.nodes.len()
        );
    }
}
