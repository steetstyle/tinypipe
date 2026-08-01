use std::collections::HashMap;
use std::sync::Arc;

use tinypipe_api::storage::GraphStorage;
use tinypipe_api::types::{Context, Execution, ExecutionStatus, ExecutionStep, GraphId, Version};
use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_compiler::compile;
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_storage::SqliteStorage;
use tinypipe_tools::daemon::{json_to_tp, tp_to_json};
use tinypipe_vm::{Checkpoint, CompiledExecutor, ExecutionOutcome, PausePolicy};

use crate::meta::{parse_route_config, RouteConfig};
use crate::state::AppState;

pub fn now_micros() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// UUID veya isim → graph definition.
pub fn resolve_graph(
    storage: &Arc<SqliteStorage>,
    id_or_name: &str,
) -> Result<tinypipe_api::storage::GraphDefinition, String> {
    match storage.load_graph(&GraphId::new(id_or_name)) {
        Ok(g) => Ok(g),
        Err(tinypipe_api::types::StorageError::GraphNotFound(_)) => storage
            .find_graph_by_name(id_or_name)
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Mevcut versiyonun plan'ını yükler; yoksa koddan derleyip kaydeder (self-heal).
pub fn load_plan_self_heal(
    storage: &Arc<SqliteStorage>,
    graph: &tinypipe_api::storage::GraphDefinition,
) -> Result<CompiledPlan, String> {
    let bytes = match storage.load_plan(&graph.id) {
        Ok(bytes) => bytes,
        Err(tinypipe_api::types::StorageError::PlanMissing(_)) => {
            let output = compile(&graph.code).map_err(|e| format!("recompile: {e}"))?;
            storage
                .save_plan(&graph.id, graph.version, &output.fb_binary)
                .map_err(|e| e.to_string())?;
            output.fb_binary
        }
        Err(e) => return Err(e.to_string()),
    };
    CompiledPlan::from_fb_bytes(&bytes).map_err(|e| format!("plan decode: {e}"))
}

/// İstek env override'ları + OS env (override kazanır).
pub fn build_env(overrides: &HashMap<String, String>) -> tinypipe_env::Env {
    tinypipe_env::Env::new(vec![
        Arc::new(tinypipe_env::static_provider::StaticEnvProvider::new(
            overrides.clone(),
        )),
        Arc::new(tinypipe_env::os::OsEnvProvider),
    ])
}

pub fn ctx_from_inputs(inputs: &serde_json::Value) -> Context {
    let mut ctx = Context::new();
    if let serde_json::Value::Object(map) = inputs {
        for (k, v) in map {
            ctx.set(k.clone(), json_to_tp(v.clone()));
        }
    }
    ctx
}

/// `http_timeout_ms` override'ı: plan kopyalanıp bütçe güncellenir.
fn apply_timeout(plan: CompiledPlan, timeout_override: Option<u32>) -> CompiledPlan {
    match timeout_override {
        Some(0) | None => plan,
        Some(ms) => {
            let mut plan = plan;
            plan.metadata.max_execution_time_ms = ms;
            plan
        }
    }
}

/// Ortak execute çekirdeği. `graph=None` (dinamik kod) ise env check ve audit atlanır.
/// Dönüş: `{status, execution_id?, duration_us, nodes_executed, output?, error?}`.
#[allow(clippy::too_many_arguments)]
pub fn run_execution(
    state: &Arc<AppState>,
    graph: Option<&tinypipe_api::storage::GraphDefinition>,
    plan: CompiledPlan,
    inputs: &serde_json::Value,
    env_over: &HashMap<String, String>,
    pause_after: Option<u32>,
    no_env_check: bool,
    timeout_override: Option<u32>,
    audit: bool,
) -> Result<serde_json::Value, String> {
    let plan = apply_timeout(plan, timeout_override);

    let env = build_env(env_over);
    if let Some(g) = graph {
        if !no_env_check {
            let reports = state
                .registry
                .validate_env(&g.name, &env)
                .map_err(|e| e.to_string())?;
            if !reports.is_empty() {
                let mut lines = Vec::new();
                for r in &reports {
                    lines.push(format!("{}: {}", r.graph_path, r.missing.join(", ")));
                }
                return Err(format!("missing environment variables:\n{}", lines.join("\n")));
            }
        }
    }

    let ctx = ctx_from_inputs(inputs);
    let execution_id = uuid::Uuid::new_v4().to_string();

    let mut execution = Execution {
        id: execution_id.clone(),
        graph_id: graph.map(|g| g.id.clone()).unwrap_or_else(|| GraphId::new("dynamic")),
        graph_version: graph.map(|g| g.version).unwrap_or(Version(0)),
        input: ctx.clone(),
        output: None,
        status: ExecutionStatus::Running,
        error: None,
        started_at: now_micros(),
        completed_at: None,
        duration_us: None,
        context: None,
    };
    if audit {
        state
            .storage
            .save_execution(&execution)
            .map_err(|e| format!("save execution: {e}"))?;
    }

    let executor = CompiledExecutor::with_env(&plan, registry_ref(state), Arc::new(env));
    let policy = PausePolicy {
        max_nodes: pause_after,
        pause_at_node_ids: None,
    };
    let outcome = executor.execute_with(ctx, &policy, None);

    let mut resp = serde_json::Map::new();
    resp.insert("execution_id".into(), serde_json::json!(execution_id));
    match outcome {
        Ok(ExecutionOutcome::Completed(result)) => {
            resp.insert("status".into(), serde_json::json!("completed"));
            resp.insert("duration_us".into(), serde_json::json!(result.duration_us));
            resp.insert("nodes_executed".into(), serde_json::json!(result.node_count));
            resp.insert(
                "output".into(),
                result
                    .output
                    .as_ref()
                    .map(tp_to_json)
                    .unwrap_or(serde_json::Value::Null),
            );
            if audit {
                execution.status = ExecutionStatus::Completed;
                execution.output = result.output.clone();
                execution.duration_us = Some(result.duration_us);
                execution.completed_at = Some(now_micros());
                execution.context = Some(result.context.clone());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|e| format!("save execution: {e}"))?;
                record_steps(&state.storage, &execution_id, &plan, &result);
            }
        }
        Ok(ExecutionOutcome::Paused(checkpoint)) => {
            resp.insert("status".into(), serde_json::json!("paused"));
            resp.insert(
                "message".into(),
                serde_json::json!(format!(
                    "paused at {} nodes — POST /api/executions/{execution_id}/resume",
                    checkpoint.node_count
                )),
            );
            if audit {
                execution.status = ExecutionStatus::Paused;
                execution.completed_at = Some(now_micros());
                execution.context = Some(checkpoint.context.clone());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|e| format!("save execution: {e}"))?;
                let blob = serde_json::to_vec(&checkpoint).map_err(|e| e.to_string())?;
                state
                    .storage
                    .save_checkpoint(&execution_id, &blob)
                    .map_err(|e| format!("save checkpoint: {e}"))?;
            }
        }
        Err(e) => {
            resp.insert("status".into(), serde_json::json!("failed"));
            resp.insert("error".into(), serde_json::json!(e.to_string()));
            if audit {
                execution.status = ExecutionStatus::Failed;
                execution.error = Some(e.to_string());
                execution.completed_at = Some(now_micros());
                execution.context = Some(execution.input.clone());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|err| format!("save execution: {err}"))?;
            }
        }
    }
    Ok(serde_json::Value::Object(resp))
}

