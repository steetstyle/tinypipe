use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tinypipe_api::storage::GraphStorage;
use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{CallTarget, ToolSpec, Version};
use tinypipe_compiler::compile;
use tinypipe_ir::plan_dump::{PlanDumpHeader, PlanFormat};
use tinypipe_ir::plan_view::{Direction, RenderOptions, ViewLevel};
use tinypipe_tools::daemon::{daemon_addr_from_env, invoke_daemon_tool, list_daemon_tools, tp_to_json};

use crate::engine::{
    build_env, load_plan_self_heal, refresh_all, resolve_graph, resume_execution, run_execution,
    run_scheduler,
};
use crate::meta::parse_route_config;
use crate::state::AppState;

// ==================== Errors ====================

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Unauthorized(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

fn err_from(s: String) -> ApiError {
    if s.contains("not found") || s.contains("not exist") {
        ApiError::NotFound(s)
    } else if s.starts_with("missing environment variables") || s.contains("compile failed")
        || s.contains("parse error") {
        ApiError::BadRequest(s)
    } else {
        ApiError::Internal(s)
    }
}

fn require_token(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<()> {
    let Some(expected) = &state.token else {
        return Err(ApiError::Unauthorized(
            "server token not configured (set TINYPIPE_SERVER_TOKEN)".into(),
        ));
    };
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(t) if t == expected => Ok(()),
        _ => Err(ApiError::Unauthorized("invalid or missing bearer token".into())),
    }
}

async fn run_async<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> std::result::Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Internal(format!("task join: {e}")))?
        .map_err(err_from)
}

// ==================== Request/Response types ====================

#[derive(Deserialize)]
pub struct CheckReq {
    pub code: String,
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub code: String,
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub code: String,
}

#[derive(Deserialize)]
pub struct DeployReq {
    pub version: Option<u64>,
}

#[derive(Deserialize)]
pub struct RollbackReq {
    pub version: u64,
}

#[derive(Deserialize)]
pub struct ExecuteReq {
    pub inputs: Option<serde_json::Value>,
    pub env: Option<HashMap<String, String>>,
    pub pause_after: Option<u32>,
    pub no_env_check: Option<bool>,
    pub timeout_ms: Option<u32>,
}

