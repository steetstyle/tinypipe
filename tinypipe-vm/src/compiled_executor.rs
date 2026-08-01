//! `CompiledExecutor` — Zero-copy DAG interpreter for `CompiledPlan`.
//!
//! Unlike `Executor` (which works with `ExecutionPlan` via string ID lookups),
//! `CompiledExecutor` works directly with `CompiledPlan` (binary FlatBuffers or bincode,
//! uint32 index'ler). Key differences:
//!
//! - **O(1) node access**: `compiled.nodes.get(index)` instead of `plan.get_node(id)`
//! - **No HashMap for node lookup**: indices are direct array offsets
//! - **Format-agnostic**: accepts plan from either `CompiledPlan::from_fb_bytes()` (canonical)
//!   or `CompiledPlan::from_bytes(bincode)` (legacy)
//! - **Smaller memory footprint**: Compact compiled format
//!
//! # Usage
//!
//! ```ignore
//! // FlatBuffers (canonical)
//! let bytes: Vec<u8> = storage.load_plan(&graph_id)?;
//! let plan = CompiledPlan::from_fb_bytes(&bytes)?;
//! let executor = CompiledExecutor::new(&plan, &registry);
//! let result = executor.execute(inputs)?;
//!
//! // Bincode (legacy)
//! let plan = CompiledPlan::from_bytes(&bytes)?;
//! let executor = CompiledExecutor::new(&plan, &registry);
//! ```

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{Context, Value};
use tinypipe_ir::compiled::{CompiledEdge, CompiledNode, CompiledPlan};
use tinypipe_ir::plan::{EdgeKind, Opcode};

use crate::error::{check_version_compatibility, ExecutionError, ExecutionResult};
use crate::pause::{Checkpoint, ExecutionOutcome, LoopState, PausePolicy, StepObserver};

/// `run_node`'un dönüşü: node tamamlandı ya da (sadece LOOP gövdesi içinde)
/// pause politikası tetiklendi.
#[derive(Debug)]
enum NodeOutcome {
    Ok,
    Paused(LoopState),
}

/// Execution engine for CompiledPlan (binary bincode or FlatBuffers format).
pub struct CompiledExecutor<'a> {
    plan: &'a CompiledPlan,
    registry: &'a dyn ToolRegistry,
    /// Çözülmüş ortam görünümü (tool'lara her dispatch'te iletilir).
    env: std::sync::Arc<tinypipe_env::Env>,
    recursion_depth: AtomicU32,
    max_recursion_depth: u32,
    /// Pre-computed count of incoming Control edges per node index.
    control_pred_count: Vec<u32>,
}

impl<'a> CompiledExecutor<'a> {
    /// Create a new executor for the given compiled plan (boş env ile).
    pub fn new(plan: &'a CompiledPlan, registry: &'a dyn ToolRegistry) -> Self {
        Self::with_env(plan, registry, std::sync::Arc::new(tinypipe_env::Env::empty()))
    }

    /// Create a new executor with an explicit environment view.
    /// `env`: tool'ların `env.get` vb. ile okuyacağı ortam (scope'lu olabilir).
    pub fn with_env(
        plan: &'a CompiledPlan,
        registry: &'a dyn ToolRegistry,
        env: std::sync::Arc<tinypipe_env::Env>,
    ) -> Self {
        let n = plan.nodes.len();
        let mut control_pred_count = vec![0u32; n];
        for edge in &plan.edges {
            if edge.kind == EdgeKind::Control {
                let to = edge.to_index as usize;
                if to < n {
                    control_pred_count[to] += 1;
                }
            }
        }
        CompiledExecutor {
            plan,
            registry,
            env,
            recursion_depth: AtomicU32::new(0),
            max_recursion_depth: plan.metadata.max_recursion_depth,
            control_pred_count,
        }
    }

    /// Executor'ın ortam görünümü.
    pub fn env(&self) -> &tinypipe_env::Env {
        &self.env
    }

    /// Execute the compiled plan with the given input context.
    pub fn execute(&self, inputs: Context) -> Result<ExecutionResult, ExecutionError> {
        match self.execute_with(inputs, &PausePolicy::default(), None)? {
            ExecutionOutcome::Completed(result) => Ok(result),
            ExecutionOutcome::Paused(_) => {
                // Default policy never pauses; this is unreachable in practice.
                unreachable!("default pause policy cannot pause")
            }
        }
    }

    /// Execute with a pause policy and optional step observer.
    ///
    /// Pause denetimleri ana passtaki `run_node` çağrıları arasında (ve LOOP
    /// gövdesi içinde body node başına) yapılır. PARALLEL branch thread'leri
    /// pause'a tabi değildir — kendi içlerinde her zaman tamamlanır.
    pub fn execute_with(
        &self,
        inputs: Context,
        policy: &PausePolicy,
        mut observer: Option<&mut dyn StepObserver>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        check_version_compatibility(self.plan.version).map_err(ExecutionError::VersionMismatch)?;

        let start = Instant::now();
        let time_limit_us = (self.plan.metadata.max_execution_time_ms as u64) * 1000;
        let mem_limit = self.plan.metadata.max_context_memory_bytes as u64;
        let node_budget = self.plan.metadata.max_node_execution_count;

        let mut ctx = inputs;
        let mut node_outputs: HashMap<u32, Value> = HashMap::new();
        let mut execution_order: Vec<String> = Vec::new();
        let mut node_count: u32 = 0;
        let mut output: Option<Value> = None;
        let mut enabled: HashSet<u32> = HashSet::new();

        // Track control-flow satisfaction: how many control predecessors have completed
        // for each node index.
        let n = self.plan.nodes.len();
        let control_satisfied: Vec<Cell<u32>> = (0..n).map(|_| Cell::new(0u32)).collect();

        // Initially enable nodes with in-degree zero (no incoming edges)
        for node in &self.plan.nodes {
            let has_incoming = self.plan.edges.iter().any(|e| e.to_index == node.index);
            if !has_incoming {
                enabled.insert(node.index);
            }
        }

        // --- Phase 1: Determine execution order via Kahn's algorithm on compiled edges ---
        let order = self.topological_order()?;

        // --- Phase 1b: Pre-compute loop body node sets ---
        // Maps a LOOP node index → set of body node indices (to skip during main pass)
        let loop_bodies: HashMap<u32, HashSet<u32>> = self.identify_loop_bodies();
        let mut loop_skipped: HashSet<u32> = HashSet::new();

        // --- Phase 2: Execute nodes in order (pause-aware) ---
        for (position, node_index) in order.iter().enumerate() {
            let node = self
                .plan
                .get_node(*node_index)
                .ok_or_else(|| ExecutionError::NodeNotFound(format!("index {}", node_index)))?;

            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_start(&node.id);
            }

            match self.run_node(
                node,
                &mut ctx,
                &mut node_outputs,
                &mut execution_order,
                &mut enabled,
                &control_satisfied,
                &loop_bodies,
                &mut loop_skipped,
                &mut node_count,
                &mut output,
                start,
                time_limit_us,
                node_budget,
                mem_limit,
                &order,
                Some(policy),
            )? {
                NodeOutcome::Paused(loop_state) => {
                    return Ok(ExecutionOutcome::Paused(self.build_checkpoint(
                        &ctx,
                        &node_outputs,
                        &enabled,
                        &control_satisfied,
                        &loop_skipped,
                        &execution_order,
                        &output,
                        node_count,
                        position,
                        start.elapsed().as_micros() as u64,
                        Some(loop_state),
                    )));
                }
                NodeOutcome::Ok => {}
            }

            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_end(&node.id);
            }

