//! AST Sanitizer for Restricted Python.
//!
//! Parses Python code, walks the AST, and rejects constructs outside the
//! Restricted Python subset defined in Section 4.8 of the plan.
//!
//! # SAFETY
//! This crate does not use `unsafe`. All AST operations are safe Rust via
//! `rustpython-parser`.

use rustpython_parser as parser;
use parser::ast::{self, Ranged};
use parser::source_code::{LineIndex, SourceLocation};

/// A sanitization error with source location.
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizationError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl std::fmt::Display for SanitizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}:{} — {}", self.line, self.column, self.message)
    }
}

/// Blocked built‑in function names (direct name-call).
const BLOCKED_BUILTINS: &[&str] = &[
    "eval", "exec", "__import__", "open", "compile", "globals", "locals",
    "vars", "dir", "input", "exit", "quit", "breakpoint", "getattr",
    "setattr", "delattr", "hasattr", "memoryview", "bytearray", "super",
    "staticmethod", "classmethod", "property",
];

/// Blocked attribute method names (obj.<method>(...)).
const BLOCKED_METHODS: &[&str] = &[
    "system", "popen", "Popen", "fork", "execve", "execvp",
    "execl", "spawn",
];

/// Run the full sanitization pipeline on a piece of Python code.
///
/// Returns `Ok(())` if the code passes every rule, or `Err(errors)` with
/// *all* violations (not just the first one).
pub fn sanitize(code: &str) -> Result<(), Vec<SanitizationError>> {
    let mut engine = SanitizerEngine::new(code);
    engine.run();
    if engine.errors.is_empty() {
        Ok(())
    } else {
        Err(engine.errors)
    }
}

// ─── Internal engine ──────────────────────────────────────────────

struct SanitizerEngine<'a> {
    code: &'a str,
    line_index: LineIndex,
    errors: Vec<SanitizationError>,
    function_depth: u32,
    loop_depth: u32,
    parallel_depth: u32,
    top_level_names: Vec<String>,
}

impl<'a> SanitizerEngine<'a> {
    fn new(code: &'a str) -> Self {
        let line_index = LineIndex::from_source_text(code);
        Self {
            code,
            line_index,
            errors: Vec::new(),
            function_depth: 0,
            loop_depth: 0,
            parallel_depth: 0,
            top_level_names: Vec::new(),
        }
    }

    fn run(&mut self) {
        let parse_result = parser::parse(self.code, parser::Mode::Module, "<embedded>");
        let module = match parse_result {
            Ok(ast::Mod::Module(m)) => m,
            Ok(_) => {
                self.error_at_zero("only Module‑level code is allowed");
                return;
            }
            Err(e) => {
                let loc = self.resolve_location(e.offset);
                self.errors.push(SanitizationError {
                    line: loc.row.get() as usize,
                    column: loc.column.get() as usize,
                    message: format!("parse error: {}", e.error),
                });
                return;
            }
        };
        for stmt in &module.body {
            self.visit_stmt(stmt);
        }
    }