#[derive(Deserialize)]
pub struct ResumeReq {
    pub max_nodes: Option<u32>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
pub struct SchedulerReq {
    pub max_nodes: Option<u32>,
}

#[derive(Deserialize)]
pub struct RunReq {
    pub code: String,
    pub inputs: Option<serde_json::Value>,
    pub env: Option<HashMap<String, String>>,
    pub pause_after: Option<u32>,
}

#[derive(Deserialize)]
pub struct ToolsTestReq {
    pub name: String,
    pub args: Option<Vec<serde_json::Value>>,
    pub kwargs: Option<serde_json::Value>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
pub struct ProfileReq {
    pub label: Option<String>,
    pub description: Option<String>,
    pub view: Option<String>,
    pub direction: Option<String>,
    pub focus: Option<Vec<String>>,
    pub config: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct PlanQuery {
    pub version: Option<u64>,
    pub format: Option<String>,
    pub view: Option<String>,
    pub direction: Option<String>,
    pub profile: Option<String>,
}

#[derive(Deserialize)]
pub struct ReportQuery {
    pub profile: Option<String>,
    pub env: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct ExecQuery {
    pub graph_id: Option<String>,
}

#[derive(Deserialize)]
pub struct DaemonQuery {
    pub addr: Option<String>,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub graphs: usize,
    pub routes: usize,
    pub plans: usize,
    pub audit: bool,
}

// ==================== Open endpoints ====================

pub async fn healthz(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let (graphs, routes, plans) = {
        let (g, p, r) = (
            state.storage.list_all_graphs(None, None).map(|v| v.len()).unwrap_or(0),
            state.routes.read().await.len(),
            state.plans.read().await.len(),
        );
        (g, r, p)
    };
    Json(HealthResponse {
        ok: true,
        graphs,
        routes,
        plans,
        audit: state.audit,
    })
}

pub async fn list_graphs(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let graphs = state
        .storage
        .list_all_graphs(None, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let items: Vec<serde_json::Value> = graphs
        .iter()
        .map(|g| {
            serde_json::json!({
                "id": g.id.0,
                "name": g.name,
                "version": g.version,
                "status": g.status,
                "active": g.active,
                "active_version": g.active_version,
                "last_event": g.last_event,
                "created_at": g.created_at,
                "fork_node": g.fork_node,
                "fork_label": g.fork_label,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "graphs": items })))
}

pub async fn list_versions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let graph = resolve_graph(&state.storage, &id).map_err(err_from)?;
    let versions = state
        .storage
        .list_versions(&graph.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::to_value(&versions).map_err(|e| ApiError::Internal(e.to_string()))?))
}

pub async fn execute_graph(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExecuteReq>,
) -> ApiResult<Response> {
    let state2 = state.clone();
    let id2 = id.clone();
    let outcome = run_async(move || {
        let graph = resolve_graph(&state2.storage, &id2)?;
        let plan = load_plan_self_heal(&state2.storage, &graph)?;
        run_execution(
            &state2,
            Some(&graph),
            plan,
            req.inputs.as_ref().unwrap_or(&serde_json::json!({})),
            req.env.as_ref().unwrap_or(&HashMap::new()),
            req.pause_after,
            req.no_env_check.unwrap_or(false),
            req.timeout_ms,
            state2.audit,
        )
    }).await?;
    classify_execution(outcome)
}

pub async fn tools_list(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let builtin: Vec<serde_json::Value> = state
        .registry
        .tool_names()
        .iter()
        .map(|name| {
            let spec: Option<ToolSpec> = state.registry.resolve(name, "0").ok();
            serde_json::json!({
                "name": name,
                "description": spec.as_ref().map(|s| &s.description).cloned().unwrap_or_default(),
                "version": spec.as_ref().map(|s| &s.version).cloned().unwrap_or_else(|| "0".into()),
                "schema_hash": spec.as_ref().map(|s| &s.schema_hash).cloned().unwrap_or_default(),
            })
        })
        .collect();

    let addr = daemon_addr_from_env();
    match list_daemon_tools(&addr) {
        Ok(tools) => Ok(Json(serde_json::json!({
            "ok": true,
            "daemon": {
                "addr": addr,
                "tools": tools.iter().map(daemon_tool_json).collect::<Vec<_>>(),
            },
            "builtin": builtin,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "ok": false,
            "daemon": { "addr": addr, "error": e },
            "builtin": builtin,
        }))),
    }
}

pub async fn daemon_status(
    Query(q): Query<DaemonQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let addr = q.addr.unwrap_or_else(daemon_addr_from_env);
    match list_daemon_tools(&addr) {
        Ok(tools) => Ok(Json(serde_json::json!({
            "ok": true,
            "addr": addr,
            "tools": tools.iter().map(daemon_tool_json).collect::<Vec<_>>(),
        }))),
        Err(e) => Ok(Json(serde_json::json!({ "ok": false, "addr": addr, "error": e }))),
    }
}

fn daemon_tool_json(t: &tinypipe_proto::tinypipe::v1::ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "timeout_ms": t.timeout_ms,
    })
}

// ==================== Token-protected endpoints ====================

pub async fn check_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CheckReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let body = run_async(move || {
        let output = compile(&req.code).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "ok": true,
            "node_count": output.compiled.nodes.len(),
            "edge_count": output.compiled.edges.len(),
            "max_execution_time_ms": output.compiled.metadata.max_execution_time_ms,
            "meta": output.compiled.metadata.meta_json,
            "compiled_bytes": output.fb_binary.len(),
        }))
    }).await?;
    Ok(Json(body))
}

async fn validate_route_conflict(
    state: &Arc<AppState>,
    graph_id: &str,
    code: &str,
) -> ApiResult<()> {
    let compiled = compile(code).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if let Some(rc) = parse_route_config(&compiled.compiled.metadata.meta_json, "")
        .map_err(ApiError::BadRequest)?
    {
        // Aynı (path, method) çifti tek graph'a aittir; farklı metodlar farklı
        // graph'lardan aynı path'e yayınlanabilir (ör. GET + POST /items).
        let routes = state.routes.read().await;
        let key = crate::meta::route_key(&rc.path, rc.method);
        if let Some(existing) = routes.get(&key) {
            if existing.graph_id != graph_id {
                return Err(ApiError::Conflict(format!(
                    "route '/{}' ({}) is already published by graph '{}'",
                    rc.path,
                    rc.method.as_str(),
                    existing.graph_id
                )));
            }
        }
    }
    Ok(())
}