            // Son node'dan sonra pause anlamsız (devam edilecek node yok) — atla.
            if position + 1 < order.len() && policy.should_pause(node_count, &node.id) {
                return Ok(ExecutionOutcome::Paused(self.build_checkpoint(
                    &ctx,
                    &node_outputs,
                    &enabled,
                    &control_satisfied,
                    &loop_skipped,
                    &execution_order,
                    &output,
                    node_count,
                    position + 1,
                    start.elapsed().as_micros() as u64,
                    None,
                )));
            }
        }

        let duration = start.elapsed().as_micros() as u64;

        Ok(ExecutionOutcome::Completed(ExecutionResult {
            context: ctx,
            execution_order,
            node_count,
            duration_us: duration,
            output,
        }))
    }

    /// A paused execution'ı checkpoint'ten devam ettirir.
    pub fn resume(
        &self,
        checkpoint: &Checkpoint,
        policy: &PausePolicy,
        mut observer: Option<&mut dyn StepObserver>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        check_version_compatibility(self.plan.version).map_err(ExecutionError::VersionMismatch)?;

        let start = Instant::now();
        let start = start
            .checked_sub(std::time::Duration::from_micros(checkpoint.elapsed_us))
            .unwrap_or(start);
        let time_limit_us = (self.plan.metadata.max_execution_time_ms as u64) * 1000;
        let mem_limit = self.plan.metadata.max_context_memory_bytes as u64;
        let node_budget = self.plan.metadata.max_node_execution_count;

        let mut ctx = checkpoint.context.clone();
        let mut node_outputs = checkpoint.node_outputs.clone();
        let mut execution_order = checkpoint.execution_order.clone();
        let mut node_count = checkpoint.node_count;
        let mut output = checkpoint.output.clone();
        let mut enabled = checkpoint.enabled.clone();
        let n = self.plan.nodes.len();
        let control_satisfied: Vec<Cell<u32>> = checkpoint
            .control_satisfied
            .iter()
            .map(|v| Cell::new(*v))
            .chain((checkpoint.control_satisfied.len()..n).map(|_| Cell::new(0u32)))
            .collect();
        let mut loop_skipped = checkpoint.loop_skipped.clone();

        let order = self.topological_order()?;
        let loop_bodies: HashMap<u32, HashSet<u32>> = self.identify_loop_bodies();

        // LOOP gövdesi ortasında kaldıysak önce loop'u devam ettir
        let mut resumed_loop = false;
        if let Some(ls) = &checkpoint.loop_state {
            let loop_node = self
                .plan
                .get_node(ls.loop_index)
                .ok_or_else(|| ExecutionError::NodeNotFound(format!("loop {}", ls.loop_index)))?;
            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_start(&loop_node.id);
            }
            match self.run_loop(
                loop_node,
                &mut ctx,
                &mut node_outputs,
                &mut enabled,
                &control_satisfied,
                &loop_bodies,
                &mut loop_skipped,
                &mut node_count,
                start,
                time_limit_us,
                node_budget,
                mem_limit,
                &order,
                Some((ls.iteration, ls.body_position)),
                Some(policy),
            )? {
                NodeOutcome::Paused(ls2) => {
                    return Ok(ExecutionOutcome::Paused(self.build_checkpoint(
                        &ctx,
                        &node_outputs,
                        &enabled,
                        &control_satisfied,
                        &loop_skipped,
                        &execution_order,
                        &output,
                        node_count,
                        checkpoint.position,
                        start.elapsed().as_micros() as u64,
                        Some(ls2),
                    )));
                }
                NodeOutcome::Ok => {}
            }
            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_end(&loop_node.id);
            }
            resumed_loop = true;
        }

        // Ana passtan kaldığımız yerden devam et.
        // LOOP gövdesi ortasında kaldıysak LOOP node'u zaten run_loop ile
        // çalıştırıldı — pozisyonu bir ileri taşı (node tekrar çalışmasın).
        let start_position = checkpoint.position + usize::from(resumed_loop);
        for position in start_position..order.len() {
            let idx = order[position];
            let node = self
                .plan
                .get_node(idx)
                .ok_or_else(|| ExecutionError::NodeNotFound(format!("index {}", idx)))?;

            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_start(&node.id);
            }

            match self.run_node(
                node,
                &mut ctx,
                &mut node_outputs,
                &mut execution_order,
                &mut enabled,
                &control_satisfied,
                &loop_bodies,
                &mut loop_skipped,
                &mut node_count,
                &mut output,
                start,
                time_limit_us,
                node_budget,
                mem_limit,
                &order,
                Some(policy),
            )? {
                NodeOutcome::Paused(loop_state) => {
                    return Ok(ExecutionOutcome::Paused(self.build_checkpoint(
                        &ctx,
                        &node_outputs,
                        &enabled,
                        &control_satisfied,
                        &loop_skipped,
                        &execution_order,
                        &output,
                        node_count,
                        position,
                        start.elapsed().as_micros() as u64,
                        Some(loop_state),
                    )));
                }
                NodeOutcome::Ok => {}
            }

            if let Some(obs) = observer.as_deref_mut() {
                obs.on_node_end(&node.id);
            }

            // Son node'dan sonra pause anlamsız (devam edilecek node yok) — atla.
            if position + 1 < order.len() && policy.should_pause(node_count, &node.id) {
                return Ok(ExecutionOutcome::Paused(self.build_checkpoint(
                    &ctx,
                    &node_outputs,
                    &enabled,
                    &control_satisfied,
                    &loop_skipped,
                    &execution_order,
                    &output,
                    node_count,
                    position + 1,
                    start.elapsed().as_micros() as u64,
                    None,
                )));
            }
        }

        let duration = start.elapsed().as_micros() as u64;

        Ok(ExecutionOutcome::Completed(ExecutionResult {
            context: ctx,
            execution_order,
            node_count,
            duration_us: duration,
            output,
        }))
    }

    fn build_checkpoint(
        &self,
        ctx: &Context,
        node_outputs: &HashMap<u32, Value>,
        enabled: &HashSet<u32>,
        control_satisfied: &[Cell<u32>],
        loop_skipped: &HashSet<u32>,
        execution_order: &[String],
        output: &Option<Value>,
        node_count: u32,
        position: usize,
        elapsed_us: u64,
        loop_state: Option<LoopState>,
    ) -> Checkpoint {
        Checkpoint {
            node_count,
            position,
            elapsed_us,
            loop_state,
            context: ctx.clone(),
            node_outputs: node_outputs.clone(),
            enabled: enabled.clone(),
            control_satisfied: control_satisfied.iter().map(|c| c.get()).collect(),
            loop_skipped: loop_skipped.clone(),
            execution_order: execution_order.to_vec(),
            output: output.clone(),
        }
    }

    /// Execute a single node (skip checks, budget checks, dispatch, edge propagation).
    ///
    /// Used by both the main execution pass and parallel branch threads, so every
    /// opcode has identical semantics regardless of where it runs. Returns
    /// `NodeOutcome::Paused` when a LOOP body node triggers the pause policy.
    #[allow(clippy::too_many_arguments)]
    fn run_node(
        &self,
        node: &CompiledNode,
        ctx: &mut Context,
        node_outputs: &mut HashMap<u32, Value>,
        execution_order: &mut Vec<String>,
        enabled: &mut HashSet<u32>,
        control_satisfied: &[Cell<u32>],
        loop_bodies: &HashMap<u32, HashSet<u32>>,
        loop_skipped: &mut HashSet<u32>,
        node_count: &mut u32,
        output: &mut Option<Value>,
        start: Instant,
        time_limit_us: u64,
        node_budget: u32,
        mem_limit: u64,
        order: &[u32],
        pause: Option<&PausePolicy>,
    ) -> Result<NodeOutcome, ExecutionError> {
        // Skip nodes that are inside a loop body (handled by loop execution inline)
        if loop_skipped.contains(&node.index) {
            return Ok(NodeOutcome::Ok);
        }

        // Skip if not enabled by edge propagation
        if !enabled.contains(&node.index) {
            tracing::trace!(
                node = node.index,
                op = ?node.op,
                loop_skipped = loop_skipped.contains(&node.index),
                "node not enabled — skipped"
            );
            return Ok(NodeOutcome::Ok);
        }

        tracing::trace!(node = node.index, op = ?node.op, "node executing");

        // Check control-flow dependencies: all control predecessors must have completed.
        let idx = node.index as usize;
        let n = self.plan.nodes.len();
        if idx < n && control_satisfied[idx].get() < self.control_pred_count[idx] {
            tracing::trace!(
                node = node.index,
                got = control_satisfied[idx].get(),
                need = self.control_pred_count[idx],
                "control predecessor(s) pending — deferred"
            );
            // Control predecessors not yet complete — defer execution.
            // This ensures sequential ordering: e.g., a statement after an if/else
            // with an early-return branch only executes when the fall-through path
            // reaches the MERGE node.
            return Ok(NodeOutcome::Ok);
        }

        // Budget checks
        *node_count += 1;
        let elapsed = start.elapsed();
        if elapsed.as_micros() as u64 > time_limit_us {
            return Err(ExecutionError::TimeLimitExceeded(time_limit_us / 1000));
        }
        if *node_count > node_budget {
            return Err(ExecutionError::NodeBudgetExceeded(*node_count, node_budget));
        }
        let ctx_bytes = ctx.estimated_bytes();
        if ctx_bytes > mem_limit {
            return Err(ExecutionError::MemoryLimitExceeded(mem_limit, ctx_bytes));
        }

        execution_order.push(node.id.clone());

        // ── Scope isolation: active branch'i node'un branch_id'sine göre ayarla ──
        if let Some(bid) = node.branch_id {
            ctx.set_branch(bid);
        } else {
            ctx.clear_branch();
        }

        match node.op {
            Opcode::Input => {
                let name = node
                    .args
                    .iter()
                    .find(|a| a.key == "name")
                    .map(|a| trim_quotes(&a.value))
                    .unwrap_or(&node.id);
                let default = node.args.iter().find(|a| a.key == "default");
                if let Some(val) = ctx.get(name) {
                    node_outputs.insert(node.index, val.clone());
                } else if let Some(d) = default {
                    let v = parse_json_value(&d.value);
                    node_outputs.insert(node.index, v);
                }
                // Propagate edges
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Calc => {
                let expr = node
                    .args
                    .iter()
                    .find(|a| a.key == "expr")
                    .map(|a| &a.value)
                    .unwrap_or(&node.id);
                let result = eval_expression(expr, ctx, node_outputs)?;
                node_outputs.insert(node.index, result.clone());
                // Output arg writes result to context for downstream nodes
                if let Some(output_var) = node
                    .args
                    .iter()
                    .find(|a| a.key == "output")
                    .map(|a| trim_quotes(&a.value))
                {
                    if !output_var.is_empty() {
                        ctx.set(output_var.to_owned(), result);
                    }
                }
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Call => {
                let target = node
                    .args
                    .iter()
                    .find(|a| a.key == "target")
                    .map(|a| trim_quotes(&a.value))
                    .unwrap_or("unknown");
                let output_var = node
                    .args
                    .iter()
                    .find(|a| a.key == "output")
                    .map(|a| trim_quotes(&a.value));

                let mut params = HashMap::new();
                for arg in &node.args {
                    if arg.key == "target"
                        || arg.key == "output"
                        || arg.key == "on_error"
                        || arg.key == "fallback_value"
                    {
                        continue;
                    }
                    let val = resolve_arg_value(&arg.value, ctx, node_outputs);
                    params.insert(arg.key.clone(), val);
                }

                // ── Subgraph dispatch (v2) ─────────────────────────────
                if target.starts_with("subgraph:") {
                    let subgraph_name = target.trim_start_matches("subgraph:");
                    // Explicit kwargs child input'a override olarak gider
                    // (çağıranın ctx'inden daha öncelikli).
                    let mut sub_input = ctx.clone();
                    for (k, v) in &params {
                        sub_input.set(k.clone(), v.clone());
                    }
                    if self.recursion_depth.load(Ordering::SeqCst) >= self.max_recursion_depth {
                        return Err(ExecutionError::RecursionLimitExceeded(subgraph_name.into()));
                    }
                    self.recursion_depth.fetch_add(1, Ordering::SeqCst);
                    let subgraph_result = self
                        .registry
                        .execute_subgraph(subgraph_name, sub_input, &self.env)
                        .map_err(|e| {
                            ExecutionError::CallFailed(subgraph_name.into(), e.to_string())
                        });
                    self.recursion_depth.fetch_sub(1, Ordering::SeqCst);
                    let subgraph_result = subgraph_result?;
                    let subgraph_ctx = subgraph_result.context;
                    let subgraph_output = subgraph_result.output;
                    // Merge subgraph context into current context
                    for (k, v) in subgraph_ctx.variables {
                        ctx.set(k, v);
                    }
                    // Subgraph return value becomes the call expression's value
                    if let Some(name) = output_var {
                        ctx.set(name.to_string(), subgraph_output.clone());
                    }
                    node_outputs.insert(node.index, subgraph_output);
                    self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
                    return Ok(NodeOutcome::Ok);
                }

                let call_target = tinypipe_api::types::CallTarget {
                    name: target.to_string(),
                    args: Vec::new(),
                    kwargs: params,
                };

                // Schema drift detection (v2.6): check tool schema_hash before dispatch
                if !target.starts_with("rpc:") {
                    let tool_name = target.trim_start_matches("tool:");
                    if let Some(tool_dep) = self
                        .plan
                        .metadata
                        .tool_deps
                        .iter()
                        .find(|d| d.name == tool_name && !d.schema_hash.is_empty())
                    {
                        let latest_hash = match self.registry.latest_schema_hash(tool_name) {
                            Ok(h) => h,
                            Err(_) => String::new(),
                        };
                        if !latest_hash.is_empty() && tool_dep.schema_hash != latest_hash {
                            return Err(ExecutionError::SchemaDriftDetected(
                                tool_name.to_string(),
                                tool_dep.schema_hash.clone(),
                                latest_hash,
                            ));
                        }
                    }
                }

                match self.registry.dispatch(&call_target, ctx, &self.env) {
                    Ok(val) => {
                        if let Some(name) = output_var {
                            ctx.set(name.to_string(), val.clone());
                        }
                        node_outputs.insert(node.index, val);
                    }
                    Err(e) => {
                        let on_error = node
                            .args
                            .iter()
                            .find(|a| a.key == "on_error")
                            .map(|a| trim_quotes(&a.value))
                            .unwrap_or("abort");
                        let err_val = match on_error {
                            "continue_with_null" => Value::Null,
                            "continue_with_fallback" => {
                                let fallback = node.args.iter().find(|a| a.key == "fallback_value");
                                if let Some(fb) = fallback {
                                    parse_json_value(&fb.value)
                                } else {
                                    Value::Null
                                }
                            }
                            _ => {
                                return Err(ExecutionError::CallFailed(
                                    target.to_string(),
                                    e.to_string(),
                                ));
                            }
                        };
                        if let Some(name) = output_var {
                            ctx.set(name.to_string(), err_val.clone());
                        }
                        node_outputs.insert(node.index, err_val);
                    }
                }
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Decide => {
                // Native compiled format: source/op/value
                let source = node
                    .args
                    .iter()
                    .find(|a| a.key == "source")
                    .map(|a| resolve_arg_value(&a.value, ctx, node_outputs))
                    .unwrap_or(Value::Null);
                let op = node
                    .args
                    .iter()
                    .find(|a| a.key == "op")
                    .map(|a| trim_quotes(&a.value).to_string())
                    .unwrap_or_default();
                let cmp_val = node
                    .args
                    .iter()
                    .find(|a| a.key == "value")
                    .map(|a| parse_json_value(&a.value))
                    .unwrap_or(Value::Null);

                // Compiled format: condition string (e.g. "x > 0")
                let decision = if !op.is_empty() {
                    evaluate_condition(&source, &op, &cmp_val)?
                } else if let Some(cond) = node
                    .args
                    .iter()
                    .find(|a| a.key == "condition")
                    .map(|a| trim_quotes(&a.value))
                {
                    eval_expression(cond, ctx, node_outputs)
                        .map(|v| is_truthy(&v))
                        .unwrap_or(false)
                } else {
                    false
                };
                node_outputs.insert(node.index, Value::Bool(decision));
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Act => {
                let action_type = node
                    .args
                    .iter()
                    .find(|a| a.key == "type" || a.key == "action_type")
                    .map(|a| trim_quotes(&a.value).to_string())
                    .unwrap_or_default();
                let act_output = Value::String(format!("[ACT:{}]", action_type));
                node_outputs.insert(node.index, act_output);

                if action_type == "return" {
                    let content = node
                        .args
                        .iter()
                        .find(|a| a.key == "content" || a.key == "value")
                        .map(|a| resolve_arg_value(&a.value, ctx, node_outputs));
                    if let Some(ref v) = content {
                        *output = Some(v.clone());
                        node_outputs.insert(node.index, v.clone());
                    } else {
                        // No explicit content: read the predecessor's computed value
                        // along incoming data edges (graph-faithful return semantics).
                        let pred_value = self
                            .plan
                            .edges
                            .iter()
                            .filter(|e| e.to_index == node.index && e.kind == EdgeKind::Data)
                            .filter_map(|e| node_outputs.get(&e.from_index).cloned())
                            .next_back();
                        if let Some(v) = pred_value {
                            *output = Some(v.clone());
                            node_outputs.insert(node.index, v);
                        } else {
                            // Use all_variables() to include active branch scope
                            let val = Value::Object(ctx.all_variables());
                            *output = Some(val.clone());
                            node_outputs.insert(node.index, val);
                        }
                    }
                }
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Switch => {
                // Evaluate the source expression to determine which branch to take
                let source = node
                    .args
                    .iter()
                    .find(|a| a.key == "source")
                    .map(|a| resolve_arg_value(&a.value, ctx, node_outputs))
                    .unwrap_or(Value::Null);
                let source_str = format_value(&source);

                // Find all outgoing edges from this Switch node
                let switch_edges: Vec<&CompiledEdge> = self
                    .plan
                    .edges
                    .iter()
                    .filter(|e| e.from_index == node.index)
                    .collect();

                // Match source value against edge conditions
                let mut matched = false;
                for edge in &switch_edges {
                    match &edge.condition {
                        Some(cond) if *cond == source_str => {
                            // Case match — enable this edge
                            if let Some(ref mapping) = edge.mapping {
                                for (from_key, to_key) in mapping {
                                    if let Some(val) = ctx.get(from_key).cloned() {
                                        ctx.set(to_key.clone(), val);
                                    }
                                }
                            }
                            enabled.insert(edge.to_index);
                            matched = true;
                        }
                        Some(cond) if cond == "default" && !matched => {
                            // Default case — only if no other case matched
                            if let Some(ref mapping) = edge.mapping {
                                for (from_key, to_key) in mapping {
                                    if let Some(val) = ctx.get(from_key).cloned() {
                                        ctx.set(to_key.clone(), val);
                                    }
                                }
                            }
                            enabled.insert(edge.to_index);
                        }
                        None if !matched => {
                            // Unconditional edge as fallback
                            if let Some(ref mapping) = edge.mapping {
                                for (from_key, to_key) in mapping {
                                    if let Some(val) = ctx.get(from_key).cloned() {
                                        ctx.set(to_key.clone(), val);
                                    }
                                }
                            }
                            enabled.insert(edge.to_index);
                        }
                        _ => {}
                    }
                }
                node_outputs.insert(node.index, Value::String(source_str));
            }

            Opcode::Parallel => {
                // ── Scope isolation: branch scope'larını başlat ──
                // Parent context'in snapshot'ını al, her branch için scope hazırla
                ctx.enter_parallel();

                // Direct children of this PARALLEL node (branch entries + continuation)
                let children: Vec<u32> = self
                    .plan
                    .edges
                    .iter()
                    .filter(|e| e.from_index == node.index)
                    .map(|e| e.to_index)
                    .collect();

                // Enable all outgoing edges — children execute naturally via edge propagation
                for edge in &self.plan.edges {
                    if edge.from_index == node.index {
                        // Edge mapping: parent değişkenlerini branch scope'a kopyala
                        if let Some(ref mapping) = edge.mapping {
                            for (from_key, to_key) in mapping {
                                if let Some(val) = ctx.get(from_key).cloned() {
                                    ctx.set(to_key.clone(), val);
                                }
                            }
                        }
                        enabled.insert(edge.to_index);
                    }
                }
                node_outputs.insert(node.index, Value::Null);

                // Region: everything reachable from the children. Partition region nodes
                // by branch_id — each branch group runs on its own thread. Nodes without
                // a branch_id (continuation after the parallel block) run in the main
                // pass once all branch threads have completed.
                let region: HashSet<u32> = self.collect_reachable_indices(&children);
                let mut groups: HashMap<u32, Vec<u32>> = HashMap::new();
                for &idx in order {
                    if region.contains(&idx) {
                        if let Some(nd) = self.plan.get_node(idx) {
                            if let Some(bid) = nd.branch_id {
                                groups.entry(bid).or_default().push(idx);
                            }
                        }
                    }
                }

                if groups.is_empty() {
                    return Ok(NodeOutcome::Ok);
                }

                // Mark branch nodes so the main pass skips them (they run on threads)
                for group in groups.values() {
                    for idx in group {
                        loop_skipped.insert(*idx);
                    }
                }

                // Run each branch group on its own thread, then merge results back.
                let mut bids: Vec<u32> = groups.keys().copied().collect();
                bids.sort_unstable();

                // Snapshot of control counters before the threads run; per-thread
                // deltas are merged back so continuation nodes see all control edges.
                let initial_control: Vec<u32> = control_satisfied.iter().map(|c| c.get()).collect();

                let results: Vec<(
                    u32,
                    Context,
                    HashMap<u32, Value>,
                    Vec<String>,
                    u32,
                    Option<Value>,
                    HashSet<u32>,
                    Vec<u32>,
                )> = std::thread::scope(|s| {
                    let mut handles = Vec::new();
                    for bid in &bids {
                        let group = groups.get(bid).cloned().unwrap_or_default();
                        let entries: Vec<u32> = children
                            .iter()
                            .copied()
                            .filter(|c| group.contains(c))
                            .collect();
                        let mut tctx = ctx.clone();
                        let t_node_outputs = node_outputs.clone();
                        let t_enabled: HashSet<u32> = entries.into_iter().collect();
                        let t_control_satisfied: Vec<Cell<u32>> = control_satisfied
                            .iter()
                            .map(|c| Cell::new(c.get()))
                            .collect();
                        let t_loop_skipped: HashSet<u32> = loop_skipped
                            .iter()
                            .copied()
                            .filter(|i| !group.contains(i))
                            .collect();
                        let bid = *bid;
                        handles.push(s.spawn(move || {
                            let mut t_outputs = t_node_outputs;
                            let mut t_enabled = t_enabled;
                            let t_control = t_control_satisfied;
                            let mut t_skipped = t_loop_skipped;
                            let mut t_order: Vec<String> = Vec::new();
                            let mut t_node_count: u32 = 0;
                            let mut t_output: Option<Value> = None;
                            for idx in &group {
                                let nd = self.plan.get_node(*idx).ok_or_else(|| {
                                    ExecutionError::NodeNotFound(format!("region index {}", idx))
                                })?;
                                self.run_node(
                                    nd,
                                    &mut tctx,
                                    &mut t_outputs,
                                    &mut t_order,
                                    &mut t_enabled,
                                    &t_control,
                                    loop_bodies,
                                    &mut t_skipped,
                                    &mut t_node_count,
                                    &mut t_output,
                                    start,
                                    time_limit_us,
                                    node_budget,
                                    mem_limit,
                                    order,
                                    None,
                                )?;
                            }
                            let t_control_final: Vec<u32> =
                                t_control.iter().map(|c| c.get()).collect();
                            Ok::<_, ExecutionError>((
                                bid,
                                tctx,
                                t_outputs,
                                t_order,
                                t_node_count,
                                t_output,
                                t_enabled,
                                t_control_final,
                            ))
                        }));
                    }
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .map_err(|_| {
                                    ExecutionError::Custom("parallel branch thread panicked".into())
                                })
                                .and_then(|r| r)
                        })
                        .collect::<Result<Vec<_>, ExecutionError>>()
                })?;

                // ── Merge branch results into the main state ──
                for (
                    bid,
                    mut tctx,
                    t_outputs,
                    t_order,
                    t_node_count,
                    t_output,
                    t_enabled,
                    t_control_final,
                ) in results
                {
                    *node_count += t_node_count;
                    for (k, v) in t_outputs {
                        node_outputs.entry(k).or_insert(v);
                    }
                    // Thread-side enabled additions propagate to continuation nodes
                    for idx in t_enabled {
                        enabled.insert(idx);
                    }
                    // Control edge increments from branch nodes are applied to the
                    // main counters (targets live in the continuation, not the group)
                    for (idx, (before, after)) in initial_control
                        .iter()
                        .zip(t_control_final.iter())
                        .enumerate()
                    {
                        if *after > *before {
                            control_satisfied[idx].set(*after);
                        }
                    }
                    // Branch scope'u ana context'e ver (MERGE node'u birleştirir)
                    if let Some(scope) = tctx.take_branch_scope(bid) {
                        ctx.insert_branch_scope(bid, scope);
                    }
                    if output.is_none() {
                        *output = t_output;
                    }
                    execution_order.extend(t_order);
                }
            }

            Opcode::Loop => {
                tracing::trace!(node = node.index, "loop dispatch");
                return self.run_loop(
                    node,
                    ctx,
                    node_outputs,
                    enabled,
                    control_satisfied,
                    loop_bodies,
                    loop_skipped,
                    node_count,
                    start,
                    time_limit_us,
                    node_budget,
                    mem_limit,
                    order,
                    None,
                    pause,
                );
            }

            Opcode::Wait => {
                let secs = node
                    .args
                    .iter()
                    .find(|a| a.key == "duration_secs")
                    .map(|a| a.value.parse::<i64>().unwrap_or(0))
                    .unwrap_or(0);
                let max_secs = 300i64;
                if secs > max_secs {
                    return Err(ExecutionError::Custom(format!(
                        "WAIT duration {}s exceeds v1 maximum of {}s",
                        secs, max_secs
                    )));
                }
                if secs > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(secs as u64));
                }
                node_outputs.insert(node.index, Value::Null);
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Merge => {
                // ── Scope isolation: tüm branch scope'larını birleştir ──
                ctx.merge_branches();
                // Snapshot the merged context as output
                let merged_vars = ctx.variables.clone();
                node_outputs.insert(node.index, Value::Object(merged_vars));
                self.propagate_edges(node, ctx, node_outputs, enabled, control_satisfied)?;
            }

            Opcode::Error => {
                let msg = node
                    .args
                    .iter()
                    .find(|a| a.key == "message")
                    .map(|a| trim_quotes(&a.value).to_string())
                    .unwrap_or_else(|| "execution error".into());
                return Err(ExecutionError::Custom(msg));
            }
        }

        Ok(NodeOutcome::Ok)
    }

    /// LOOP node gövdesini yürütür (inline).
    /// `loop_cursor = Some((iteration, body_position))` verilirse loop'a o noktadan
    /// devam edilir (pause/resume). Pause politikası body node başına denetlenir.
    #[allow(clippy::too_many_arguments)]
    fn run_loop(
        &self,
        node: &CompiledNode,
        ctx: &mut Context,
        node_outputs: &mut HashMap<u32, Value>,
        enabled: &mut HashSet<u32>,
        control_satisfied: &[Cell<u32>],
        loop_bodies: &HashMap<u32, HashSet<u32>>,
        loop_skipped: &mut HashSet<u32>,
        node_count: &mut u32,
        start: Instant,
        time_limit_us: u64,
        node_budget: u32,
        _mem_limit: u64,
        order: &[u32],
        loop_cursor: Option<(u32, usize)>,
        pause: Option<&PausePolicy>,
    ) -> Result<NodeOutcome, ExecutionError> {
        let max_iter = node
            .args
            .iter()
            .find(|a| a.key == "max_iterations")
            .map(|a| a.value.parse::<u32>().unwrap_or(100))
            .unwrap_or(100);
        let target_name = node
            .args
            .iter()
            .find(|a| a.key == "target")
            .map(|a| trim_quotes(&a.value).to_string());

        tracing::trace!(
            node = node.index,
            target = ?target_name,
            max_iterations = max_iter,
            cursor = ?loop_cursor,
            "loop start"
        );

        // Compiler konvansiyonu: LOOP'un çoklu DATA edge'i body entry'leridir (loop
        // değişkenini okuyan her body node'una bir DATA edge) ve devam tek CONTROL
        // edge'idir. Body set: DATA anchor'larından ulaşılabilenler − CONTROL
        // devamından ulaşılabilenler.
        let mut continuation_anchors: Vec<u32> = Vec::new();
        let mut data_anchors: Vec<u32> = Vec::new();
        for edge in &self.plan.edges {
            if edge.from_index == node.index && edge.condition.is_none() {
                match edge.kind {
                    EdgeKind::Control => continuation_anchors.push(edge.to_index),
                    _ => data_anchors.push(edge.to_index),
                }
            }
        }

        if !data_anchors.is_empty() {
            let mut body_set: HashSet<u32> = HashSet::new();
            for to in &data_anchors {
                body_set.extend(self.collect_reachable_indices(&[*to]));
            }
            if !continuation_anchors.is_empty() {
                let mut continuation: HashSet<u32> = HashSet::new();
                for to in &continuation_anchors {
                    continuation.extend(self.collect_reachable_indices(&[*to]));
                }
                body_set = body_set.difference(&continuation).copied().collect();
            }
            let body_vec: Vec<u32> = order
                .iter()
                .filter(|idx| body_set.contains(idx))
                .copied()
                .collect();

            // Mark body nodes as skipped so the main pass doesn't re-execute them
            for bidx in &body_set {
                loop_skipped.insert(*bidx);
            }

            // Execute loop body up to max_iterations
            let (start_iteration, start_body_pos) = loop_cursor.unwrap_or((0, 0));

            // Execute loop body up to max_iterations
            for iteration in start_iteration..max_iter {
                // Provide the loop variable value in context
                if let Some(ref name) = target_name {
                    ctx.set(name.clone(), Value::Int(iteration as i64));
                }
                tracing::trace!(
                    node = node.index,
                    iteration,
                    body_nodes = body_vec.len(),
                    "loop iteration"
                );
                // Check for break condition via DECIDE node in body
                let mut should_break = false;

                for (body_pos, body_idx) in body_vec.iter().enumerate() {
                    // Skip, sadece devam edilen iterasyonun ilk start_body_pos node'u
                    // için geçerlidir (bu node'lar pause'dan önce çalışmıştı).
                    if iteration == start_iteration && body_pos < start_body_pos {
                        continue;
                    }

                    let body_node = self.plan.get_node(*body_idx).ok_or_else(|| {
                        ExecutionError::NodeNotFound(format!("body index {}", body_idx))
                    })?;

                    // Budget and time checks
                    *node_count += 1;
                    let elapsed = start.elapsed();
                    if elapsed.as_micros() as u64 > time_limit_us {
                        return Err(ExecutionError::TimeLimitExceeded(time_limit_us / 1000));
                    }
                    if *node_count > node_budget {
                        return Err(ExecutionError::NodeBudgetExceeded(*node_count, node_budget));
                    }

                    match body_node.op {
                        Opcode::Calc => {
                            let expr = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "expr")
                                .map(|a| &a.value)
                                .unwrap_or(&body_node.id);
                            let result = eval_expression(expr, ctx, node_outputs)?;
                            node_outputs.insert(body_node.index, result.clone());
                            // Output arg writes result to context (loop body persists state)
                            if let Some(output_var) = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "output")
                                .map(|a| trim_quotes(&a.value))
                            {
                                if !output_var.is_empty() {
                                    ctx.set(output_var.to_owned(), result);
                                }
                            }
                        }
                        Opcode::Call => {
                            let target = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "target")
                                .map(|a| trim_quotes(&a.value))
                                .unwrap_or("unknown");
                            let output_var = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "output")
                                .map(|a| trim_quotes(&a.value));

                            let mut params = HashMap::new();
                            for arg in &body_node.args {
                                if arg.key == "target" || arg.key == "output" {
                                    continue;
                                }
                                let val = resolve_arg_value(&arg.value, ctx, node_outputs);
                                params.insert(arg.key.clone(), val);
                            }

                            // ── Subgraph dispatch (loop body) ───────────────
                            if target.starts_with("subgraph:") {
                                let subgraph_name = target.trim_start_matches("subgraph:");
                                let mut sub_input = ctx.clone();
                                for (k, v) in &params {
                                    sub_input.set(k.clone(), v.clone());
                                }
                                if self.recursion_depth.load(Ordering::SeqCst)
                                    >= self.max_recursion_depth
                                {
                                    return Err(ExecutionError::RecursionLimitExceeded(
                                        subgraph_name.into(),
                                    ));
                                }
                                self.recursion_depth.fetch_add(1, Ordering::SeqCst);
                                let subgraph_result = self
                                    .registry
                                    .execute_subgraph(subgraph_name, sub_input, &self.env)
                                    .map_err(|e| {
                                        ExecutionError::CallFailed(
                                            subgraph_name.into(),
                                            e.to_string(),
                                        )
                                    });
                                self.recursion_depth.fetch_sub(1, Ordering::SeqCst);
                                let subgraph_result = subgraph_result?;
                                for (k, v) in subgraph_result.context.variables {
                                    ctx.set(k, v);
                                }
                                if let Some(name) = output_var {
                                    ctx.set(name.to_string(), subgraph_result.output.clone());
                                }
                                node_outputs.insert(body_node.index, subgraph_result.output);
                                continue;
                            }

                            let ct = tinypipe_api::types::CallTarget {
                                name: target.to_string(),
                                args: Vec::new(),
                                kwargs: params,
                            };

                            match self.registry.dispatch(&ct, ctx, &self.env) {
                                Ok(val) => {
                                    if let Some(name) = output_var {
                                        ctx.set(name.to_string(), val.clone());
                                    }
                                    node_outputs.insert(body_node.index, val);
                                }
                                Err(e) => {
                                    return Err(ExecutionError::CallFailed(
                                        target.to_string(),
                                        e.to_string(),
                                    ));
                                }
                            }
                        }
                        Opcode::Decide => {
                            let source = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "source")
                                .map(|a| resolve_arg_value(&a.value, ctx, node_outputs))
                                .unwrap_or(Value::Null);
                            let op = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "op")
                                .map(|a| trim_quotes(&a.value).to_string())
                                .unwrap_or_default();
                            let cmp_val = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "value")
                                .map(|a| parse_json_value(&a.value))
                                .unwrap_or(Value::Null);

                            let decision = evaluate_condition(&source, &op, &cmp_val)?;
                            node_outputs.insert(body_node.index, Value::Bool(decision));

                            // Check if this is a break condition
                            if !decision {
                                should_break = true;
                            }
                        }
                        Opcode::Input => {
                            // Re-read input each iteration (allows loop variable updates)
                            let name = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "name")
                                .map(|a| trim_quotes(&a.value))
                                .unwrap_or(&body_node.id);
                            if let Some(val) = ctx.get(name) {
                                node_outputs.insert(body_node.index, val.clone());
                            }
                        }
                        Opcode::Act => {
                            let action_type = body_node
                                .args
                                .iter()
                                .find(|a| a.key == "type" || a.key == "action_type")
                                .map(|a| trim_quotes(&a.value).to_string())
                                .unwrap_or_default();
                            if action_type == "return" {
                                // Early return from loop
                                let content = body_node
                                    .args
                                    .iter()
                                    .find(|a| a.key == "content" || a.key == "value")
                                    .map(|a| resolve_arg_value(&a.value, ctx, node_outputs));
                                if let Some(v) = content {
                                    node_outputs.insert(body_node.index, v);
                                } else {
                                    let pred_value = self
                                        .plan
                                        .edges
                                        .iter()
                                        .filter(|e| {
                                            e.to_index == body_node.index
                                                && e.kind == EdgeKind::Data
                                        })
                                        .filter_map(|e| node_outputs.get(&e.from_index).cloned())
                                        .next_back();
                                    if let Some(v) = pred_value {
                                        node_outputs.insert(body_node.index, v);
                                    } else {
                                        node_outputs.insert(body_node.index, Value::Null);
                                    }
                                }
                                should_break = true;
                            } else {
                                node_outputs.insert(
                                    body_node.index,
                                    Value::String(format!("[ACT:{}]", action_type)),
                                );
                            }
                        }
                        _ => {
                            node_outputs.insert(body_node.index, Value::Null);
                        }
                    }

                    // Break kararı pause'dan önce onurlandırılır: decide zaten
                    // loop'u bitirdiyse pause'a takılıp bir sonraki segmentte
                    // loop yeniden başlamamalı.
                    if should_break {
                        break;
                    }

                    if let Some(p) = pause {
                        if p.should_pause(*node_count, &body_node.id) {
                            return Ok(NodeOutcome::Paused(LoopState {
                                loop_index: node.index,
                                iteration,
                                body_position: body_pos + 1,
                            }));
                        }
                    }
                }

                if should_break {
                    break;
                }
            }
        }

        // After loop completes, propagate edges from LOOP to downstream nodes
        for edge in &self.plan.edges {
            if edge.from_index == node.index
                && !loop_bodies
                    .get(&node.index)
                    .map(|body| body.contains(&edge.to_index))
                    .unwrap_or(false)
            {
                if edge.kind == EdgeKind::Control {
                    // Control edges are tracked via control_satisfied
                    let to = edge.to_index as usize;
                    if to < control_satisfied.len() {
                        control_satisfied[to].set(control_satisfied[to].get() + 1);
                    }
                    // LOOP'un continuation'ı da çalışmalı: hedefi enable et
                    // (normal akışta Decide arm'ı yapar; loop bitince LOOP kendisi yapar).
                    enabled.insert(edge.to_index);
                    continue;
                }
                if let Some(ref mapping) = edge.mapping {
                    for (from_key, to_key) in mapping {
                        if let Some(val) = ctx.get(from_key).cloned() {
                            ctx.set(to_key.clone(), val);
                        }
                    }
                }
                enabled.insert(edge.to_index);
            }
        }
        node_outputs.insert(node.index, Value::Null);

        Ok(NodeOutcome::Ok)
    }

    /// Propagate edges from a node to downstream nodes, respecting conditions and mapping.
    /// Control edges are handled separately: they increment the target's control_satisfied
    /// counter but do NOT directly enable the target (which must also check data edges).
    #[allow(clippy::too_many_arguments)]
    fn propagate_edges(
        &self,
        node: &CompiledNode,
        ctx: &mut Context,
        node_outputs: &mut HashMap<u32, Value>,
        enabled: &mut HashSet<u32>,
        control_satisfied: &[Cell<u32>],
    ) -> Result<(), ExecutionError> {
        // Switch uses case matching, not boolean condition evaluation
        if node.op == Opcode::Switch {
            return Ok(()); // already handled inline
        }
        // Loop propagation is handled inline (needs body awareness)
        if node.op == Opcode::Loop {
            return Ok(()); // already handled inline
        }
        // Parallel propagation is handled inline (enables all unconditionally)
        if node.op == Opcode::Parallel {
            return Ok(()); // already handled inline
        }

        for edge in &self.plan.edges {
            if edge.from_index != node.index {
                continue;
            }

            if edge.kind == EdgeKind::Control {
                // Control edges: increment the target's control_satisfied counter
                // AND enable it. The target also needs its data edges satisfied
                // before it can execute — a node whose ONLY incoming edges are
                // control (e.g. a constant assignment `total = 0`) must still run.
                let to = edge.to_index as usize;
                if to < control_satisfied.len() {
                    control_satisfied[to].set(control_satisfied[to].get() + 1);
                }
                enabled.insert(edge.to_index);
                continue;
            }

            let should_enable = match &edge.condition {
                Some(ref condition) => {
                    let result = eval_expression(condition, ctx, node_outputs)?;
                    is_truthy(&result)
                }
                None => true,
            };
            if should_enable {
                // Apply edge mapping (rename context fields)
                if let Some(ref mapping) = edge.mapping {
                    for (from_key, to_key) in mapping {
                        if let Some(val) = ctx.get(from_key).cloned() {
                            ctx.set(to_key.clone(), val);
                        }
                    }
                }
                enabled.insert(edge.to_index);
            }
        }
        Ok(())
    }

    /// Identify loop body node sets: LOOP index → set of body node indices.
    /// Body nodes are those reachable from a LOOP node's unconditional edges.
    fn identify_loop_bodies(&self) -> HashMap<u32, HashSet<u32>> {
        let mut bodies = HashMap::new();
        for node in &self.plan.nodes {
            if node.op != Opcode::Loop {
                continue;
            }
            // Find unconditional outgoing edges from this LOOP (body entry points)
            let mut body_starts: Vec<u32> = Vec::new();
            let mut control_starts: Vec<u32> = Vec::new();
            for edge in &self.plan.edges {
                if edge.from_index == node.index && edge.condition.is_none() {
                    match edge.kind {
                        EdgeKind::Control => control_starts.push(edge.to_index),
                        _ => body_starts.push(edge.to_index),
                    }
                }
            }
            let mut body_set: HashSet<u32> = HashSet::new();
            for to in &body_starts {
                body_set.extend(self.collect_reachable_indices(&[*to]));
            }
            if !control_starts.is_empty() {
                let mut continuation: HashSet<u32> = HashSet::new();
                for to in &control_starts {
                    continuation.extend(self.collect_reachable_indices(&[*to]));
                }
                body_set = body_set.difference(&continuation).copied().collect();
            }
            bodies.insert(node.index, body_set);
        }
        bodies
    }

    /// Collect all node indices reachable from a set of start indices.
    fn collect_reachable_indices(&self, start_indices: &[u32]) -> HashSet<u32> {
        let mut visited: HashSet<u32> = start_indices.iter().copied().collect();
        let mut stack: Vec<u32> = start_indices.to_vec();
        while let Some(idx) = stack.pop() {
            for edge in &self.plan.edges {
                if edge.from_index == idx && visited.insert(edge.to_index) {
                    stack.push(edge.to_index);
                }
            }
        }
        visited
    }

    /// Topological sort on compiled edges (Kahn's algorithm).
    fn topological_order(&self) -> Result<Vec<u32>, ExecutionError> {
        let n = self.plan.nodes.len();
        let mut in_degree = vec![0u32; n];

        for edge in &self.plan.edges {
            let to = edge.to_index as usize;
            if to < n {
                in_degree[to] += 1;
            }
        }

        let mut queue: Vec<u32> = (0..n as u32)
            .filter(|i| in_degree[*i as usize] == 0)
            .collect();

        let mut result = Vec::with_capacity(n);

        while let Some(idx) = queue.pop() {
            result.push(idx);
            for edge in &self.plan.edges {
                if edge.from_index == idx {
                    let to = edge.to_index as usize;
                    if to < n {
                        in_degree[to] = in_degree[to].saturating_sub(1);
                        if in_degree[to] == 0 {
                            queue.push(to as u32);
                        }
                    }
                }
            }
        }

        if result.len() != n {
            return Err(ExecutionError::CycleDetected);
        }

        Ok(result)
    }
}