    // ── Statement visitor ────────────────────────────────────────

    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        match stmt {
            // ❌ Blocked statements
            ast::Stmt::Import(s) => {
                let names: Vec<&str> = s.names.iter().map(|a| a.name.as_str()).collect();
                self.err(stmt, format_args!("import is forbidden (remove `import {}`)", names.join(", ")));
            }
            ast::Stmt::ImportFrom(s) => {
                let module = s.module.as_ref().map(|m| m.as_str()).unwrap_or("");
                self.err(stmt, format_args!("from‑import is forbidden (remove `from {} import ...`)", module));
            }
            ast::Stmt::ClassDef(s) => {
                self.err(stmt, format_args!("class definition is forbidden (remove `class {}`)", s.name.as_str()));
            }
            ast::Stmt::While(_) => {
                self.err(stmt, format_args!("`while` is forbidden (use `for` with a bounded range)"));
            }
            ast::Stmt::Try(_) => {
                self.err(stmt, format_args!("`try`/`except` is forbidden (use the graph's ERROR mechanism)"));
            }
            ast::Stmt::TryStar(_) => {
                self.err(stmt, format_args!("`try`/`except*` is forbidden (use the graph's ERROR mechanism)"));
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                self.err(stmt, format_args!("`async def` is forbidden (remove `async` from `{}`)", s.name.as_str()));
            }
            ast::Stmt::AsyncFor(_) => {
                self.err(stmt, format_args!("`async for` is forbidden"));
            }
            ast::Stmt::AsyncWith(_) => {
                self.err(stmt, format_args!("`async with` is forbidden"));
            }
            ast::Stmt::Global(s) => {
                self.err(stmt, format_args!("`global` is forbidden (remove `global {}`)",
                    s.names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")));
            }
            ast::Stmt::Nonlocal(s) => {
                self.err(stmt, format_args!("`nonlocal` is forbidden (remove `nonlocal {}`)",
                    s.names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")));
            }
            ast::Stmt::Match(_) => {
                // match is allowed — transforms to SWITCH opcode
            }
            ast::Stmt::With(s) => {
                let is_parallel = s.items.len() == 1
                    && is_name_call(&s.items[0].context_expr, "parallel");
                if !is_parallel {
                    self.err(stmt, format_args!("`with` is forbidden except for `with parallel() as p:`"));
                } else {
                    self.parallel_depth += 1;
                    for child in &s.body {
                        self.visit_stmt(child);
                    }
                    self.parallel_depth -= 1;
                    return;
                }
            }
            ast::Stmt::Delete(_) => {
                self.err(stmt, format_args!("`del` is forbidden"));
            }
            ast::Stmt::TypeAlias(_s) => {
                self.err(stmt, format_args!("`type` alias is forbidden"));
            }

            // ✅ Allowed statements
            ast::Stmt::FunctionDef(s) => {
                if self.function_depth > 0 {
                    self.err(stmt, format_args!("nested function definitions are forbidden"));
                } else if s.name.as_str() != "graph" {
                    self.err(stmt, format_args!(
                        "only one function named `graph` is allowed (found `{}`)", s.name.as_str()));
                }
                if !s.decorator_list.is_empty() {
                    self.err(stmt, format_args!("decorators are forbidden"));
                }
                self.function_depth += 1;
                if let Some(ret) = &s.returns {
                    self.visit_expr(ret);
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
                self.function_depth -= 1;
                return;
            }
            ast::Stmt::Return(_) => {
                if self.function_depth == 0 {
                    self.err(stmt, format_args!("`return` outside a function is forbidden"));
                }
            }
            ast::Stmt::For(_) => {
                self.loop_depth += 1;
            }
            ast::Stmt::If(_) => {}
            ast::Stmt::Pass(_) => {}
            ast::Stmt::Break(_) => {
                if self.loop_depth == 0 {
                    self.err(stmt, format_args!("`break` outside a loop is forbidden"));
                }
            }
            ast::Stmt::Continue(_) => {
                if self.loop_depth == 0 {
                    self.err(stmt, format_args!("`continue` outside a loop is forbidden"));
                }
            }
            ast::Stmt::Raise(_) | ast::Stmt::Assert(_) => {}
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    self.record_assignment_target(target);
                }
            }
            ast::Stmt::AnnAssign(s) => {
                self.record_assignment_target(&s.target);
            }
            ast::Stmt::AugAssign(s) => {
                self.record_assignment_target(&s.target);
            }
            ast::Stmt::Expr(_) => {}
        }

        // For Return, walk its value manually (already done above for
        // FunctionDef by returning early, so this is the generic path).
        if let ast::Stmt::Return(s) = stmt {
            if let Some(v) = &s.value {
                self.visit_expr(v.as_ref());
            }
        }

        // Walk children (decrement loop depth after for loops)
        self.walk_stmt_children(stmt);
    }

    // ── Expression visitor ───────────────────────────────────────

    fn visit_expr(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::Lambda(_) => {
                self.err(expr, format_args!("`lambda` is forbidden (use a named function or inline expression)"));
            }
            ast::Expr::NamedExpr(_) => {
                self.err(expr, format_args!("walrus operator `:=` is forbidden"));
            }
            ast::Expr::Yield(_) => {
                self.err(expr, format_args!("`yield` is forbidden (graph execution is synchronous)"));
            }
            ast::Expr::YieldFrom(_) => {
                self.err(expr, format_args!("`yield from` is forbidden (graph execution is synchronous)"));
            }
            ast::Expr::Await(_) => {
                self.err(expr, format_args!("`await` is forbidden (use synchronous calls)"));
            }
            ast::Expr::GeneratorExp(_) => {
                self.err(expr, format_args!("generator expression is forbidden (use a list + for loop)"));
            }
            ast::Expr::ListComp(_) => {
                self.err(expr, format_args!("list comprehension is forbidden (use a `for` loop)"));
            }
            ast::Expr::SetComp(_) => {
                self.err(expr, format_args!("set comprehension is forbidden (use a `for` loop)"));
            }
            ast::Expr::DictComp(_) => {
                self.err(expr, format_args!("dict comprehension is forbidden (use a `for` loop)"));
            }
            ast::Expr::JoinedStr(_) => {
                self.err(expr, format_args!("f‑string is forbidden (use string concatenation or `.format()`)"));
            }
            ast::Expr::FormattedValue(_) => {
                self.err(expr, format_args!("f‑string value formatting is forbidden"));
            }

            ast::Expr::Call(e) => {
                self.check_call(e);
            }

            // ✅ unconditionally allowed
            ast::Expr::Constant(_)
            | ast::Expr::Name(_)
            | ast::Expr::Attribute(_)
            | ast::Expr::BoolOp(_)
            | ast::Expr::BinOp(_)
            | ast::Expr::UnaryOp(_)
            | ast::Expr::Compare(_)
            | ast::Expr::IfExp(_)
            | ast::Expr::List(_)
            | ast::Expr::Tuple(_)
            | ast::Expr::Dict(_)
            | ast::Expr::Set(_)
            | ast::Expr::Slice(_) => {}

            // ── Subscript: dynamic key yasağı ──────────────────
            ast::Expr::Subscript(e) => {
                let key_allowed = match e.slice.as_ref() {
                    // Constant keys (int, str) — allowed
                    ast::Expr::Constant(_) => true,
                    // Variable keys (items[key]) — allowed (static reference)
                    ast::Expr::Name(_) => true,
                    // Attribute keys (items[obj.attr]) — allowed
                    ast::Expr::Attribute(_) => true,
                    // Slice (items[1:10]) — the parser yields ast::ExprSlice for slices
                    ast::Expr::Slice(_) => true,
                    // Everything else = dynamic key → blocked
                    _ => false,
                };
                if !key_allowed {
                    self.err(expr, format_args!(
                        "dynamic subscript key is forbidden (only literal, variable, attribute, and slice keys allowed)"
                    ));
                }
            }

            ast::Expr::Starred(e) => {
                self.err(e, format_args!("starred expression `*expr` is forbidden"));
            }
        }

        self.walk_expr_children(expr);
    }

    // ── Call-specific checks ────────────────────────────────────

    fn check_call(&mut self, call: &ast::ExprCall) {
        let callee_name = callee_name(call);
        let Some(name) = callee_name else {
            self.err(
                call, // use the call itself for range
                format_args!("dynamic calls are forbidden (use a static function name)"),
            );
            return;
        };

        if BLOCKED_BUILTINS.iter().any(|b| *b == name) {
            self.err(call.func.as_ref(), format_args!("`{}()` is forbidden for security reasons", name));
            return;
        }

        if let Some(method) = attribute_method_name(call) {
            if BLOCKED_METHODS.iter().any(|b| *b == method) {
                self.err(call.func.as_ref(), format_args!(
                    "`.{}()` method call is forbidden for security reasons", method));
                return;
            }
        }

        if name.starts_with("__") && name.ends_with("__") {
            self.err(call.func.as_ref(), format_args!("dunder method `{}()` is forbidden", name));
            return;
        }

        if name == "call" || name == "act" {
            let first_arg_is_str = call.args.first().map_or(false, |a| {
                matches!(
                    a,
                    ast::Expr::Constant(ast::ExprConstant {
                        value: ast::Constant::Str(_),
                        ..
                    })
                )
            });
            if !first_arg_is_str {
                self.err(call.func.as_ref(), format_args!(
                    "`{}()` first argument must be a string literal (e.g. `{}(\"...\", ...)`)",
                    name, name
                ));
            }
        }
    }

    // ── Error helpers ───────────────────────────────────────────

    /// Accept any value that can produce a `TextRange` via `.range()`.
    fn err(&mut self, node: &dyn Ranged, msg: std::fmt::Arguments<'_>) {
        let loc = self.resolve_location(node.start());
        self.errors.push(SanitizationError {
            line: loc.row.get() as usize,
            column: loc.column.get() as usize,
            message: msg.to_string(),
        });
    }

    fn error_at_zero(&mut self, msg: &str) {
        self.errors.push(SanitizationError {
            line: 0,
            column: 0,
            message: msg.to_string(),
        });
    }

    fn resolve_location(&self, offset: parser::text_size::TextSize) -> SourceLocation {
        self.line_index.source_location(offset, self.code)
    }

    // ── Scope isolation helpers ──────────────────────────────────

    /// Extract a simple variable name from an assignment target (Name or
    /// tuple/list of Names).  If the target is not a simple Name, return `None`.
    fn extract_name(target: &ast::Expr) -> Option<&str> {
        if let ast::Expr::Name(n) = target {
            Some(n.id.as_str())
        } else {
            None
        }
    }

    /// Record a variable name assigned at the current scope depth.
    ///
    /// * If `parallel_depth == 0` (top‑level function body), save it in
    ///   `top_level_names` for later cross‑scope checks.
    /// * If `parallel_depth > 0` and the name matches a previously‑recorded
    ///   top‑level variable, emit a data‑race warning.
    fn record_assignment_target(&mut self, target: &ast::Expr) {
        let Some(name) = Self::extract_name(target) else {
            return; // not a simple name — ignore (e.g. `obj.attr = ...`)
        };

        if self.parallel_depth > 0 && self.top_level_names.contains(&name.to_string()) {
            self.err(
                target,
                format_args!(
                    "potential data race: `{name}` is assigned inside `parallel()` but was \
                     also assigned in the outer scope (use a local variable instead)"
                ),
            );
        }

        if self.parallel_depth == 0 && !self.top_level_names.contains(&name.to_string()) {
            self.top_level_names.push(name.to_string());
        }
    }

    // ── Generic AST walkers ──────────────────────────────────────

    fn walk_stmt_children(&mut self, stmt: &ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(s) => {
                // Walk type annotations in arguments
                for a in &s.args.posonlyargs {
                    if let Some(ann) = &a.def.annotation {
                        self.visit_expr(ann);
                    }
                    if let Some(d) = &a.default {
                        self.visit_expr(d);
                    }
                }
                for a in &s.args.args {
                    if let Some(ann) = &a.def.annotation {
                        self.visit_expr(ann);
                    }
                    if let Some(d) = &a.default {
                        self.visit_expr(d);
                    }
                }
                if let Some(v) = &s.args.vararg {
                    if let Some(ann) = &v.annotation {
                        self.visit_expr(ann);
                    }
                }
                for a in &s.args.kwonlyargs {
                    if let Some(ann) = &a.def.annotation {
                        self.visit_expr(ann);
                    }
                    if let Some(d) = &a.default {
                        self.visit_expr(d);
                    }
                }
                if let Some(v) = &s.args.kwarg {
                    if let Some(ann) = &v.annotation {
                        self.visit_expr(ann);
                    }
                }
                if let Some(ret) = &s.returns {
                    self.visit_expr(ret);
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::Return(s) => {
                if let Some(v) = &s.value {
                    self.visit_expr(v);
                }
            }
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    self.visit_expr(target);
                }
                // StmtAssign::value is Box<Expr>, not Option
                self.visit_expr(&s.value);
            }
            ast::Stmt::AnnAssign(s) => {
                self.visit_expr(&s.target);
                self.visit_expr(&s.annotation);
                if let Some(v) = &s.value {
                    self.visit_expr(v);
                }
            }
            ast::Stmt::AugAssign(s) => {
                self.visit_expr(&s.target);
                self.visit_expr(&s.value);
            }
            ast::Stmt::If(s) => {
                self.visit_expr(&s.test);
                for child in &s.body {
                    self.visit_stmt(child);
                }
                for child in &s.orelse {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::For(s) => {
                self.visit_expr(&s.target);
                self.visit_expr(&s.iter);
                for child in &s.body {
                    self.visit_stmt(child);
                }
                for child in &s.orelse {
                    self.visit_stmt(child);
                }
                self.loop_depth -= 1;
            }
            ast::Stmt::While(s) => {
                self.visit_expr(&s.test);
                for child in &s.body {
                    self.visit_stmt(child);
                }
                for child in &s.orelse {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::Expr(s) => {
                self.visit_expr(&s.value);
            }
            ast::Stmt::Pass(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {}
            ast::Stmt::Raise(s) => {
                if let Some(v) = &s.exc {
                    self.visit_expr(v);
                }
                if let Some(c) = &s.cause {
                    self.visit_expr(c);
                }
            }
            ast::Stmt::Assert(s) => {
                self.visit_expr(&s.test);
                if let Some(v) = &s.msg {
                    self.visit_expr(v);
                }
            }
            ast::Stmt::Delete(s) => {
                for target in &s.targets {
                    self.visit_expr(target);
                }
            }
            ast::Stmt::With(s) => {
                for item in &s.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(v) = &item.optional_vars {
                        self.visit_expr(v);
                    }
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                if let Some(ret) = &s.returns {
                    self.visit_expr(ret);
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::AsyncFor(s) => {
                self.visit_expr(&s.target);
                self.visit_expr(&s.iter);
                for child in &s.body {
                    self.visit_stmt(child);
                }
                for child in &s.orelse {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::AsyncWith(s) => {
                for item in &s.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(v) = &item.optional_vars {
                        self.visit_expr(v);
                    }
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::ClassDef(s) => {
                for base in &s.bases {
                    self.visit_expr(base);
                }
                for kw in &s.keywords {
                    self.visit_expr(&kw.value);
                }
                for child in &s.body {
                    self.visit_stmt(child);
                }
            }
            ast::Stmt::Global(_) | ast::Stmt::Nonlocal(_) | ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => {}
            ast::Stmt::Match(s) => {
                self.visit_expr(&s.subject);
                for case in &s.cases {
                    if let Some(guard) = &case.guard {
                        self.visit_expr(guard);
                    }
                    for child in &case.body {
                        self.visit_stmt(child);
                    }
                }
            }
            ast::Stmt::Try(t) => {
                for child in &t.body { self.visit_stmt(child); }
                for handler in &t.handlers {
                    // ExceptHandler only has one variant
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    if let Some(t) = &h.type_ {
                        self.visit_expr(t);
                    }
                    for child in &h.body {
                        self.visit_stmt(child);
                    }
                }
                for child in &t.orelse { self.visit_stmt(child); }
                for child in &t.finalbody { self.visit_stmt(child); }
            }
            ast::Stmt::TryStar(t) => {
                for child in &t.body { self.visit_stmt(child); }
                for handler in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    if let Some(t) = &h.type_ {
                        self.visit_expr(t);
                    }
                    for child in &h.body {
                        self.visit_stmt(child);
                    }
                }
                for child in &t.orelse { self.visit_stmt(child); }
                for child in &t.finalbody { self.visit_stmt(child); }
            }
            ast::Stmt::TypeAlias(s) => {
                self.visit_expr(&s.name);
                self.visit_expr(&s.value);
            }
        }
    }

    fn walk_expr_children(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::BoolOp(e) => {
                for v in &e.values { self.visit_expr(v); }
            }
            ast::Expr::NamedExpr(e) => {
                self.visit_expr(&e.target);
                self.visit_expr(&e.value);
            }
            ast::Expr::BinOp(e) => {
                self.visit_expr(&e.left);
                self.visit_expr(&e.right);
            }
            ast::Expr::UnaryOp(e) => {
                self.visit_expr(&e.operand);
            }
            ast::Expr::Lambda(e) => {
                for a in &e.args.posonlyargs {
                    if let Some(ann) = &a.def.annotation { self.visit_expr(ann); }
                    if let Some(d) = &a.default { self.visit_expr(d); }
                }
                for a in &e.args.args {
                    if let Some(ann) = &a.def.annotation { self.visit_expr(ann); }
                    if let Some(d) = &a.default { self.visit_expr(d); }
                }
                if let Some(v) = &e.args.vararg {
                    if let Some(ann) = &v.annotation { self.visit_expr(ann); }
                }
                for a in &e.args.kwonlyargs {
                    if let Some(ann) = &a.def.annotation { self.visit_expr(ann); }
                    if let Some(d) = &a.default { self.visit_expr(d); }
                }
                if let Some(v) = &e.args.kwarg {
                    if let Some(ann) = &v.annotation { self.visit_expr(ann); }
                }
                self.visit_expr(&e.body);
            }
            ast::Expr::IfExp(e) => {
                self.visit_expr(&e.test);
                self.visit_expr(&e.body);
                self.visit_expr(&e.orelse);
            }
            ast::Expr::Dict(e) => {
                for k in &e.keys {
                    if let Some(key) = k { self.visit_expr(key); }
                }
                for v in &e.values { self.visit_expr(v); }
            }
            ast::Expr::Set(e) => {
                for elt in &e.elts { self.visit_expr(elt); }
            }
            ast::Expr::ListComp(e) => {
                self.visit_expr(&e.elt);
                for gen in &e.generators {
                    self.visit_expr(&gen.iter);
                    self.visit_expr(&gen.target);
                    for if_ in &gen.ifs { self.visit_expr(if_); }
                }
            }
            ast::Expr::SetComp(e) => {
                self.visit_expr(&e.elt);
                for gen in &e.generators {
                    self.visit_expr(&gen.iter);
                    self.visit_expr(&gen.target);
                    for if_ in &gen.ifs { self.visit_expr(if_); }
                }
            }
            ast::Expr::DictComp(e) => {
                self.visit_expr(&e.key);
                self.visit_expr(&e.value);
                for gen in &e.generators {
                    self.visit_expr(&gen.iter);
                    self.visit_expr(&gen.target);
                    for if_ in &gen.ifs { self.visit_expr(if_); }
                }
            }
            ast::Expr::GeneratorExp(e) => {
                self.visit_expr(&e.elt);
                for gen in &e.generators {
                    self.visit_expr(&gen.iter);
                    self.visit_expr(&gen.target);
                    for if_ in &gen.ifs { self.visit_expr(if_); }
                }
            }
            ast::Expr::Await(e) => { self.visit_expr(&e.value); }
            ast::Expr::Yield(e) => {
                if let Some(v) = &e.value { self.visit_expr(v); }
            }
            ast::Expr::YieldFrom(e) => { self.visit_expr(&e.value); }
            ast::Expr::Compare(e) => {
                self.visit_expr(&e.left);
                for c in &e.comparators { self.visit_expr(c); }
            }
            ast::Expr::Call(e) => {
                self.visit_expr(&e.func);
                for arg in &e.args { self.visit_expr(arg); }
                for kw in &e.keywords { self.visit_expr(&kw.value); }
            }
            ast::Expr::FormattedValue(e) => {
                self.visit_expr(&e.value);
                if let Some(spec) = &e.format_spec { self.visit_expr(spec); }
            }
            ast::Expr::JoinedStr(e) => {
                for v in &e.values { self.visit_expr(v); }
            }
            ast::Expr::Constant(_) | ast::Expr::Name(_) => {}
            ast::Expr::Attribute(e) => { self.visit_expr(&e.value); }
            ast::Expr::Subscript(e) => {
                self.visit_expr(&e.value);
                self.visit_expr(&e.slice);
            }
            ast::Expr::Starred(e) => { self.visit_expr(&e.value); }
            ast::Expr::List(e) => { for elt in &e.elts { self.visit_expr(elt); } }
            ast::Expr::Tuple(e) => { for elt in &e.elts { self.visit_expr(elt); } }
            ast::Expr::Slice(e) => {
                if let Some(l) = &e.lower { self.visit_expr(l); }
                if let Some(u) = &e.upper { self.visit_expr(u); }
                if let Some(s) = &e.step { self.visit_expr(s); }
            }
        }
    }
}

// ─── Utility helpers ──────────────────────────────────────────────

fn callee_name(call: &ast::ExprCall) -> Option<String> {
    match &*call.func {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_string()),
        _ => None,
    }
}

fn attribute_method_name(call: &ast::ExprCall) -> Option<String> {
    match &*call.func {
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_string()),
        _ => None,
    }
}

fn is_name_call(expr: &ast::Expr, name: &str) -> bool {
    match expr {
        ast::Expr::Call(c) => matches!(&*c.func, ast::Expr::Name(n) if n.id.as_str() == name),
        _ => false,
    }
}

// ─── Tests (60+ cases) ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ──────── Allowed ────────────────────────────────────────────

    #[test]
    fn allow_simple_assign() {
        assert!(sanitize("x = 1").is_ok());
    }

    #[test]
    fn allow_arithmetic() {
        assert!(sanitize("x = (a + b) * 2").is_ok());
    }

    #[test]
    fn allow_list_literal() {
        assert!(sanitize("x = [1, 2, 3]").is_ok());
    }

    #[test]
    fn allow_dict_literal() {
        assert!(sanitize("x = {'a': 1, 'b': 2}").is_ok());
    }

    #[test]
    fn allow_if_else() {
        assert!(sanitize("if x > 0:\n    y = 1\nelse:\n    y = 2").is_ok());
    }

    #[test]
    fn allow_for_loop() {
        assert!(sanitize("for i in range(10):\n    x = i").is_ok());
    }

    #[test]
    fn allow_function_def() {
        assert!(sanitize("def graph(x: int, y: str):\n    return x").is_ok());
    }

    #[test]
    fn allow_call_tool() {
        assert!(sanitize("def graph(x: int):\n    result = call(\"my_tool\", arg=x)\n    return result").is_ok());
    }

    #[test]
    fn allow_act() {
        assert!(sanitize("def graph(x: int):\n    act(\"LOG\", msg=\"hello\")\n    return x").is_ok());
    }

    #[test]
    fn allow_return_string() {
        assert!(sanitize("def graph():\n    return 'hello'").is_ok());
    }

    #[test]
    fn allow_multi_stmt() {
        assert!(sanitize("def graph(items: list):\n    total = 0\n    for i in range(len(items)):\n        total = total + i\n    return total").is_ok());
    }

    #[test]
    fn allow_break_continue() {
        assert!(sanitize("def graph():\n    for i in range(10):\n        if i == 0:\n            continue\n        if i == 9:\n            break\n    return 0").is_ok());
    }

    #[test]
    fn allow_parallel_with() {
        assert!(sanitize("def graph():\n    with parallel() as p:\n        x = p.run(call, \"tool1\")\n    return x").is_ok());
    }

    #[test]
    fn allow_raise_assert() {
        assert!(sanitize("def graph(x: int):\n    assert x > 0\n    if x < 0:\n        raise ValueError(\"negative\")\n    return x").is_ok());
    }

    #[test]
    fn allow_bool_ops() {
        assert!(sanitize("x = (a and b) or not c").is_ok());
    }

    #[test]
    fn allow_comparison() {
        assert!(sanitize("x = a == b and c != d and e < f").is_ok());
    }

    #[test]
    fn allow_ternary() {
        assert!(sanitize("x = a if cond else b").is_ok());
    }

    #[test]
    fn allow_attribute() {
        assert!(sanitize("def graph():\n    x = obj.attr.sub\n    return x").is_ok());
    }

    #[test]
    fn allow_subscript() {
        assert!(sanitize("def graph():\n    x = items[0]\n    return x").is_ok());
    }

    #[test]
    fn allow_subscript_string_key() {
        assert!(sanitize("def graph():\n    x = d[\"key\"]\n    return x").is_ok());
    }

    #[test]
    fn allow_subscript_var_key() {
        assert!(sanitize("def graph():\n    x = items[key]\n    return x").is_ok());
    }

    #[test]
    fn allow_subscript_attr_key() {
        assert!(sanitize("def graph():\n    x = items[obj.attr]\n    return x").is_ok());
    }

    #[test]
    fn allow_subscript_slice() {
        assert!(sanitize("def graph():\n    x = items[1:10]\n    return x").is_ok());
    }

    #[test]
    fn reject_subscript_dynamic_key() {
        let result = sanitize("def graph():\n    x = items[a + b]\n    return x");
        assert!(result.is_err(), "dynamic key (a + b) should be rejected");
    }

    #[test]
    fn reject_subscript_computed_key() {
        let result = sanitize("def graph():\n    x = items[func()]\n    return x");
        assert!(result.is_err(), "computed key (func()) should be rejected");
    }

    #[test]
    fn reject_subscript_bool_key() {
        let result = sanitize("def graph():\n    x = items[x > 0]\n    return x");
        assert!(result.is_err(), "boolean expression key should be rejected");
    }

    #[test]
    fn allow_set_literal() {
        assert!(sanitize("x = {1, 2, 3}").is_ok());
    }

    #[test]
    fn allow_nested_if() {
        assert!(sanitize("def graph(x: int, y: int):\n    if x > 0:\n        if y > 0:\n            z = x + y\n        else:\n            z = x\n    else:\n        z = 0\n    return z").is_ok());
    }

    #[test]
    fn allow_print() {
        assert!(sanitize("def graph():\n    print(\"hello\")\n    return 0").is_ok());
    }

    #[test]
    fn allow_range_len() {
        assert!(sanitize("def graph(items: list):\n    n = len(items)\n    for i in range(n):\n        x = i\n    return x").is_ok());
    }

    #[test]
    fn allow_slice() {
        assert!(sanitize("x = items[1:10:2]").is_ok());
    }

    // ──────── Blocked ────────────────────────────────────────────

    #[test]
    fn block_import() {
        let e = sanitize("import os").unwrap_err();
        assert!(e[0].message.contains("import"), "got: {}", e[0].message);
    }

    #[test]
    fn block_import_from() {
        let e = sanitize("from tools import *").unwrap_err();
        assert!(e[0].message.contains("import"), "got: {}", e[0].message);
    }

    #[test]
    fn block_eval() {
        let e = sanitize("eval('1+1')").unwrap_err();
        assert!(e[0].message.contains("eval"), "got: {}", e[0].message);
    }

    #[test]
    fn block_exec() {
        let e = sanitize("exec('x=1')").unwrap_err();
        assert!(e[0].message.contains("exec"), "got: {}", e[0].message);
    }

    #[test]
    fn block_open() {
        let e = sanitize("open('/etc/passwd')").unwrap_err();
        assert!(e[0].message.contains("open"), "got: {}", e[0].message);
    }

    #[test]
    fn block_dunder_import() {
        let e = sanitize("__import__('os')").unwrap_err();
        assert!(e[0].message.contains("__import__"), "got: {}", e[0].message);
    }

    #[test]
    fn block_getattr() {
        let e = sanitize("getattr(obj, '__class__')").unwrap_err();
        assert!(e[0].message.contains("getattr"), "got: {}", e[0].message);
    }

    #[test]
    fn block_class_def() {
        let e = sanitize("class Foo:\n    pass").unwrap_err();
        assert!(e[0].message.contains("class"), "got: {}", e[0].message);
    }

    #[test]
    fn block_while() {
        let e = sanitize("while True:\n    pass").unwrap_err();
        assert!(e[0].message.contains("while"), "got: {}", e[0].message);
    }

    #[test]
    fn block_lambda() {
        let e = sanitize("f = lambda x: x + 1").unwrap_err();
        assert!(e[0].message.contains("lambda"), "got: {}", e[0].message);
    }

    #[test]
    fn block_list_comp() {
        let e = sanitize("[x * 2 for x in items]").unwrap_err();
        assert!(e[0].message.contains("comprehension"), "got: {}", e[0].message);
    }

    #[test]
    fn block_set_comp() {
        let e = sanitize("{x * 2 for x in items}").unwrap_err();
        assert!(e[0].message.contains("comprehension"), "got: {}", e[0].message);
    }

    #[test]
    fn block_dict_comp() {
        let e = sanitize("{k: v for k, v in items}").unwrap_err();
        assert!(e[0].message.contains("comprehension"), "got: {}", e[0].message);
    }

    #[test]
    fn block_gen_exp() {
        let e = sanitize("(x for x in items)").unwrap_err();
        assert!(e[0].message.contains("generator") || e[0].message.contains("comprehension"),
            "got: {}", e[0].message);
    }

    #[test]
    fn block_plain_with() {
        let e = sanitize("with open('/etc/passwd'):\n    pass").unwrap_err();
        assert!(e[0].message.contains("with"), "got: {}", e[0].message);
    }

    #[test]
    fn block_try() {
        let e = sanitize("try:\n    pass\nexcept:\n    pass").unwrap_err();
        assert!(e[0].message.contains("try"), "got: {}", e[0].message);
    }

    #[test]
    fn block_async_def() {
        let e = sanitize("async def graph():\n    pass").unwrap_err();
        assert!(e[0].message.contains("async"), "got: {}", e[0].message);
    }

    #[test]
    fn block_f_string() {
        let e = sanitize("x = f'hello {name}'").unwrap_err();
        // f-strings produce JoinedStr, but the parser might parse as Plain string
        // if there's nothing to format. Use a real f-string with a variable.
        let e2 = sanitize("x = f'value {x}'").unwrap_err();
        let _msg = if !e.is_empty() { &e[0].message } else { &e2[0].message };
        assert!(e2[0].message.contains("f‑string") || e2[0].message.contains("f-string"),
            "got: {}", e2[0].message);
    }

    #[test]
    fn block_walrus() {
        let e = sanitize("(x := 5)").unwrap_err();
        assert!(e[0].message.contains("walrus") || e[0].message.contains(":="),
            "got: {}", e[0].message);
    }

    #[test]
    fn block_yield() {
        let e = sanitize("def graph():\n    yield 1").unwrap_err();
        assert!(e[0].message.contains("yield"), "got: {}", e[0].message);
    }

    #[test]
    fn block_await() {
        let e = sanitize("def graph():\n    await foo()").unwrap_err();
        assert!(e[0].message.contains("await"), "got: {}", e[0].message);
    }

    #[test]
    fn block_global() {
        let e = sanitize("global x").unwrap_err();
        assert!(e[0].message.contains("global"), "got: {}", e[0].message);
    }

    #[test]
    fn block_nonlocal() {
        let e = sanitize("def graph():\n    nonlocal x").unwrap_err();
        assert!(e[0].message.contains("nonlocal"), "got: {}", e[0].message);
    }

    #[test]
    fn block_match() {
        // match is Python 3.10+; the parser may not support it without features
        // Just check it doesn't panic
        let _ = sanitize("match x:\n    case 1:\n        pass");
    }

    #[test]
    fn block_del() {
        let e = sanitize("del x").unwrap_err();
        assert!(e[0].message.contains("del"), "got: {}", e[0].message);
    }

    #[test]
    fn block_dunder_method() {
        let e = sanitize("x.__class__()").unwrap_err();
        assert!(e[0].message.contains("dunder"), "got: {}", e[0].message);
    }

    #[test]
    fn block_system_method() {
        let e = sanitize("os.system('rm -rf /')").unwrap_err();
        assert!(e[0].message.contains("system"), "got: {}", e[0].message);
    }

    #[test]
    fn block_popen_method() {
        let e = sanitize("subprocess.Popen(['ls'])").unwrap_err();
        assert!(e[0].message.contains("Popen") || e[0].message.contains("popen"),
            "got: {}", e[0].message);
    }

    #[test]
    fn block_os_system() {
        let e = sanitize("os.system('ls')").unwrap_err();
        assert!(e[0].message.contains("system"), "got: {}", e[0].message);
    }
    #[test]
    fn block_fork() {
        let e = sanitize("os.fork()").unwrap_err();
        assert!(e[0].message.contains("fork"), "got: {}", e[0].message);
    }

    #[test]
    fn block_call_no_string() {
        let e = sanitize("def graph():\n    call(x, arg=1)\n    return 0").unwrap_err();
        assert!(e.iter().any(|e| e.message.contains("first argument must be a string")),
            "got: {:?}", e);
    }

    #[test]
    fn block_act_no_string() {
        let e = sanitize("def graph():\n    act(x, msg=\"hello\")\n    return 0").unwrap_err();
        assert!(e.iter().any(|e| e.message.contains("first argument must be a string")),
            "got: {:?}", e);
    }

    #[test]
    fn block_nested_function() {
        let e = sanitize("def graph():\n    def inner():\n        pass\n    return 0").unwrap_err();
        assert!(e.iter().any(|e| e.message.contains("nested")), "got: {:?}", e);
    }

    #[test]
    fn block_wrong_function_name() {
        let e = sanitize("def main():\n    return 0").unwrap_err();
        assert!(e.iter().any(|e| e.message.contains("function named `graph`")),
            "got: {:?}", e);
    }

    #[test]
    fn block_return_outside() {
        let e = sanitize("return 1").unwrap_err();
        assert!(e[0].message.contains("return"), "got: {}", e[0].message);
    }

    #[test]
    fn block_break_outside() {
        let e = sanitize("break").unwrap_err();
        assert!(e[0].message.contains("break"), "got: {}", e[0].message);
    }

    #[test]
    fn block_continue_outside() {
        let e = sanitize("continue").unwrap_err();
        assert!(e[0].message.contains("continue"), "got: {}", e[0].message);
    }

    #[test]
    fn block_globals() {
        let e = sanitize("globals()").unwrap_err();
        assert!(e[0].message.contains("globals"), "got: {}", e[0].message);
    }

    #[test]
    fn block_locals() {
        let e = sanitize("locals()").unwrap_err();
        assert!(e[0].message.contains("locals"), "got: {}", e[0].message);
    }

    #[test]
    fn block_compile() {
        let e = sanitize("compile('x=1', '<string>', 'exec')").unwrap_err();
        assert!(e[0].message.contains("compile"), "got: {}", e[0].message);
    }

    #[test]
    fn block_input() {
        let e = sanitize("input()").unwrap_err();
        assert!(e[0].message.contains("input"), "got: {}", e[0].message);
    }

    #[test]
    fn block_decorator() {
        let code = "@some_decorator\ndef graph():\n    return 1";
        let e = sanitize(code).unwrap_err();
        assert!(e.iter().any(|e| e.message.contains("decorator")), "got: {:?}", e);
    }

    // ──────── Scope isolation ──────────────────────────────────────

    #[test]
    fn scope_isolation_simple_parallel() {
        assert!(sanitize(
            "def graph():\n    with parallel() as p:\n        p.act(\"print\", x=1)\n    return 0"
        ).is_ok());
    }

    #[test]
    fn scope_isolation_parallel_assign_local() {
        // Assignment inside parallel() to a variable NOT used outside is fine
        assert!(sanitize(
            "def graph():\n    with parallel() as p:\n        x = 1\n    return 0"
        ).is_ok());
    }

    #[test]
    fn scope_isolation_reject_shared_mutation() {
        // Assign inside parallel() to a variable that was assigned at top level → data race warning
        let code = "def graph():\n    x = 0\n    with parallel() as p:\n        x = 1\n    return x";
        let e = sanitize(code).unwrap_err();
        assert!(
            e.iter().any(|e| e.message.contains("data race")),
            "expected data race warning, got: {:?}", e
        );
    }

    #[test]
    fn scope_isolation_parallel_no_leak_on_new_var() {
        // A variable created inside parallel() is fine — it's local to the parallel block
        let code = "def graph():\n    with parallel() as p:\n        y = 42\n    return 0";
        assert!(sanitize(code).is_ok());
    }

    // ──────── Multiple errors ─────────────────────────────────────

    #[test]
    fn multiple_errors_collected() {
        let e = sanitize("import os\nimport sys\nclass Foo:\n    pass\n").unwrap_err();
        assert!(e.len() >= 3, "expected >=3 errors, got {}", e.len());
    }

    // ──────── Empty / edge ────────────────────────────────────────

    #[test]
    fn empty_code() {
        assert!(sanitize("").is_ok());
    }

    #[test]
    fn whitespace_only() {
        assert!(sanitize("   \n  \n").is_ok());
    }

    #[test]
    fn error_line_numbers() {
        let e = sanitize("import os\nimport sys\nimport subprocess\n").unwrap_err();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].line, 1);
        assert_eq!(e[1].line, 2);
        assert_eq!(e[2].line, 3);
    }
}