pub async fn create_graph(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> ApiResult<Response> {
    require_token(&state, &headers)?;
    validate_route_conflict(&state, "", &req.code).await?;
    let state2 = state.clone();
    let (id, name) = run_async(move || {
        let output = compile(&req.code).map_err(|e| format!("compile failed: {e}"))?;
        let graph_id = state2
            .storage
            .create_graph(&req.name, &req.code)
            .map_err(|e| format!("create: {e}"))?;
        state2
            .storage
            .save_plan(&graph_id, Version(1), &output.fb_binary)
            .map_err(|e| format!("save plan: {e}"))?;
        Ok((graph_id.0.clone(), req.name))
    }).await?;
    let (plans, routes) = refresh_all(&state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("graph created but route registration failed: {e}")))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "graph_id": id,
            "name": name,
            "version": 1,
            "published_routes": routes,
            "cached_plans": plans,
        })),
    )
        .into_response())
}

pub async fn update_graph(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let graph = resolve_graph(&state.storage, &id).map_err(err_from)?;
    validate_route_conflict(&state, &graph.id.0, &req.code).await?;
    let state2 = state.clone();
    let graph_id = graph.id.clone();
    let version = run_async(move || {
        let output = compile(&req.code).map_err(|e| format!("compile failed: {e}"))?;
        let new_version = state2
            .storage
            .update_graph(&graph_id, &req.code)
            .map_err(|e| format!("update: {e}"))?;
        state2
            .storage
            .save_plan(&graph_id, new_version, &output.fb_binary)
            .map_err(|e| format!("save plan: {e}"))?;
        Ok(new_version)
    }).await?;
    refresh_all(&state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("updated but route registration failed: {e}")))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "graph_id": graph.id.0,
        "version": version,
    })))
}

pub async fn deploy_graph(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<DeployReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let graph = resolve_graph(&state.storage, &id).map_err(err_from)?;
    let version = match req.version {
        Some(v) => v,
        None => state
            .storage
            .list_versions(&graph.id)
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .last()
            .map(|(v, _, _)| *v)
            .ok_or_else(|| ApiError::NotFound("no versions to deploy".into()))?,
    };
    state
        .storage
        .deploy(&graph.id, Version(version))
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    refresh_all(&state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("deployed but route registration failed: {e}")))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "graph_id": graph.id.0,
        "version": version,
        "active": true,
    })))
}

pub async fn rollback_graph(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<RollbackReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let graph = resolve_graph(&state.storage, &id).map_err(err_from)?;
    state
        .storage
        .rollback(&graph.id, Version(req.version))
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    refresh_all(&state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("rolled back but route registration failed: {e}")))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "graph_id": graph.id.0,
        "version": req.version,
    })))
}

pub async fn resume(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ResumeReq>,
) -> ApiResult<Response> {
    require_token(&state, &headers)?;
    let state2 = state.clone();
    let outcome = run_async(move || {
        resume_execution(&state2, &id, req.max_nodes, req.env.as_ref().unwrap_or(&HashMap::new()))
    }).await?;
    classify_execution(outcome)
}

pub async fn scheduler_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SchedulerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let state2 = state.clone();
    let body = run_async(move || run_scheduler(&state2, req.max_nodes)).await?;
    Ok(Json(body))
}

pub async fn list_executions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ExecQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let Some(id_or_name) = q.graph_id else {
        return Err(ApiError::BadRequest("query param 'graph_id' is required".into()));
    };
    let graph_id = resolve_graph(&state.storage, &id_or_name).map_err(err_from)?.id;
    let execs = state
        .storage
        .list_executions(&graph_id, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::to_value(&execs).map_err(|e| ApiError::Internal(e.to_string()))?))
}

pub async fn show_execution(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let exec = state
        .storage
        .load_execution(&id)
        .map_err(|e| ApiError::NotFound(format!("execution '{id}': {e}")))?;
    let steps = state
        .storage
        .list_steps(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "execution": exec,
        "steps": steps,
    })))
}