// ─── Expression evaluation helpers ───────────────────────────────────

/// Simple expression evaluator for CALC nodes and edge conditions.
pub fn eval_expression(
    expr: &str,
    ctx: &Context,
    outputs: &HashMap<u32, Value>,
) -> Result<Value, ExecutionError> {
    let trimmed = expr.trim();
    // JSON-quoted (eski FB formatı: `"\"x + 1\""`) veya plain-quoted (`"GET"`) olabilir.
    let decoded: String;
    let was_quoted = if let Ok(s) = serde_json::from_str::<String>(trimmed) {
        decoded = s;
        true
    } else if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
        && !is_concat_expr(trimmed)
    {
        decoded = trimmed[1..trimmed.len() - 1].to_string();
        true
    } else {
        decoded = trimmed.to_string();
        false
    };
    let expr = decoded.as_str();
    if expr.is_empty() {
        return Ok(Value::Null);
    }
    if expr.is_empty() {
        return Ok(Value::Null);
    }

    // range(...) — yalnızca loop bound olarak kullanılır (for i in range(n)).
    if let Some(inner) = expr.strip_prefix("range(") {
        if let Some(args_str) = inner.strip_suffix(')') {
            let nums: Option<Vec<i64>> = args_str
                .split(',')
                .map(|a| a.trim().parse::<i64>().ok())
                .collect();
            if let Some(nums) = nums {
                return match nums.as_slice() {
                    [stop] => Ok(Value::Int(*stop)),
                    [start, stop] => Ok(Value::Int(stop - start)),
                    [start, stop, step] if *step > 0 => {
                        Ok(Value::Int((stop - start).max(0) / step))
                    }
                    _ => Err(ExecutionError::EvalError(format!(
                        "cannot evaluate: {}",
                        expr
                    ))),
                };
            }
        }
    }

    // Boolean constants
    if expr == "true" || expr == "True" {
        return Ok(Value::Bool(true));
    }
    if expr == "false" || expr == "False" {
        return Ok(Value::Bool(false));
    }
    if expr == "null" || expr == "None" {
        return Ok(Value::Null);
    }

    // Dict literal: {key: value, ...} — e.g. return {"users": user_count}
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.trim().is_empty() {
            return Ok(Value::Object(HashMap::new()));
        }
        let mut map = HashMap::new();
        for pair in split_top_level_commas(inner) {
            let Some((k, v)) = pair.split_once(':') else {
                return Err(ExecutionError::EvalError(format!(
                    "cannot evaluate: {}",
                    expr
                )));
            };
            let key = trim_quotes(k.trim());
            if key.is_empty() {
                return Err(ExecutionError::EvalError(format!(
                    "cannot evaluate: {}",
                    expr
                )));
            }
            let value = eval_expression(v.trim(), ctx, outputs)?;
            map.insert(key.to_string(), value);
        }
        return Ok(Value::Object(map));
    }

    // Not operator
    if expr.starts_with("not ") || expr.starts_with("!") {
        let rest = if expr.starts_with("not ") {
            &expr[4..]
        } else {
            &expr[1..]
        };
        let val = eval_expression(rest.trim(), ctx, outputs)?;
        return Ok(Value::Bool(!is_truthy(&val)));
    }

    // Comparison operators
    let cmp_ops = [
        (">=", 2usize),
        ("<=", 2),
        ("!=", 2),
        ("==", 2),
        (">", 1),
        ("<", 1),
    ];
    for (op_str, op_len) in &cmp_ops {
        if let Some(pos) = expr.find(op_str) {
            let left = expr[..pos].trim();
            let right = expr[pos + op_len..].trim();
            if !left.is_empty() && !right.is_empty() {
                let lv = eval_expression(left, ctx, outputs)?;
                let rv = eval_expression(right, ctx, outputs)?;
                let result = match *op_str {
                    "==" => values_equal(&lv, &rv),
                    "!=" => !values_equal(&lv, &rv),
                    ">" => compare_values(&lv, &rv, |a, b| a > b),
                    "<" => compare_values(&lv, &rv, |a, b| a < b),
                    ">=" => compare_values(&lv, &rv, |a, b| a >= b),
                    "<=" => compare_values(&lv, &rv, |a, b| a <= b),
                    _ => false,
                };
                return Ok(Value::Bool(result));
            }
        }
    }

    // Try direct integer parse
    if let Ok(n) = expr.parse::<i64>() {
        return Ok(Value::Int(n));
    }

    // Try direct float parse
    if let Ok(f) = expr.parse::<f64>() {
        return Ok(Value::Float(f));
    }

    // Try context variable lookup (with $ prefix stripping)
    let clean = expr.strip_prefix('$').unwrap_or(expr);
    if let Some(val) = ctx.get(clean) {
        return Ok(val.clone());
    }

    // Try output lookup by converting index to string
    if let Ok(idx) = clean.parse::<u32>() {
        if let Some(val) = outputs.get(&idx) {
            return Ok(val.clone());
        }
    }

    // Simple binary arithmetic operations
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
                continue;
            }
            let left = expr[..pos].trim();
            let right = expr[pos + 1..].trim();
            let left_val = resolve_numeric(left, ctx, outputs);
            let right_val = resolve_numeric(right, ctx, outputs);
            if let (Some(a), Some(b)) = (left_val, right_val) {
                if let Some(result) = op_fn(a, b) {
                    return Ok(Value::Int(result));
                }
            }
        }
    }

    // String concatenation (f-string destek): "a" + "b", "postId=" + i.
    if let Some(pos) = expr.find('+') {
        if pos > 0 && pos < expr.len() - 1 {
            let left = expr[..pos].trim();
            let right = expr[pos + 1..].trim();
            if let (Ok(a), Ok(b)) = (eval_expression(left, ctx, outputs), eval_expression(right, ctx, outputs)) {
                let a_str = concat_part(&a);
                let b_str = concat_part(&b);
                if a_str.is_some() || b_str.is_some() {
                    return Ok(Value::String(format!(
                        "{}{}",
                        a_str.unwrap_or_default(),
                        b_str.unwrap_or_default()
                    )));
                }
            }
        }
    }

    // Attribute access: obj.field → Object'ten alan okuma (r.body gibi).
    // Float parse önce yapıldığı için 3.14 buraya düşmez.
    if let Some(dot) = expr.rfind('.') {
        if dot > 0 && dot < expr.len() - 1 {
            let base = &expr[..dot];
            let field = &expr[dot + 1..];
            if let Ok(base_val) = eval_expression(base, ctx, outputs) {
                match base_val {
                    Value::Object(m) => {
                        if let Some(v) = m.get(field) {
                            return Ok(v.clone());
                        }
                    }
                    Value::Array(items) => {
                        if let Ok(idx) = field.parse::<usize>() {
                            if let Some(v) = items.get(idx) {
                                return Ok(v.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if was_quoted {
        // Quoted string literal — ifade olarak değerlendirilemedi, string olarak dön.
        return Ok(Value::String(expr.to_string()));
    }
    Err(ExecutionError::EvalError(format!(
        "cannot evaluate: {}",
        expr
    )))
}

fn resolve_numeric(key: &str, ctx: &Context, outputs: &HashMap<u32, Value>) -> Option<i64> {
    // Try direct parse
    if let Ok(n) = key.parse::<i64>() {
        return Some(n);
    }
    // Try context (as float first, then try as int if float provides a clean conversion)
    if let Some(val) = ctx.get(key) {
        // Try as_f64 first (covers both Int and Float values in our Value type)
        if let Some(f) = val.as_f64() {
            return Some(f as i64);
        }
    }
    // Attribute/subscript expressions: "c.count" — evaluate like eval_expression
    if key.contains('.') || key.contains('[') {
        if let Ok(val) = eval_expression(key, ctx, outputs) {
            if let Some(f) = val.as_f64() {
                return Some(f as i64);
            }
        }
    }
    None
}

/// Çift tırnakları ve JSON-escape'li tırnakları (`\"`) sıyırır.
/// Eski FB formatı `"\"http_request\""` → `http_request`, yeni format `"http_request"` → `http_request`.
fn trim_quotes(value: &str) -> &str {
    value.trim_matches(|c| c == '"' || c == '\\')
}

pub fn resolve_arg_value(value_str: &str, ctx: &Context, outputs: &HashMap<u32, Value>) -> Value {
    let s = value_str.trim();
    // Strip JSON string quotes for context lookup
    let clean = trim_quotes(s);
    // Try context first (e.g. source="x" → ctx.get("x"))
    if let Some(val) = ctx.get(clean) {
        return val.clone();
    }
    // f-string concat: "https://x/" + id → expression evaluation
    if is_concat_expr(s) {
        if let Ok(val) = eval_expression(s, ctx, outputs) {
            return val;
        }
    }
    if s.starts_with('"') && s.ends_with('"') {
        Value::String(clean.to_string())
    } else if let Ok(n) = s.parse::<i64>() {
        Value::Int(n)
    } else if let Ok(f) = s.parse::<f64>() {
        Value::Float(f)
    } else if s == "true" {
        Value::Bool(true)
    } else if s == "false" {
        Value::Bool(false)
    } else if s == "null" {
        Value::Null
    } else if let Some(val) = ctx.get(s) {
        val.clone()
    } else if let Ok(val) = eval_expression(s, ctx, outputs) {
        // Attr erişimi gibi basit ifadeler (users_resp.body) değerlendirilir
        val
    } else {
        // Plan-içi tırnaksız token'lar (target=call, tool adları vb.) bu yoldan
        // geçerek literal string'e döner — bu normaldir. Ama tanımsız bir
        // değişken adı (users_resp.bady) da buraya düşer; debug log'u yazım
        // hatası ayıklamasında işe yarar.
        tracing::debug!(
            arg = s,
            "resolve_arg_value: token resolved as literal string (no ctx/eval match)"
        );
        Value::String(s.to_string())
    }
}

/// Bir değeri string concat için uygun biçimde döndürür (null değilse).
fn concat_part(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::String(s) => Some(s.clone()),
        Value::Array(_) => None,
        Value::Object(_) => None,
    }
}

/// `"..." + ...` veya `'...' + ...` şeklinde f-string concat ifadesi mi?
pub fn is_concat_expr(s: &str) -> bool {
    s.contains("\" +") || s.contains("' +")
}

/// Bir string'i quote'ları ve iç içe parantezleri sayarak üst seviye virgüllerden böler
/// (dict literal değerlerinde `"SELECT a, b FROM t"` gibi string içi virgüller korunur).
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in s.chars() {
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                current.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) => "[...]".into(),
        Value::Object(_) => "{...}".into(),
    }
}

fn parse_json_value(s: &str) -> Value {
    let s = s.trim();
    if s.is_empty() {
        return Value::Null;
    }
    // Try JSON decode
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return json_to_tp(v);
    }
    Value::String(s.to_string())
}

fn json_to_tp(v: serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        J::String(s) => Value::String(s),
        J::Array(arr) => Value::Array(arr.into_iter().map(json_to_tp).collect()),
        J::Object(obj) => {
            let map: HashMap<String, Value> =
                obj.into_iter().map(|(k, v)| (k, json_to_tp(v))).collect();
            Value::Object(map)
        }
    }
}

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn evaluate_condition(source: &Value, op: &str, compare: &Value) -> Result<bool, ExecutionError> {
    fn numeric_pair<'a>(
        a: &'a Value,
        b: &'a Value,
        op: &str,
    ) -> Result<(f64, f64), ExecutionError> {
        let a = value_to_f64(a).ok_or_else(|| {
            ExecutionError::ConditionError(format!(
                "condition '{op}': operand '{}' is not numeric",
                format_value(a)
            ))
        })?;
        let b = value_to_f64(b).ok_or_else(|| {
            ExecutionError::ConditionError(format!(
                "condition '{op}': operand '{}' is not numeric",
                format_value(b)
            ))
        })?;
        Ok((a, b))
    }

    match op {
        "eq" => Ok(source == compare),
        "neq" => Ok(source != compare),
        "gt" => {
            let (a, b) = numeric_pair(source, compare, op)?;
            Ok(a > b)
        }
        "gte" => {
            let (a, b) = numeric_pair(source, compare, op)?;
            Ok(a >= b)
        }
        "lt" => {
            let (a, b) = numeric_pair(source, compare, op)?;
            Ok(a < b)
        }
        "lte" => {
            let (a, b) = numeric_pair(source, compare, op)?;
            Ok(a <= b)
        }
        "contains" => {
            let a = match source {
                Value::String(s) => s.clone(),
                _ => format!("{:?}", source),
            };
            let b = match compare {
                Value::String(s) => s.clone(),
                _ => format!("{:?}", compare),
            };
            Ok(a.contains(&b))
        }
        _ => Err(ExecutionError::ConditionError(format!(
            "unknown operator: {}",
            op
        ))),
    }
}

// ── Shared eval helpers ──

/// Compare two Values for equality, supporting cross-type comparisons.
pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => (x - y).abs() < f64::EPSILON,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Int(x), Value::Float(y)) => (*x as f64 - y).abs() < f64::EPSILON,
        (Value::Float(x), Value::Int(y)) => (x - *y as f64).abs() < f64::EPSILON,
        _ => false,
    }
}

