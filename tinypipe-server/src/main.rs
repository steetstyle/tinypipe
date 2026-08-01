use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::Router;
use tinypipe_storage::SqliteStorage;
use tinypipe_tools::{SubgraphToolRegistry, default_tools};

use tinypipe_server::api;
use tinypipe_server::engine::refresh_all;
use tinypipe_server::state::AppState;

fn env_bool(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

fn api_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/check", post(api::check_code))
        .route("/run", post(api::run_dynamic))
        .route("/graphs", get(api::list_graphs).post(api::create_graph))
        .route("/graphs/{id}", put(api::update_graph))
        .route("/graphs/{id}/versions", get(api::list_versions))
        .route("/graphs/{id}/deploy", post(api::deploy_graph))
        .route("/graphs/{id}/rollback", post(api::rollback_graph))
        .route("/graphs/{id}/plan", get(api::plan_dump))
        .route("/graphs/{id}/execute", post(api::execute_graph))
        .route("/executions", get(api::list_executions))
        .route("/executions/{id}", get(api::show_execution))
        .route("/executions/{id}/resume", post(api::resume))
        .route("/scheduler/run", post(api::scheduler_run))
        .route("/report", get(api::report))
        .route("/profiles", get(api::profiles_list))
        .route("/profiles/{name}", post(api::profiles_create))
        .route("/profiles/{name}", get(api::profiles_show))
        .route("/profiles/{name}", delete(api::profiles_delete))
        .route("/tools", get(api::tools_list))
        .route("/tools/test", post(api::tools_test))
        .route("/daemon/status", get(api::daemon_status))
}

#[tokio::main]
async fn main() {
    init_tracing();

    let addr: SocketAddr = std::env::var("TINYPIPE_SERVER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("invalid TINYPIPE_SERVER_ADDR: {e}");
            std::process::exit(1);
        });
    let token = std::env::var("TINYPIPE_SERVER_TOKEN").ok().filter(|t| !t.is_empty());
    let audit = env_bool("TINYPIPE_SERVER_AUDIT");
    let db_path = std::env::var("TINYPIPE_DB").unwrap_or_else(|_| "./tinypipe.db".into());

    if token.is_none() {
        tracing::warn!(
            "TINYPIPE_SERVER_TOKEN is not set — mutating endpoints and /api/run will return 401"
        );
    }
    if audit {
        tracing::warn!("TINYPIPE_SERVER_AUDIT is on — every execution writes to the database");
    }

    let storage = match SqliteStorage::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("failed to open database '{db_path}': {e}");
            std::process::exit(1);
        }
    };
    let registry = Arc::new(SubgraphToolRegistry::with_tools(storage.clone(), default_tools())).init();
    let state = Arc::new(AppState::new(storage, registry, token, audit));

    match refresh_all(&state).await {
        Ok((plans, routes)) => {
            tracing::info!("loaded {plans} cached plans, {routes} published routes");
        }
        Err(e) => {
            eprintln!("startup route registration failed (invalid http_* META):\n{e}");
            std::process::exit(1);
        }
    }

    let app = Router::new()
        .route("/healthz", get(api::healthz))
        .nest("/api", api_routes())
        .fallback(tinypipe_server::routes::publish)
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        "tinypipe-server listening on http://{addr} (audit={}, published routes at site root)",
        if audit { "on" } else { "off" }
    );
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