/// `POST /api/executions/{id}/resume` — checkpoint'ten sürdürür.
pub fn resume_execution(
    state: &Arc<AppState>,
    execution_id: &str,
    max_nodes: Option<u32>,
    env_over: &HashMap<String, String>,
) -> Result<serde_json::Value, String> {
    let mut execution = state
        .storage
        .load_execution(execution_id)
        .map_err(|e| format!("execution '{execution_id}': {e}"))?;
    if execution.status != ExecutionStatus::Paused {
        return Err(format!("execution '{execution_id}' is not paused"));
    }

    let blob = state
        .storage
        .load_checkpoint(execution_id)
        .map_err(|e| e.to_string())?;
    let checkpoint: Checkpoint =
        serde_json::from_slice(&blob).map_err(|e| format!("checkpoint decode: {e}"))?;

    let plan_bytes = state
        .storage
        .load_plan_version(&execution.graph_id, execution.graph_version)
        .map_err(|e| e.to_string())?;
    let plan = CompiledPlan::from_fb_bytes(&plan_bytes).map_err(|e| format!("plan decode: {e}"))?;

    let env = build_env(env_over);
    let executor = CompiledExecutor::with_env(&plan, registry_ref(state), Arc::new(env));
    let policy = PausePolicy {
        max_nodes,
        pause_at_node_ids: None,
    };
    let outcome = executor.resume(&checkpoint, &policy, None);

    let mut resp = serde_json::Map::new();
    resp.insert("execution_id".into(), serde_json::json!(execution_id));
    match outcome {
        Ok(ExecutionOutcome::Completed(result)) => {
            resp.insert("status".into(), serde_json::json!("completed"));
            resp.insert("duration_us".into(), serde_json::json!(result.duration_us));
            resp.insert("nodes_executed".into(), serde_json::json!(result.node_count));
            resp.insert(
                "output".into(),
                result
                    .output
                    .as_ref()
                    .map(tp_to_json)
                    .unwrap_or(serde_json::Value::Null),
            );
            if state.audit {
                execution.status = ExecutionStatus::Completed;
                execution.output = result.output.clone();
                execution.duration_us = Some(result.duration_us);
                execution.completed_at = Some(now_micros());
                execution.context = Some(result.context.clone());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|e| e.to_string())?;
                record_steps(&state.storage, execution_id, &plan, &result);
            }
        }
        Ok(ExecutionOutcome::Paused(cp)) => {
            resp.insert("status".into(), serde_json::json!("paused"));
            resp.insert("node_count".into(), serde_json::json!(cp.node_count));
            if state.audit {
                execution.status = ExecutionStatus::Paused;
                execution.context = Some(cp.context.clone());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|e| e.to_string())?;
                let blob = serde_json::to_vec(&cp).map_err(|e| e.to_string())?;
                state
                    .storage
                    .save_checkpoint(execution_id, &blob)
                    .map_err(|e| e.to_string())?;
            }
        }
        Err(e) => {
            resp.insert("status".into(), serde_json::json!("failed"));
            resp.insert("error".into(), serde_json::json!(e.to_string()));
            if state.audit {
                execution.status = ExecutionStatus::Failed;
                execution.error = Some(e.to_string());
                execution.completed_at = Some(now_micros());
                state
                    .storage
                    .save_execution(&execution)
                    .map_err(|err| err.to_string())?;
            }
        }
    }
    Ok(serde_json::Value::Object(resp))
}