pub async fn plan_dump(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<PlanQuery>,
) -> ApiResult<Response> {
    require_token(&state, &headers)?;
    let graph = resolve_graph(&state.storage, &id).map_err(err_from)?;
    let state2 = state.clone();
    let graph2 = graph.clone();
    let body = run_async(move || {
        let plan_bytes = match q.version {
            Some(v) => state2
                .storage
                .load_plan_version(&graph2.id, Version(v))
                .map_err(|e| e.to_string())?,
            None => state2
                .storage
                .load_plan(&graph2.id)
                .map_err(|e| e.to_string())?,
        };
        let plan = tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&plan_bytes)
            .map_err(|e| format!("plan decode: {e}"))?;
        let format = PlanFormat::parse(q.format.as_deref().unwrap_or("text"))
            .ok_or_else(|| format!("invalid format '{}'", q.format.as_deref().unwrap_or("text")))?;

        let mut options = RenderOptions {
            view: q.view.as_deref().and_then(ViewLevel::parse).unwrap_or(ViewLevel::Full),
            direction: q
                .direction
                .as_deref()
                .and_then(Direction::parse)
                .unwrap_or(Direction::Td),
            numbered_groups: true,
        };
        if let Some(pname) = &q.profile {
            let profile = tinypipe_insight::profile::resolve(&state2.storage, pname)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("profile '{pname}' not found"))?;
            let p_opts = tinypipe_insight::profile::render_options(&profile);
            if q.view.is_none() {
                options.view = p_opts.view;
            }
            if q.direction.is_none() {
                options.direction = p_opts.direction;
            }
        }
        let header = PlanDumpHeader {
            graph_name: &graph2.name,
            graph_version: q.version.unwrap_or(graph2.version.0),
            encoded_len: plan_bytes.len(),
        };
        Ok(format.render(&plan, &header, options))
    }).await?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response())
}

pub async fn report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ReportQuery>,
) -> ApiResult<Response> {
    require_token(&state, &headers)?;
    let state2 = state.clone();
    let body = run_async(move || {
        tinypipe_insight::profile::seed_builtin_profiles(&state2.storage).map_err(|e| e.to_string())?;
        let profile = match &q.profile {
            Some(name) => tinypipe_insight::profile::resolve(&state2.storage, name)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("profile '{name}' not found"))?,
            None => tinypipe_insight::profile::builtin_profile("senior")
                .expect("builtin 'senior' must exist"),
        };
        let mut env_map = HashMap::new();
        for kv in q.env.unwrap_or_default() {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("invalid env '{kv}', expected KEY=VALUE"))?;
            env_map.insert(k.to_string(), v.to_string());
        }
        let env = build_env(&env_map);
        let metrics = tinypipe_insight::metrics::collect(&state2.storage, Some(&env))
            .map_err(|e| e.to_string())?;
        Ok(tinypipe_insight::report::render(&profile, &metrics))
    }).await?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response())
}

pub async fn profiles_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let profiles = state
        .storage
        .list_profiles()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::to_value(&profiles).map_err(|e| ApiError::Internal(e.to_string()))?))
}

pub async fn profiles_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<ProfileReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    if let Some(v) = &req.view {
        if ViewLevel::parse(v).is_none() {
            return Err(ApiError::BadRequest(format!("invalid view '{v}'")));
        }
    }
    if let Some(d) = &req.direction {
        if Direction::parse(d).is_none() {
            return Err(ApiError::BadRequest(format!("invalid direction '{d}'")));
        }
    }
    let existing = state.storage.load_profile(&name).ok();
    if let Some(p) = &existing {
        if p.builtin {
            return Err(ApiError::Conflict(format!("profile '{name}' is built-in")));
        }
    }
    let profile = tinypipe_api::types::Profile {
        name: name.clone(),
        label: req.label.unwrap_or_else(|| name.clone()),
        description: req.description.unwrap_or_default(),
        view: req.view.unwrap_or_else(|| "full".into()),
        direction: req.direction.unwrap_or_else(|| "td".into()),
        focus: req.focus.unwrap_or_default(),
        config: req.config.unwrap_or_else(|| serde_json::json!({})),
        builtin: false,
    };
    state
        .storage
        .save_profile(&profile)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

pub async fn profiles_show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let profile = tinypipe_insight::profile::resolve(&state.storage, &name)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("profile '{name}' not found")))?;
    Ok(Json(serde_json::to_value(&profile).map_err(|e| ApiError::Internal(e.to_string()))?))
}