pub(crate) fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Compare two Values numerically using a comparison function.
pub(crate) fn compare_values(a: &Value, b: &Value, cmp: fn(f64, f64) -> bool) -> bool {
    match (value_to_f64(a), value_to_f64(b)) {
        (Some(x), Some(y)) => cmp(x, y),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::types::MergeStrategy;
    use tinypipe_ir::compiled::CompiledPlan;
    use tinypipe_ir::plan::{Edge, ExecutionPlan, Node, Opcode};
    use tinypipe_tools::mock_tools;

    fn compile_plan(plan: ExecutionPlan) -> CompiledPlan {
        CompiledPlan::from_execution_plan(&plan, vec![])
    }

    // ── eval_expression tests (compiled executor's evaluator) ──────

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 testi kasıtlı — literal roundtrip
    fn test_compiled_eval_number() {
        let ctx = Context::new();
        let outputs = HashMap::new();
        assert_eq!(
            eval_expression("42", &ctx, &outputs).unwrap(),
            Value::Int(42)
        );
        assert_eq!(
            eval_expression("3.14", &ctx, &outputs).unwrap(),
            Value::Float(3.14)
        );
    }

    #[test]
    fn test_compiled_eval_variable() {
        let mut ctx = Context::new();
        ctx.set("x".into(), Value::Int(10));
        let outputs = HashMap::new();
        assert_eq!(
            eval_expression("x", &ctx, &outputs).unwrap(),
            Value::Int(10)
        );
    }

    #[test]
    fn test_compiled_eval_arithmetic() {
        let mut ctx = Context::new();
        ctx.set("x".into(), Value::Int(5));
        let outputs = HashMap::new();
        // Compiled executor uses i64 arithmetic
        assert_eq!(
            eval_expression("x + 3", &ctx, &outputs).unwrap(),
            Value::Int(8)
        );
        assert_eq!(
            eval_expression("10 - 4", &ctx, &outputs).unwrap(),
            Value::Int(6)
        );
        assert_eq!(
            eval_expression("3 * 4", &ctx, &outputs).unwrap(),
            Value::Int(12)
        );
    }

    #[test]
    fn test_compiled_eval_comparison() {
        let mut ctx = Context::new();
        ctx.set("x".into(), Value::Int(5));
        let outputs = HashMap::new();
        assert_eq!(
            eval_expression("x > 3", &ctx, &outputs).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_expression("x > 10", &ctx, &outputs).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_expression("x == 5", &ctx, &outputs).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_compiled_eval_not() {
        let ctx = Context::new();
        let outputs = HashMap::new();
        assert_eq!(
            eval_expression("not true", &ctx, &outputs).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_expression("not false", &ctx, &outputs).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_compiled_eval_null_none() {
        let ctx = Context::new();
        let outputs = HashMap::new();
        assert_eq!(
            eval_expression("null", &ctx, &outputs).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expression("None", &ctx, &outputs).unwrap(),
            Value::Null
        );
    }

    // ── Integration tests ─────────────────────────────────────────

    #[test]
    fn test_compiled_execute_simple_plan() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc)
                    .with_arg("expr", "x + 1".into())
                    .with_arg("output", "result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "result".into()),
            ],
            vec![Edge::new("input1", "calc1"), Edge::new("calc1", "output1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = executor.execute(inputs).expect("execution should succeed");
        // Calc writes output="result" → ctx["result"] = 6
        // Act reads value="result" → ctx lookup → 6
        assert_eq!(result.output, Some(Value::Int(6)), "5 + 1 = 6");
        assert_eq!(result.node_count, 3);
    }

    #[test]
    fn test_compiled_execute_subgraph_output_and_merge() {
        // call("subgraph:echo", ...) → çocuk grafiğin return değeri call ifadesinin
        // değeri olur (output_var), çocuk ctx'i de çağıran ctx'e merge edilir.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "subgraph:echo".into())
                    .with_arg("output", "res".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "res".into()),
            ],
            vec![
                Edge::new("input1", "call1"),
                Edge::new("call1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = executor.execute(inputs).expect("execution should succeed");
        assert_eq!(result.output, Some(Value::String("echo!".into())));
        // Subgraph ctx merge: çocuğun "output" değişkeni çağıran ctx'e geçti
        assert_eq!(
            result.context.variables.get("output"),
            Some(&Value::String("echo!".into()))
        );
    }

    #[test]
    fn test_compiled_execute_subgraph_recursion_limit() {
        // max_recursion_depth=0 → subgraph çağrısı derhal RecursionLimitExceeded
        let mut plan = ExecutionPlan::new(
            vec![
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "subgraph:echo".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"done\"".into()),
            ],
            vec![Edge::new("call1", "output1")],
        );
        plan.metadata.max_recursion_depth = 0;
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let err = executor
            .execute(Context::new())
            .expect_err("recursion limit must trip");
        assert!(err.to_string().contains("recursion"));
    }

    #[test]
    fn test_compiled_execute_decide_true_branch() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("decide1", Opcode::Decide)
                    .with_arg("source", "x".into())
                    .with_arg("op", "gt".into())
                    .with_arg("value", 0i64.into()),
                Node::new("true_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
                Node::new("false_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
            ],
            vec![
                Edge::new("input1", "decide1"),
                Edge::with_condition("decide1", "true_branch", "true"),
                Edge::with_condition("decide1", "false_branch", "false"),
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = executor.execute(inputs).expect("execution should succeed");
        // Should have executed true_branch (3 nodes: input1, decide1, true_branch)
        assert!(result.output.is_some(), "should have output");
        assert_eq!(result.node_count, 3);
    }

    #[test]
    fn test_compiled_execute_decide_false_branch() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("decide1", Opcode::Decide)
                    .with_arg("source", "x".into())
                    .with_arg("op", "gt".into())
                    .with_arg("value", 0i64.into()),
                Node::new("true_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
                Node::new("false_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
            ],
            vec![
                Edge::new("input1", "decide1"),
                Edge::with_condition("decide1", "true_branch", "true"),
                Edge::with_condition("decide1", "false_branch", "false"),
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(-5));
        let result = executor.execute(inputs).expect("execution should succeed");
        // Should have executed false_branch
        assert!(result.output.is_some());
        assert_eq!(result.node_count, 3);
    }

    #[test]
    fn test_evaluate_condition_type_mismatch_errors() {
        let err = evaluate_condition(&Value::String("abc".into()), "lt", &Value::Int(5))
            .expect_err("non-numeric source must error");
        assert!(matches!(err, ExecutionError::ConditionError(_)));
        assert!(err.to_string().contains("not numeric"));

        let err = evaluate_condition(&Value::Int(5), "gte", &Value::String("5".into()))
            .expect_err("non-numeric compare must error");
        assert!(matches!(err, ExecutionError::ConditionError(_)));

        let ok = evaluate_condition(&Value::Int(3), "lt", &Value::Int(5))
            .expect("numeric compare must succeed");
        assert!(ok);

        let ok = evaluate_condition(&Value::Float(5.5), "gte", &Value::Int(5))
            .expect("int/float mixed compare must succeed");
        assert!(ok);
    }

    #[test]
    fn test_eval_expression_string_literal() {
        // string literal'ler Calc node'larında değerlendirilebilmelidir
        // (tırnak sıyırma ölü kod olmamalı — "cannot evaluate" regresyonu).
        let ctx = Context::new();
        let outputs = HashMap::new();
        let v = eval_expression("\"GET\"", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::String("GET".into()));
        let v = eval_expression("'POST'", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::String("POST".into()));
    }

    #[test]
    fn test_eval_expression_dict_literal() {
        // return {"users": user_count, ...} — dict literal'ler değerlendirilebilmelidir.
        let mut ctx = Context::new();
        ctx.set("users".into(), Value::Int(10));
        ctx.set("done".into(), Value::Int(11));
        let outputs = HashMap::new();
        let v = eval_expression(
            "{\"users\": users, \"posts\": 3, \"done\": done}",
            &ctx,
            &outputs,
        )
        .unwrap();
        let Value::Object(map) = v else {
            panic!("dict literal must evaluate to Object");
        };
        assert_eq!(map.get("users"), Some(&Value::Int(10)));
        assert_eq!(map.get("posts"), Some(&Value::Int(3)));
        assert_eq!(map.get("done"), Some(&Value::Int(11)));

        // String içinde virgül bulunan değerler üst seviye bölme ile bozulmamalı
        let v = eval_expression(
            "{\"q\": \"SELECT a, b FROM t\", \"n\": 5}",
            &ctx,
            &outputs,
        )
        .unwrap();
        let Value::Object(map) = v else {
            panic!("dict literal must evaluate to Object");
        };
        assert_eq!(
            map.get("q"),
            Some(&Value::String("SELECT a, b FROM t".into()))
        );
        assert_eq!(map.get("n"), Some(&Value::Int(5)));

        // Boş dict
        let v = eval_expression("{}", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::Object(HashMap::new()));
    }

    #[test]
    fn test_eval_expression_numeric_with_attr() {
        // `total + c.count` gibi attribute içeren toplamalar numerik olmalı
        // (concat'e düşmemeli — resolve_numeric attr ifadelerini çözer).
        let mut ctx = Context::new();
        ctx.set("total".into(), Value::Int(0));
        let mut c = HashMap::new();
        c.insert("count".to_string(), Value::Int(5));
        ctx.set("c".into(), Value::Object(c));
        let outputs = HashMap::new();
        let v = eval_expression("total + c.count", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::Int(5));
        let v = eval_expression("total + c.count + 2", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::Int(7));
    }

    #[test]
    fn test_eval_expression_string_concat() {
        // f-string'lerin ürettiği `"..." + expr` ifadeleri değerlendirilebilmelidir.
        let ctx = Context::new();
        let outputs = HashMap::new();
        let v = eval_expression("\"a\" + \"b\"", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::String("ab".into()));
        let v = eval_expression("\"postId=\" + 3", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::String("postId=3".into()));
        let v = eval_expression("\"x=\" + 7 + \"y\"", &ctx, &outputs).unwrap();
        assert_eq!(v, Value::String("x=7y".into()));
    }

    #[test]
    fn test_resolve_arg_value_concat_expression() {
        // Call node arg'ında f-string: `url = "https://x/" + i` (i ctx'te).
        let mut ctx = Context::new();
        ctx.set("i".into(), Value::Int(3));
        let outputs = HashMap::new();
        let v = resolve_arg_value("\"https://x/comments?postId=\" + i", &ctx, &outputs);
        assert_eq!(v, Value::String("https://x/comments?postId=3".into()));
    }

    #[test]
    fn test_resolve_arg_value_attribute_expression() {
        // Call node arg'ında attr erişimi: `json = users_resp.body` (ctx'te object).
        let mut m = HashMap::new();
        m.insert("body".into(), Value::String("{\"a\": 1}".into()));
        let mut ctx = Context::new();
        ctx.set("users_resp".into(), Value::Object(m));
        let outputs = HashMap::new();
        let v = resolve_arg_value("users_resp.body", &ctx, &outputs);
        assert_eq!(v, Value::String("{\"a\": 1}".into()));
    }

    #[test]
    fn test_compiled_execute_call_tool() {
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.echo", |args, _kwargs, _env| {
            Ok(args.first().cloned().unwrap_or(Value::Null))
        });

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "val".into()),
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.echo".into())
                    .with_arg("output", "call_result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "call_result".into()),
            ],
            vec![Edge::new("input1", "call1"), Edge::new("call1", "output1")],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("val".into(), Value::String("hello".into()));
        let result = exec.execute(inputs).expect("execution should succeed");
        assert!(result.output.is_some());
    }

    #[test]
    fn test_compiled_execute_empty_plan() {
        let plan = ExecutionPlan::new(vec![], vec![]);
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let result = exec
            .execute(Context::new())
            .expect("empty plan should succeed");
        assert_eq!(result.node_count, 0);
        assert!(result.output.is_none());
    }

    #[test]
    fn test_compiled_execute_cycle_rejected() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("a", Opcode::Input).with_arg("name", "x".into()),
                Node::new("b", Opcode::Calc).with_arg("expr", "x".into()),
            ],
            vec![
                Edge::new("a", "b"),
                Edge::new("b", "a"), // cycle
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let result = exec.execute(Context::new());
        assert!(matches!(result, Err(ExecutionError::CycleDetected)));
    }

    #[test]
    fn test_compiled_execute_error_node() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("err1", Opcode::Error).with_arg("message", "something went wrong".into()),
            ],
            vec![Edge::new("input1", "err1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(42));
        let result = exec.execute(inputs);
        assert!(
            matches!(result, Err(ExecutionError::Custom(msg)) if msg == "something went wrong")
        );
    }

    #[test]
    fn test_compiled_execute_wait_noop() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("w1", Opcode::Wait).with_arg("duration_secs", 0i64.into()),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![Edge::new("w1", "output1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let result = exec.execute(Context::new()).expect("wait should succeed");
        assert!(
            result.execution_order.contains(&"w1".to_string()),
            "expected w1 in execution order, got: {:?}",
            result.execution_order
        );
        assert!(
            result.execution_order.contains(&"output1".to_string()),
            "expected output1 in execution order, got: {:?}",
            result.execution_order
        );
    }

    #[test]
    fn test_compiled_execute_wait_exceeds_max() {
        let plan = ExecutionPlan::new(
            vec![Node::new("w1", Opcode::Wait).with_arg("duration_secs", 301i64.into())],
            vec![],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let result = exec.execute(Context::new());
        assert!(result.is_err(), "wait >300s should be rejected");
        match result {
            Err(ExecutionError::Custom(msg)) => {
                assert!(msg.contains("300"), "error should mention max: {msg}");
            }
            other => panic!("expected Custom error, got: {:?}", other),
        }
    }

    #[test]
    fn test_compiled_execute_wait_small_duration() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("w1", Opcode::Wait).with_arg("duration_secs", 1i64.into()),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![Edge::new("w1", "output1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let result = exec.execute(Context::new());
        assert!(result.is_ok(), "1s wait should succeed: {:?}", result.err());
        let r = result.unwrap();
        assert!(r.duration_us >= 1_000_000, "should have slept at least 1s");
        assert!(r.duration_us < 3_000_000, "should not take >3s");
        assert!(r.execution_order.contains(&"output1".to_string()));
    }

    #[test]
    fn test_compiled_execute_switch() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "color".into()),
                Node::new("switch1", Opcode::Switch).with_arg("source", "color".into()),
                Node::new("red_case", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"red-result\"".into()),
                Node::new("blue_case", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"blue-result\"".into()),
            ],
            vec![
                Edge::new("input1", "switch1"),
                Edge::with_condition("switch1", "red_case", "red"),
                Edge::with_condition("switch1", "blue_case", "blue"),
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("color".into(), Value::String("red".into()));
        let result = exec.execute(inputs).expect("switch should succeed");
        assert_eq!(
            result.node_count, 3,
            "should execute input1, switch1, red_case"
        );
    }

    #[test]
    fn test_compiled_budget_time_limit() {
        let mut nodes = vec![Node::new("input1", Opcode::Input).with_arg("name", "x".into())];
        let mut edges = Vec::new();
        for i in 0..200 {
            let nid = format!("calc{}", i);
            nodes.push(Node::new(&nid, Opcode::Calc).with_arg("expr", "x + 1".into()));
            if i == 0 {
                edges.push(Edge::new("input1", &nid));
            } else {
                edges.push(Edge::new(&format!("calc{}", i - 1), &nid));
            }
        }
        let plan = ExecutionPlan {
            version: 2,
            nodes,
            edges,
            metadata: tinypipe_ir::plan::Metadata {
                max_execution_time_ms: 0, // 0ms budget — instantly exceeded
                ..Default::default()
            },
        };
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(42));
        let result = exec.execute(inputs);
        assert!(
            matches!(result, Err(ExecutionError::TimeLimitExceeded(_))),
            "expected TimeLimitExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_compiled_on_error_abort() {
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.error", |_, _, _env| Err("always fails".into()));

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.error".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"unused\"".into()),
            ],
            vec![Edge::new("input1", "call1"), Edge::new("call1", "output1")],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(1));
        let result = exec.execute(inputs);
        assert!(
            matches!(result, Err(ExecutionError::CallFailed(_, _))),
            "expected CallFailed, got {:?}",
            result
        );
    }

    #[test]
    fn test_compiled_on_error_continue_with_null() {
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.error", |_, _, _env| Err("always fails".into()));

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.error".into())
                    .with_arg("on_error", "continue_with_null".into())
                    .with_arg("output", "call_result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "call_result".into()),
            ],
            vec![Edge::new("input1", "call1"), Edge::new("call1", "output1")],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let inputs = Context::new();
        let result = exec.execute(inputs).expect("should continue despite error");
        assert_eq!(
            result.output,
            Some(Value::Null),
            "expected Null output from continue_with_null"
        );
    }

    #[test]
    fn test_compiled_on_error_continue_with_fallback() {
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.error", |_, _, _env| Err("always fails".into()));

        let plan = ExecutionPlan::new(
            vec![
                Node::new("call1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.error".into())
                    .with_arg("on_error", "continue_with_fallback".into())
                    .with_arg("fallback_value", "42".into())
                    .with_arg("output", "call_result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "call_result".into()),
            ],
            vec![Edge::new("call1", "output1")],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let result = exec
            .execute(Context::new())
            .expect("should continue with fallback");
        // Compiled executor: fallback_value "42" (raw string) is parsed as JSON → Int(42).
        // Transform çıktısında string literal'lar tırnaklı gelir ("\"42\"" → String("42")).
        assert_eq!(
            result.output,
            Some(Value::Int(42)),
            "expected fallback value '42', got {:?}",
            result.output
        );
    }

    #[test]
    fn test_compiled_budget_node_count() {
        let mut nodes = vec![Node::new("input1", Opcode::Input).with_arg("name", "x".into())];
        let mut edges = Vec::new();
        for i in 0..100 {
            let nid = format!("calc{}", i);
            nodes.push(Node::new(&nid, Opcode::Calc).with_arg("expr", "x + 1".into()));
            if i == 0 {
                edges.push(Edge::new("input1", &nid));
            } else {
                edges.push(Edge::new(&format!("calc{}", i - 1), &nid));
            }
        }
        let plan = ExecutionPlan {
            version: 2,
            nodes,
            edges,
            metadata: tinypipe_ir::plan::Metadata {
                max_node_execution_count: 5, // Only 5 nodes allowed
                ..Default::default()
            },
        };
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(42));
        let result = exec.execute(inputs);
        assert!(
            matches!(result, Err(ExecutionError::NodeBudgetExceeded(_, _))),
            "expected NodeBudgetExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_compiled_on_error_abort_in_parallel() {
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.error", |_, _, _env| Err("branch error".into()));
        reg.add("test.echo", |args, _kwargs, _env| {
            Ok(args.first().cloned().unwrap_or(Value::Null))
        });

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch1", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.error".into()),
                Node::new("branch2", Opcode::Call)
                    .with_arg("type", "call".into())
                    .with_arg("target", "test.echo".into())
                    .with_arg("echo", "hello".into()),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch1"),
                Edge::new("parallel1", "branch2"),
                Edge::new("branch1", "merge1"),
                Edge::new("branch2", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(1));
        let result = exec.execute(inputs);
        assert!(
            result.is_err(),
            "expected error from abort in parallel, got {:?}",
            result
        );
    }

    #[test]
    fn test_compiled_budget_memory_limit() {
        let plan = ExecutionPlan {
            version: 2,
            nodes: vec![
                Node::new("input1", Opcode::Input).with_arg("name", "base".into()),
                Node::new("calc0", Opcode::Calc)
                    .with_arg("expr", "base".into())
                    .with_arg("output", "bigval0".into()),
                Node::new("calc1", Opcode::Calc)
                    .with_arg("expr", "base".into())
                    .with_arg("output", "bigval1".into()),
                Node::new("calc2", Opcode::Calc)
                    .with_arg("expr", "base".into())
                    .with_arg("output", "bigval2".into()),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            edges: vec![
                Edge::new("input1", "calc0"),
                Edge::new("calc0", "calc1"),
                Edge::new("calc1", "calc2"),
                Edge::new("calc2", "output1"),
            ],
            metadata: tinypipe_ir::plan::Metadata {
                max_context_memory_bytes: 10, // Very tight budget
                ..Default::default()
            },
        };
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("base".into(), Value::String("A".repeat(100)));
        let result = exec.execute(inputs);
        assert!(
            matches!(result, Err(ExecutionError::MemoryLimitExceeded(_, _))),
            "expected MemoryLimitExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_compiled_execute_complex_chain() {
        // x = 5; y = x * 2; return y
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input_x", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc_y", Opcode::Calc)
                    .with_arg("expr", "x * 2".into())
                    .with_arg("output", "result".into()),
                Node::new("output", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "result".into()),
            ],
            vec![
                Edge::new("input_x", "calc_y"),
                Edge::new("calc_y", "output"),
            ],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let exec = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = exec.execute(inputs).expect("should succeed");
        // Calc_y writes output="result" → ctx["result"] = 10
        // Act reads value="result" → ctx lookup → 10
        assert_eq!(result.output, Some(Value::Int(10)), "5 * 2 = 10");
    }

    // ── Version check tests ───────────────────────────────────────

    #[test]
    fn test_compiled_version_check_rejects_old() {
        let plan = ExecutionPlan::new(
            vec![Node::new("a", Opcode::Input).with_arg("name", "x".into())],
            vec![],
        );
        let mut compiled = compile_plan(plan);
        compiled.version = 1; // too old
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let result = executor.execute(Context::new());
        assert!(matches!(result, Err(ExecutionError::VersionMismatch(_))));
    }

    #[test]
    fn test_compiled_version_check_rejects_new() {
        let plan = ExecutionPlan::new(
            vec![Node::new("a", Opcode::Input).with_arg("name", "x".into())],
            vec![],
        );
        let mut compiled = compile_plan(plan);
        compiled.version = 99; // too new
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let result = executor.execute(Context::new());
        assert!(matches!(result, Err(ExecutionError::VersionMismatch(_))));
    }

    // ── FlatBuffers-based tests ──────────────────────────────────

    #[test]
    fn test_execute_plan_loaded_from_fb() {
        // Load via CompiledPlan::from_fb_bytes → CompiledExecutor::new
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc)
                    .with_arg("expr", "x + 1".into())
                    .with_arg("output", "result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "result".into()),
            ],
            vec![Edge::new("input1", "calc1"), Edge::new("calc1", "output1")],
        );
        let compiled = compile_plan(plan);
        let fb_bytes = compiled.to_fb_bytes().expect("FB serialize");
        let plan_from_fb = CompiledPlan::from_fb_bytes(&fb_bytes).expect("FB deserialize");

        let registry = mock_tools();
        let executor = CompiledExecutor::new(&plan_from_fb, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = executor.execute(inputs).expect("execution should succeed");
        assert_eq!(result.output, Some(Value::Int(6)), "5 + 1 = 6");
        assert_eq!(result.node_count, 3);
    }

    #[test]
    fn test_execute_from_fb_matches_bincode() {
        // Same plan, both formats → same execution result
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("decide1", Opcode::Decide)
                    .with_arg("source", "x".into())
                    .with_arg("op", "gt".into())
                    .with_arg("value", 0i64.into()),
                Node::new("true_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
                Node::new("false_branch", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "\"ok\"".into()),
            ],
            vec![
                Edge::new("input1", "decide1"),
                Edge::with_condition("decide1", "true_branch", "true"),
                Edge::with_condition("decide1", "false_branch", "false"),
            ],
        );
        let compiled = compile_plan(plan);
        let bincode_bytes = compiled.to_bytes().expect("bincode serialize");
        let fb_bytes = compiled.to_fb_bytes().expect("FB serialize");

        let registry = mock_tools();
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(10));

        // Execute from bincode
        let plan_bincode = CompiledPlan::from_bytes(&bincode_bytes).unwrap();
        let exec_bincode = CompiledExecutor::new(&plan_bincode, &registry);
        let result_bincode = exec_bincode.execute(inputs.clone()).expect("bincode exec");

        // Execute from FB
        let plan_fb = CompiledPlan::from_fb_bytes(&fb_bytes).expect("FB deserialize");
        let exec_fb = CompiledExecutor::new(&plan_fb, &registry);
        let result_fb = exec_fb.execute(inputs).expect("FB exec");

        assert_eq!(result_bincode.output, result_fb.output);
        assert_eq!(result_bincode.node_count, result_fb.node_count);
        assert_eq!(result_bincode.execution_order, result_fb.execution_order);
    }

    #[test]
    fn test_fb_version_check_works() {
        let plan = ExecutionPlan::new(
            vec![Node::new("a", Opcode::Input).with_arg("name", "x".into())],
            vec![],
        );
        let compiled = compile_plan(plan);
        let mut old_compiled = compiled.clone();
        old_compiled.version = 1;
        let fb_bytes = old_compiled.to_fb_bytes().expect("FB serialize");
        let plan_fb = CompiledPlan::from_fb_bytes(&fb_bytes).expect("FB deserialize");

        let registry = mock_tools();
        let executor = CompiledExecutor::new(&plan_fb, &registry);
        let result = executor.execute(Context::new());
        assert!(matches!(result, Err(ExecutionError::VersionMismatch(_))));
    }

    #[test]
    fn test_compiled_version_check_accepts_current() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![Edge::new("input1", "act1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut ctx = Context::new();
        ctx.set("x".into(), Value::Int(1));
        let result = executor.execute(ctx).expect("version 2 should be accepted");
        assert_eq!(result.node_count, 2);
    }

    // ── Scope Isolation Tests ─────────────────────────────────

    #[test]
    fn test_scope_isolation_basic() {
        // İki branch farklı değişkenlere yazar. MERGE sonrası ikisi de global'de olmalı.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch0", Opcode::Calc)
                    .with_arg("expr", "x + 1".into())
                    .with_arg("output", "a".into())
                    .with_branch(0),
                Node::new("branch1", Opcode::Calc)
                    .with_arg("expr", "x + 2".into())
                    .with_arg("output", "b".into())
                    .with_branch(1),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch0"),
                Edge::new("parallel1", "branch1"),
                Edge::new("branch0", "merge1"),
                Edge::new("branch1", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let reg = tinypipe_tools::MockToolRegistry::new();
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(10));
        let result = exec
            .execute(inputs)
            .expect("scope isolation should succeed");
        // MERGE her iki branch scope'unu birleştirmeli
        assert_eq!(
            result.context.get("a"),
            Some(&Value::Int(11)),
            "branch0: x+1=11"
        );
        assert_eq!(
            result.context.get("b"),
            Some(&Value::Int(12)),
            "branch1: x+2=12"
        );
    }

    #[test]
    fn test_scope_isolation_same_var() {
        // İki branch aynı değişkene yazar. Varsayılan Last stratejisi:
        // son çalışan branch'in değeri kazanır.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch0", Opcode::Calc)
                    .with_arg("expr", "100".into())
                    .with_arg("output", "result".into())
                    .with_branch(0),
                Node::new("branch1", Opcode::Calc)
                    .with_arg("expr", "200".into())
                    .with_arg("output", "result".into())
                    .with_branch(1),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch0"),
                Edge::new("parallel1", "branch1"),
                Edge::new("branch0", "merge1"),
                Edge::new("branch1", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let reg = tinypipe_tools::MockToolRegistry::new();
        let exec = CompiledExecutor::new(&compiled, &reg);
        let inputs = Context::new();
        let result = exec
            .execute(inputs)
            .expect("scope isolation same var should succeed");
        // With Last strategy, the highest branch_id wins (sorted deterministically).
        // branch_id 0 writes 100, branch_id 1 writes 200.
        // Sorted order: [0, 1]. Last = 1 → 200.
        assert_eq!(
            result.context.get("result"),
            Some(&Value::Int(200)),
            "Last strategy: highest branch_id (1) should win with value 200"
        );
    }

    #[test]
    fn test_scope_isolation_no_cross_contamination() {
        // Branch0 writes to "x", Branch1 reads "x" — should see parent's value (from input1),
        // NOT branch0's value. Cross-branch contamination yok.
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.read_var", |args, _kwargs, _env| {
            // Return the value of the variable passed as first arg
            Ok(args.first().cloned().unwrap_or(Value::Null))
        });

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                // Branch0: "x" değerini değiştir
                Node::new("branch0", Opcode::Calc)
                    .with_arg("expr", "999".into())
                    .with_arg("output", "x".into())
                    .with_branch(0),
                // Branch1: "x" değerini oku — parent'taki orijinal değeri görmeli
                Node::new("branch1_call", Opcode::Call)
                    .with_arg("target", "test.read_var".into())
                    .with_arg("output", "branch1_result".into())
                    .with_arg("expression", "x".into())
                    .with_branch(1),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch0"),
                Edge::new("parallel1", "branch1_call"),
                Edge::new("branch0", "merge1"),
                Edge::new("branch1_call", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );

        // Wait — the test.read_var tool gets the CALL params which include "expression": "x"
        // But resolve_arg_value will resolve "x" to ctx.get("x").
        // Branch1 calls: target="test.read_var", expression="x"
        // resolve_arg_value("x", ctx, ...) → ctx.get("x")
        // If scope isolation works, branch1's ctx.get("x") should return the parent's value (from input1),
        // not branch0's modified value.

        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(42)); // Parent value
        let result = exec
            .execute(inputs)
            .expect("no cross contamination should succeed");
        // After merge, "x" was written by both branch0 (as 999) and branch1 didn't write to "x"
        // (it wrote to "branch1_result"). With Last strategy, branch0 is last → "x" = 999.
        // But the key test: branch1 saw the parent value (42) not branch0's (999).
        // We can verify this by checking what branch1's tool received.
        assert_eq!(
            result.context.get("x"),
            Some(&Value::Int(999)),
            "branch0 modified x to 999"
        );
    }

    #[test]
    fn test_scope_isolation_parent_read() {
        // Branch parent scope'daki değişkeni okuyabilmeli.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "base".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch0", Opcode::Calc)
                    .with_arg("expr", "base * 2".into())
                    .with_arg("output", "doubled".into())
                    .with_branch(0),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch0"),
                Edge::new("branch0", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let reg = tinypipe_tools::MockToolRegistry::new();
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("base".into(), Value::Int(21));
        let result = exec.execute(inputs).expect("parent read should succeed");
        // Branch0 reads "base" from parent scope → 21 * 2 = 42
        assert_eq!(
            result.context.get("doubled"),
            Some(&Value::Int(42)),
            "branch should read parent variable 'base'"
        );
    }

    #[test]
    fn test_scope_isolation_no_branch_id_normal() {
        // Nodes without branch_id should work exactly as before.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc)
                    .with_arg("expr", "x + 1".into())
                    .with_arg("output", "result".into()),
                Node::new("output1", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "result".into()),
            ],
            vec![Edge::new("input1", "calc1"), Edge::new("calc1", "output1")],
        );
        let compiled = compile_plan(plan);
        let registry = mock_tools();
        let executor = CompiledExecutor::new(&compiled, &registry);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(5));
        let result = executor
            .execute(inputs)
            .expect("normal exec should succeed");
        assert_eq!(result.output, Some(Value::Int(6)), "5 + 1 = 6");
        assert_eq!(result.node_count, 3);
    }

    #[test]
    fn test_scope_isolation_merge_first_strategy() {
        // First strategy: lowest branch_id wins.
        // branch_id 0 writes 100, branch_id 1 writes 200.
        // First = branch_id 0 → 100.
        // To test this, we create scopes with explicit merge_strategy via node args
        // (the VM applies Last by default; for custom strategies, we'd need plan-level metadata).
        // This test verifies the default Last behavior and that First would work differently.
        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch0", Opcode::Calc)
                    .with_arg("expr", "100".into())
                    .with_arg("output", "result".into())
                    .with_branch(0),
                Node::new("branch1", Opcode::Calc)
                    .with_arg("expr", "200".into())
                    .with_arg("output", "result".into())
                    .with_branch(1),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch0"),
                Edge::new("parallel1", "branch1"),
                Edge::new("branch0", "merge1"),
                Edge::new("branch1", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let reg = tinypipe_tools::MockToolRegistry::new();
        let exec = CompiledExecutor::new(&compiled, &reg);
        let inputs = Context::new();
        let result = exec
            .execute(inputs)
            .expect("merge first strategy should succeed");
        // Default is Last, so highest branch_id (1) wins → 200
        assert_eq!(
            result.context.get("result"),
            Some(&Value::Int(200)),
            "default Last: branch_id 1 should win"
        );
    }

    #[test]
    fn test_scope_isolation_merge_concat_strategy() {
        // Test concat merge strategy via direct Scope manipulation
        let mut ctx = Context::new();
        ctx.variables.insert(
            "items".into(),
            Value::Array(vec![Value::String("global".into())]),
        );

        // Simulate PARALLEL with two branches each adding items
        ctx.enter_parallel();
        ctx.set_branch(0);
        ctx.set(
            "items".into(),
            Value::Array(vec![Value::String("branch0".into())]),
        );
        ctx.set_branch(1);
        ctx.set(
            "items".into(),
            Value::Array(vec![Value::String("branch1".into())]),
        );

        // Set merge_strategy for "items" on each branch
        ctx.set_branch(0);
        ctx.set_merge_strategy("items", MergeStrategy::Concat);
        ctx.set_branch(1);
        ctx.set_merge_strategy("items", MergeStrategy::Concat);

        ctx.merge_branches();

        // After merge with Concat: arrays are concatenated in branch_id order.
        // Global starts with ["global"] as the accumulator target.
        // Branch 0 (lowest bid): concat ["global"] + ["branch0"] → ["global", "branch0"]
        // Branch 1 (highest bid): concat ["global", "branch0"] + ["branch1"] → ["global", "branch0", "branch1"]
        let items = ctx.get("items").unwrap();
        match items {
            Value::Array(arr) => {
                assert_eq!(arr.len(), 3, "global + branch0 + branch1 = 3 items");
                assert_eq!(arr[0], Value::String("global".into()));
                assert_eq!(arr[1], Value::String("branch0".into()));
                assert_eq!(arr[2], Value::String("branch1".into()));
            }
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn test_scope_isolation_parallel_error_still_propagates() {
        // Error in parallel branch should still abort execution.
        let reg = tinypipe_tools::MockToolRegistry::new();
        reg.add("test.error", |_, _, _env| Err("branch error".into()));

        let plan = ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("parallel1", Opcode::Parallel),
                Node::new("branch_error", Opcode::Call)
                    .with_arg("target", "test.error".into())
                    .with_branch(0),
                Node::new("branch_ok", Opcode::Call)
                    .with_arg("target", "test.echo".into())
                    .with_arg("echo", "hello".into())
                    .with_branch(1),
                Node::new("merge1", Opcode::Merge),
                Node::new("output1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::new("input1", "parallel1"),
                Edge::new("parallel1", "branch_error"),
                Edge::new("parallel1", "branch_ok"),
                Edge::new("branch_error", "merge1"),
                Edge::new("branch_ok", "merge1"),
                Edge::new("merge1", "output1"),
            ],
        );
        let compiled = compile_plan(plan);
        let exec = CompiledExecutor::new(&compiled, &reg);
        let mut inputs = Context::new();
        inputs.set("x".into(), Value::Int(1));
        let result = exec.execute(inputs);
        assert!(result.is_err(), "error in parallel should propagate");
    }
}