/// `POST /api/scheduler/run` — tüm paused execution'ları ilerletir.
pub fn run_scheduler(
    state: &Arc<AppState>,
    max_nodes: Option<u32>,
) -> Result<serde_json::Value, String> {
    let paused = state
        .storage
        .list_paused_executions()
        .map_err(|e| e.to_string())?;
    let mut processed = 0usize;
    let mut completed = 0usize;
    let mut still_paused = 0usize;
    let mut failed = 0usize;

    for exec in &paused {
        processed += 1;
        match resume_execution(state, &exec.id, max_nodes, &HashMap::new()) {
            Ok(v) => match v.get("status").and_then(|s| s.as_str()) {
                Some("completed") => completed += 1,
                Some("paused") => still_paused += 1,
                _ => failed += 1,
            },
            Err(_) => failed += 1,
        }
    }

    Ok(serde_json::json!({
        "processed": processed,
        "completed": completed,
        "still_paused": still_paused,
        "failed": failed,
    }))
}

fn registry_ref(state: &Arc<AppState>) -> &dyn ToolRegistry {
    state.registry.as_ref() as &dyn ToolRegistry
}

/// Per-node audit kayıtları (CLI `record_steps` ile aynı mantık).
pub fn record_steps(
    storage: &Arc<SqliteStorage>,
    execution_id: &str,
    plan: &CompiledPlan,
    result: &tinypipe_vm::ExecutionResult,
) {
    let node_by_id: HashMap<&str, &tinypipe_ir::compiled::CompiledNode> =
        plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut cursor: u64 = now_micros().parse().unwrap_or(0);
    for (node_id, duration_us) in &result.node_durations {
        let op = node_by_id
            .get(node_id.as_str())
            .map(|n| format!("{:?}", n.op))
            .unwrap_or_else(|| "unknown".into());
        let started_at = cursor;
        let completed_at = started_at + duration_us;
        cursor = completed_at;
        let step = ExecutionStep {
            id: uuid::Uuid::new_v4().to_string(),
            execution_id: execution_id.to_string(),
            node_id: node_id.clone(),
            node_op: op,
            status: "completed".into(),
            error: None,
            started_at: started_at.to_string(),
            completed_at: Some(completed_at.to_string()),
            duration_us: Some(*duration_us),
            context_before: None,
            context_after: None,
            parent_step_id: None,
        };
        let _ = storage.save_step(&step);
    }
}

/// Plan + route tablolarını storage'dan yeniden kurar.
/// META `http_*` doğrulaması fail-fast: hatalı config tüm isteği döndürür.
pub async fn refresh_all(state: &Arc<AppState>) -> Result<(usize, usize), String> {
    let graphs = state
        .storage
        .list_all_graphs(None, None)
        .map_err(|e| e.to_string())?;
    let mut plans: HashMap<String, Arc<CompiledPlan>> = HashMap::new();
    let mut routes: HashMap<String, RouteConfig> = HashMap::new();
    for g in &graphs {
        let plan = load_plan_self_heal(&state.storage, g)?;
        let meta = plan.metadata.meta_json.clone();
        match parse_route_config(&meta, &g.name) {
            Ok(Some(mut rc)) => {
                rc.graph_id = g.id.0.clone();
                let key = crate::meta::route_key(&rc.path, rc.method);
                if let Some(existing) = routes.get(&key) {
                    return Err(format!(
                        "duplicate http_route '/{}' ({}): graphs '{}' and '{}'",
                        rc.path,
                        rc.method.as_str(),
                        existing.graph_id,
                        g.name
                    ));
                }
                routes.insert(key, rc);
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        plans.insert(g.id.0.clone(), Arc::new(plan));
    }
    *state.plans.write().await = plans;
    *state.routes.write().await = routes;
    *state.resp_cache.write().await = HashMap::new();
    let n = state.plans.read().await.len();
    let r = state.routes.read().await.len();
    Ok((n, r))
}