pub async fn profiles_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    state
        .storage
        .delete_profile(&name)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

pub async fn tools_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ToolsTestReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_token(&state, &headers)?;
    let state2 = state.clone();
    let body = run_async(move || {
        let env = build_env(req.env.as_ref().unwrap_or(&HashMap::new()));

        let mut call = CallTarget::new(&req.name);
        if let Some(args) = &req.args {
            call.args = args.iter().map(|v| tinypipe_tools::daemon::json_to_tp(v.clone())).collect();
        }
        if let Some(kwargs) = &req.kwargs {
            if let Some(map) = kwargs.as_object() {
                for (k, v) in map {
                    call.kwargs
                        .insert(k.clone(), tinypipe_tools::daemon::json_to_tp(v.clone()));
                }
            } else {
                return Ok(serde_json::json!({
                    "success": false,
                    "error": "kwargs must be a JSON object",
                }));
            }
        }

        if let Ok(spec) = state2.registry.resolve(&req.name, "0") {
            let result = state2.registry.dispatch(&call, &tinypipe_api::types::Context::new(), &env);
            return Ok(match result {
                Ok(value) => serde_json::json!({
                    "success": true,
                    "tool": req.name,
                    "version": spec.version,
                    "output": tp_to_json(&value),
                }),
                Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
            });
        }

        let addr = daemon_addr_from_env();
        let args_json = req
            .args
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| e.to_string())?;
        let kwargs_json = req
            .kwargs
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| e.to_string())?;
        match invoke_daemon_tool(
            &addr,
            &req.name,
            args_json.as_deref().unwrap_or("[]"),
            kwargs_json.as_deref().unwrap_or("{}"),
            req.env.clone().unwrap_or_default(),
        ) {
            Ok(output) => {
                let value: serde_json::Value = if output.success {
                    serde_json::from_str(&output.output_json).unwrap_or(serde_json::Value::Null)
                } else {
                    return Ok(serde_json::json!({
                        "success": false,
                        "daemon": addr,
                        "error": output.error_message,
                    }));
                };
                Ok(serde_json::json!({
                    "success": true,
                    "daemon": addr,
                    "duration_us": output.duration_us,
                    "output": value,
                }))
            }
            Err(e) => Ok(serde_json::json!({ "success": false, "daemon": addr, "error": e })),
        }
    }).await?;
    Ok(Json(body))
}

/// `POST /api/run` — dinamik kod çalıştır (token zorunlu, audit yok, env check yok).
pub async fn run_dynamic(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RunReq>,
) -> ApiResult<Response> {
    require_token(&state, &headers)?;
    let state2 = state.clone();
    let code2 = req.code.clone();
    let inputs2 = req.inputs.unwrap_or_else(|| serde_json::json!({}));
    let env2 = req.env.unwrap_or_default();
    let pause2 = req.pause_after;
    let outcome = run_async(move || {
        let key = AppState::fnv1a(code2.as_bytes());
        let plan = {
            let dyn_plans = state2.dynamic_plans.blocking_read();
            if let Some((_ts, cached_code, plan)) = dyn_plans.get(&key) {
                if cached_code == &code2 {
                    plan.clone()
                } else {
                    drop(dyn_plans);
                    let compiled = compile(&code2).map_err(|e| format!("compile failed: {e}"))?;
                    let arc = Arc::new(compiled.compiled);
                    state2.cache_dynamic_plan(key, code2.clone(), arc.clone());
                    arc
                }
            } else {
                drop(dyn_plans);
                let compiled = compile(&code2).map_err(|e| format!("compile failed: {e}"))?;
                let arc = Arc::new(compiled.compiled);
                state2.cache_dynamic_plan(key, code2.clone(), arc.clone());
                arc
            }
        };
        run_execution(&state2, None, (*plan).clone(), &inputs2, &env2, pause2, true, None, false)
    }).await?;
    classify_execution(outcome)
}

// ==================== Shared ====================

fn classify_execution(outcome: serde_json::Value) -> ApiResult<Response> {
    let status = outcome.get("status").and_then(|s| s.as_str()).unwrap_or("failed");
    match status {
        "completed" => Ok((StatusCode::OK, Json(outcome)).into_response()),
        "paused" => Ok((StatusCode::ACCEPTED, Json(outcome)).into_response()),
        _ => Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(outcome)).into_response()),
    }
}
