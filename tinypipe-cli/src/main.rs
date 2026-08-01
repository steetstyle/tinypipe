//! `tinypipe-cli` — CLI for the tinypipe execution graph platform.
//!
//! Usage:
//!   tinypipe-cli create <name> <code>         Create a new graph from code
//!   tinypipe-cli create --from-llm <name> <description>
//!                                            Create a graph from natural language
//!   tinypipe-cli update <id> <code>           Update a graph (new version)
//!   tinypipe-cli execute <id> [json_input] [--pause-after N]
//!                                            Execute a graph (optionally paused)
//!   tinypipe-cli resume <execution_id> [--max-nodes N]
//!                                            Resume a paused execution
//!   tinypipe-cli scheduler run [--max-nodes N]
//!                                            Resume all paused executions
//!   tinypipe-cli plan <id> [version] [--format text|mermaid|dot]
//!                                            Dump the compiled plan
//!   tinypipe-cli list                         List graphs
//!   tinypipe-cli check <code>                 Check code for errors (auto-repair format)
//!
//! LLM environment variables:
//!   OPENAI_API_KEY     — uses OpenAI (model: gpt-4o-mini)
//!   ANTHROPIC_API_KEY  — uses Anthropic (model: claude-sonnet-4-20250514)
//!   (default)          — uses Ollama at http://localhost:11434 (model: llama3.2)
//!
//! Examples:
//!   tinypipe-cli create "hello" "def graph(x: int):\n    return x"
//!   tinypipe-cli create --from-llm "hello" "return the input value as-is"
//!   tinypipe-cli execute <id> '{"x": 42}'
//!   tinypipe-cli check "def graph(x: int):\n    return x"

use std::collections::HashMap;

use tinypipe_api::storage::GraphStorage;
use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{CallTarget, Context, Value};
use tinypipe_api::types::{Execution, ExecutionStatus, ExecutionStep};
use tinypipe_api::types::{GraphId, Version};
use tinypipe_compiler::{auto_repair, compile};
use tinypipe_storage::SqliteStorage;
use tinypipe_tools::daemon::{daemon_addr_from_env, list_daemon_tools, register_daemon_tools};
use tinypipe_tools::{default_tools, SubgraphToolRegistry};
use tinypipe_vm::CompiledExecutor;

// LLM integration (requires `llm` feature)
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::provider::{OllamaConfig, Provider};
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::LlmContext;

/// Logging: `RUST_LOG` değerine göre seviye seçer (varsayılan: info).
/// Örn: `RUST_LOG=trace tinypipe-cli execute ...` ile VM node-trace'leri görünür.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() {
    init_tracing();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: tinypipe-cli <command> [args...]");
        eprintln!();
        eprintln!("Commands:");
        eprintln!("  create <name> <code>              Create a new graph from code");
        eprintln!("  create --from-llm <name> <desc>    Create a graph from natural language");
        eprintln!("  deploy <id> [version]              Deploy a graph version (default: current)");
        eprintln!("  rollback <id> <version>            Rollback to a previous version");
        eprintln!("  versions <id>                      List all versions of a graph");
        eprintln!("  executions list <id>               List executions of a graph");
        eprintln!("  executions show <execution_id>     Show execution details + steps");
        eprintln!("  update <id> <code>                Update a graph (new version)");
        eprintln!("  execute <id> [json_input] [--pause-after N]   Execute a graph");
        eprintln!("  resume <execution_id> [--max-nodes N]          Resume a paused execution");
        eprintln!("  scheduler run [--max-nodes N]                  Resume all paused executions");
        eprintln!("  plan <id> [version] [--format text|mermaid|dot]");
        eprintln!("        [--view full|summary|layers] [--direction td|lr] [--profile <name>]");
        eprintln!("                                   Dump the compiled plan (role profiles apply)");
        eprintln!("  report [--profile <name>] [--env KEY=V] [--env-file <path>]");
        eprintln!("                                   Role-based portfolio report");
        eprintln!("  profiles list                    List all profiles (builtin + custom)");
        eprintln!("  profiles show <name>             Show a profile");
        eprintln!("  profiles create <name> [--label L] [--description D] [--view v] [--direction d] [--focus a,b] [--config <json>]");
        eprintln!("                                   Create a custom profile");
        eprintln!("  profiles delete <name>           Delete a custom profile (builtin protected)");
        eprintln!("  tools list                       List built-in + daemon tools");
        eprintln!("  tools test <name> '<json args>' [--env KEY=V]");
        eprintln!("                                   Test a tool with JSON args");
        eprintln!("  daemon status [addr]             Check the tool daemon (default: TINYPIPE_DAEMON_ADDR)");
        eprintln!("  list                              List graphs");
        eprintln!("  check <code>                      Check code for errors");
        eprintln!();
        eprintln!("LLM environment variables:");
        eprintln!("  OPENAI_API_KEY     — uses OpenAI (gpt-4o-mini)");
        eprintln!("  ANTHROPIC_API_KEY  — uses Anthropic (claude-sonnet-4-20250514)");
        eprintln!("  (default)          — Ollama at http://localhost:11434 (llama3.2)");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  tinypipe-cli create hello \"def graph(x: int):\\n    return x\"");
        eprintln!("  tinypipe-cli create --from-llm hello \"return the input\"");
        eprintln!("  tinypipe-cli execute <id> '{{\"x\": 42}}'");
        std::process::exit(1);
    }

    let command = &args[1];
    match command.as_str() {
        "create" => {
            if args.len() >= 3 && args[2] == "--from-llm" {
                if args.len() < 5 {
                    eprintln!("Usage: tinypipe-cli create --from-llm <name> <description>");
                    std::process::exit(1);
                }
                cmd_create_from_llm(&args[3], &args[4]);
            } else {
                if args.len() < 4 {
                    eprintln!("Usage: tinypipe-cli create <name> <code>");
                    std::process::exit(1);
                }
                cmd_create(&args[2], &unescape_code(&args[3]));
            }
        }
        "update" => {
            if args.len() < 4 {
                eprintln!("Usage: tinypipe-cli update <id> <code>");
                std::process::exit(1);
            }
            cmd_update(&args[2], &unescape_code(&args[3]));
        }
        "execute" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli execute <id> [json_input] [--pause-after N] [--no-env-check]");
                std::process::exit(1);
            }
            let input_json = args
                .get(3)
                .filter(|s| !s.starts_with("--"))
                .map(|s| s.as_str())
                .unwrap_or("{}");
            let mut pause_after = None;
            if let Some(pos) = args.iter().position(|a| a == "--pause-after") {
                pause_after = args.get(pos + 1).and_then(|v| v.parse::<u32>().ok());
                if pause_after.is_none() {
                    eprintln!("Error: --pause-after requires a number");
                    std::process::exit(1);
                }
            }
            let skip_env_check = args.iter().any(|a| a == "--no-env-check");
            let env = parse_env_args(&args[3..]);
            cmd_execute(&args[2], input_json, pause_after, env, skip_env_check);
        }
        "resume" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli resume <execution_id> [--max-nodes N] [--no-env-check]");
                std::process::exit(1);
            }
            let mut max_nodes = None;
            if let Some(pos) = args.iter().position(|a| a == "--max-nodes") {
                max_nodes = args.get(pos + 1).and_then(|v| v.parse::<u32>().ok());
                if max_nodes.is_none() {
                    eprintln!("Error: --max-nodes requires a number");
                    std::process::exit(1);
                }
            }
            let skip_env_check = args.iter().any(|a| a == "--no-env-check");
            let env = parse_env_args(&args[3..]);
            cmd_resume(&args[2], max_nodes, env, skip_env_check);
        }
        "scheduler" => {
            if args.len() < 3 || args[2] != "run" {
                eprintln!("Usage: tinypipe-cli scheduler run [--max-nodes N]");
                std::process::exit(1);
            }
            let mut max_nodes = None;
            if let Some(pos) = args.iter().position(|a| a == "--max-nodes") {
                max_nodes = args.get(pos + 1).and_then(|v| v.parse::<u32>().ok());
                if max_nodes.is_none() {
                    eprintln!("Error: --max-nodes requires a number");
                    std::process::exit(1);
                }
            }
            let env = parse_env_args(&args[3..]);
            cmd_scheduler_run(max_nodes, env);
        }
        "list" => {
            cmd_list();
        }
        "deploy" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli deploy <id> [version]");
                std::process::exit(1);
            }
            let version = args.get(3).and_then(|v| v.parse::<u64>().ok()).map(Version);
            cmd_deploy(&args[2], version);
        }
        "rollback" => {
            if args.len() < 4 {
                eprintln!("Usage: tinypipe-cli rollback <id> <version>");
                std::process::exit(1);
            }
            let version = match args[3].parse::<u64>() {
                Ok(v) => Version(v),
                Err(_) => {
                    eprintln!("Error: version must be a number, got '{}'", args[3]);
                    std::process::exit(1);
                }
            };
            cmd_rollback(&args[2], version);
        }
        "versions" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli versions <id>");
                std::process::exit(1);
            }
            cmd_versions(&args[2]);
        }
        "executions" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli executions list <id> | show <execution_id>");
                std::process::exit(1);
            }
            match args[2].as_str() {
                "list" => {
                    if args.len() < 4 {
                        eprintln!("Usage: tinypipe-cli executions list <id>");
                        std::process::exit(1);
                    }
                    cmd_executions_list(&args[3]);
                }
                "show" => {
                    if args.len() < 4 {
                        eprintln!("Usage: tinypipe-cli executions show <execution_id>");
                        std::process::exit(1);
                    }
                    cmd_executions_show(&args[3]);
                }
                other => {
                    eprintln!("Unknown executions subcommand: {other}");
                    eprintln!("Commands: list, show");
                    std::process::exit(1);
                }
            }
        }
        "plan" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli plan <id> [version] [--format text|mermaid|dot] [--view full|summary|layers] [--direction td|lr] [--profile <name>]");
                std::process::exit(1);
            }
            let version = args
                .get(3)
                .filter(|s| !s.starts_with("--"))
                .and_then(|v| parse_version_arg(v))
                .map(Version);
            let format = args
                .iter()
                .position(|a| a == "--format")
                .and_then(|pos| args.get(pos + 1))
                .and_then(|s| tinypipe_ir::PlanFormat::parse(s))
                .unwrap_or(tinypipe_ir::PlanFormat::Text);
            let view = args
                .iter()
                .position(|a| a == "--view")
                .and_then(|pos| args.get(pos + 1))
                .and_then(|s| tinypipe_ir::plan_view::ViewLevel::parse(s))
                .unwrap_or(tinypipe_ir::plan_view::ViewLevel::Full);
            let direction = args
                .iter()
                .position(|a| a == "--direction")
                .and_then(|pos| args.get(pos + 1))
                .and_then(|s| tinypipe_ir::plan_view::Direction::parse(s))
                .unwrap_or(tinypipe_ir::plan_view::Direction::Td);
            let mut options = tinypipe_ir::plan_view::RenderOptions {
                view,
                direction,
                numbered_groups: true,
            };
            // Rol profili: explicit --view/--direction flag'leri kazanır.
            if let Some(pos) = args.iter().position(|a| a == "--profile") {
                let profile_name = match args.get(pos + 1) {
                    Some(n) => n,
                    None => {
                        eprintln!("Error: --profile requires a name");
                        std::process::exit(1);
                    }
                };
                let storage = open_storage();
                match tinypipe_insight::profile::resolve(&storage, profile_name) {
                    Ok(Some(profile)) => {
                        if args.iter().position(|a| a == "--view").is_none() {
                            options.view = tinypipe_ir::plan_view::ViewLevel::parse(&profile.view)
                                .unwrap_or(tinypipe_ir::plan_view::ViewLevel::Full);
                        }
                        if args.iter().position(|a| a == "--direction").is_none() {
                            options.direction = tinypipe_ir::plan_view::Direction::parse(
                                &profile.direction,
                            )
                            .unwrap_or(tinypipe_ir::plan_view::Direction::Td);
                        }
                    }
                    Ok(None) => {
                        eprintln!("Error: unknown profile '{profile_name}'");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Error loading profile '{profile_name}': {e}");
                        std::process::exit(1);
                    }
                }
            }
            cmd_plan_dump(&args[2], version, format, options);
        }
        "report" => {
            let profile_name = args
                .iter()
                .position(|a| a == "--profile")
                .and_then(|pos| args.get(pos + 1).cloned());
            cmd_report(profile_name.as_deref(), &args[2..]);
        }
        "profiles" => {
            cmd_profiles(&args[2..]);
        }
        "check" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli check <code>");
                std::process::exit(1);
            }
            cmd_check(&unescape_code(&args[2]));
        }
        "tools" => {
            cmd_tools(&args[2..]);
        }
        "daemon" => {
            cmd_daemon(&args[2..]);
        }
        _ => {
            eprintln!("Unknown command: {command}");
            eprintln!("Commands: create, deploy, rollback, versions, update, execute, resume, scheduler, list, plan, report, profiles, tools, daemon, check");
            std::process::exit(1);
        }
    }
}

// ─── Commands ──────────────────────────────────────────────────────

/// `tinypipe-cli create <name> <code>` — compile code and save as a new graph.
fn cmd_create(name: &str, code: &str) {
    // 1. Compile (transform + validate + optimize + codegen)
    let output = match compile(code) {
        Ok(output) => output,
        Err(msg) => {
            eprintln!("Compilation failed:");
            eprintln!("  {}", msg);
            if let Some(report) = auto_repair::check_code(code, 1, 3) {
                eprintln!();
                eprintln!("{}", report);
            }
            std::process::exit(1);
        }
    };

    // 2. Save
    let storage = open_storage();
    let graph_id = storage
        .create_graph(name, code)
        .expect("Failed to save graph");
    storage
        .save_plan(&graph_id, Version(1), &output.fb_binary)
        .expect("Failed to save compiled plan");

    println!("✓ Graph created: {}", graph_id.0);
    println!("  Name: {}", name);
    println!("  Nodes: {}", output.compiled.metadata.node_count);
    println!("  Edges: {}", output.compiled.metadata.edge_count);
    println!("  Binary (FlatBuffers): {} bytes", output.fb_binary.len());
    println!("  Optimizations: {:?}", output.optimizations);

    println!();
    println!("━━━ Compiler Feedback ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  ✅ Başarılı: Graph \"{}\" valide edildi.", name);
    println!(
        "  Node sayısı: {}, Edge sayısı: {}",
        output.compiled.metadata.node_count, output.compiled.metadata.edge_count
    );
    if !output.optimizations.is_empty() {
        println!("  Optimizasyonlar: {}", output.optimizations.join(", "));
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

/// `tinypipe-cli create --from-llm <name> <description>` — generate code
/// via LLM, then compile and save as a new graph.
#[cfg(feature = "llm")]
fn cmd_create_from_llm(name: &str, description: &str) {
    // 1. Detect provider from environment and call LLM to generate code
    let backend = detect_llm_provider();
    let context = LlmContext::default();

    eprintln!("  🤖 Generating code from: \"{}\"", description);
    eprintln!("  Provider: {}", backend.name());
    eprintln!();

    let llm_start = std::time::Instant::now();
    let raw_response = match backend.generate_code(description, &context) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("✗ LLM error: {e}");
            eprintln!("  Try providing the code directly with:");
            eprintln!("    tinypipe-cli create {} <code>", name);
            std::process::exit(1);
        }
    };
    let llm_elapsed = llm_start.elapsed();
    eprintln!("  ⏱  LLM responded in {:?}", llm_elapsed);

    // 2. Extract Python code from LLM response
    let code = match tinypipe_compiler::llm::extract_code_from_response(&raw_response) {
        Some(extracted) => extracted,
        None => {
            eprintln!("✗ LLM response contained no valid Python code");
            eprintln!("  Raw response:");
            for line in raw_response.lines() {
                eprintln!("    {}", line);
            }
            std::process::exit(1);
        }
    };

    // 3. Show generated code
    println!();
    println!("━━━ Generated Code ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    for line in code.lines() {
        println!("  {}", line);
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    // 4. Compile and save (reuse cmd_create)
    cmd_create(name, &code);
}

/// Stub for when the `llm` feature is disabled.
#[cfg(not(feature = "llm"))]
fn cmd_create_from_llm(_name: &str, _description: &str) {
    eprintln!("✗ LLM integration not available.");
    eprintln!("  Rebuild with the `llm` feature enabled:");
    eprintln!("    cargo build --features tinypipe-compiler/llm");
    eprintln!("  Or provide the code directly:");
    eprintln!("    tinypipe-cli create {} <code>", _name);
    std::process::exit(1);
}

/// Detect the LLM provider from environment variables.
///
/// Order of precedence:
/// 1. `OPENAI_API_KEY` → OpenAI (model: gpt-4o-mini)
/// 2. `ANTHROPIC_API_KEY` → Anthropic (model: claude-sonnet-4-20250514)
/// 3. (default) → Ollama at http://localhost:11434 (model: llama3.2)
#[cfg(feature = "llm")]
fn detect_llm_provider() -> Box<dyn tinypipe_compiler::llm::LlmBackend> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            eprintln!("  🔑 Using OpenAI (gpt-4o-mini)");
            return Provider::OpenAI {
                api_key: key,
                model: "gpt-4o-mini".into(),
                base_url: None,
            }
            .into_backend();
        }
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            eprintln!("  🔑 Using Anthropic (claude-sonnet-4-20250514)");
            return Provider::Anthropic {
                api_key: key,
                model: "claude-sonnet-4-20250514".into(),
            }
            .into_backend();
        }
    }

    // Default: Ollama (local)
    eprintln!("  🦙 Using local Ollama (llama3.2 at http://localhost:11434)");
    eprintln!("  (Set OPENAI_API_KEY or ANTHROPIC_API_KEY env var to use cloud)");
    Provider::Ollama {
        config: OllamaConfig {
            base_url: std::env::var("OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434".into()),
            model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".into()),
            timeout: std::time::Duration::from_secs(120),
            keep_alive: std::time::Duration::from_secs(300),
            temperature: 0.1,
        },
    }
    .into_backend()
}

/// Resolve a user-provided identifier to a GraphId.
/// If the input is a valid UUID, use it directly.
/// Otherwise, look up by name in the storage.
fn resolve_graph_id(storage: &SqliteStorage, input: &str) -> GraphId {
    // Try as UUID first
    if let Ok(items) = storage.list_all_graphs(None, None) {
        // Check if input matches a UUID directly
        let by_uuid = items.iter().find(|g| g.id == input);
        if let Some(item) = by_uuid {
            return GraphId::new(&item.id);
        }
        // Check by name
        let by_name = items.iter().find(|g| g.name == input);
        if let Some(item) = by_name {
            return GraphId::new(&item.id);
        }
    }
    // Fallback: treat as UUID anyway (will get a proper error from storage)
    GraphId::new(input)
}

/// Plan'ı DB'den yükler. Eski sürümlerde oluşturulan graph'larda plan
/// sütunu NULL kalabilir (save_plan yokken) — bu durumda kodu yeniden
/// derleyip kalıcı olarak kaydeder (self-heal).
fn load_plan_self_heal(storage: &SqliteStorage, graph_id: &GraphId) -> Vec<u8> {
    match storage.load_plan(graph_id) {
        Ok(bytes) => bytes,
        Err(tinypipe_api::types::StorageError::PlanMissing(_)) => {
            let graph = match storage.load_graph(graph_id) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("Error loading graph: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!(
                "⚠ No stored plan for '{}' (v{}) — recompiling and persisting...",
                graph.name, graph.version.0
            );
            let output = match compile(&graph.code) {
                Ok(o) => o,
                Err(msg) => {
                    eprintln!("Recompilation failed: {msg}");
                    std::process::exit(1);
                }
            };
            storage
                .save_plan(graph_id, graph.version, &output.fb_binary)
                .unwrap_or_else(|e| {
                    eprintln!("Failed to persist repaired plan: {e}");
                    std::process::exit(1);
                });
            eprintln!(
                "✓ Plan repaired (v{}, {} nodes)",
                graph.version.0, output.compiled.metadata.node_count
            );
            output.fb_binary
        }
        Err(e) => {
            eprintln!("Error loading plan: {e}");
            std::process::exit(1);
        }
    }
}

/// Belirli bir version'ın planını yükler; eksikse o version'ın kodunu
/// derleyip kaydeder (self-heal).
fn load_plan_version_self_heal(
    storage: &SqliteStorage,
    graph_id: &GraphId,
    version: Version,
) -> Vec<u8> {
    use tinypipe_api::types::StorageError;
    match storage.load_plan_version(graph_id, version) {
        Ok(bytes) => bytes,
        Err(StorageError::PlanVersionMissing(_, _)) => {
            let code = storage
                .list_versions(graph_id)
                .unwrap_or_else(|e| {
                    eprintln!("Error listing versions: {e}");
                    std::process::exit(1);
                })
                .into_iter()
                .find(|(v, _, _)| *v == version.0)
                .map(|(_, code, _)| code)
                .unwrap_or_else(|| {
                    eprintln!("Version v{} not found for graph", version.0);
                    std::process::exit(1);
                });
            eprintln!(
                "⚠ No stored plan for v{} — recompiling and persisting...",
                version.0
            );
            let output = match compile(&code) {
                Ok(o) => o,
                Err(msg) => {
                    eprintln!("Recompilation failed: {msg}");
                    std::process::exit(1);
                }
            };
            storage
                .save_plan(graph_id, version, &output.fb_binary)
                .unwrap_or_else(|e| {
                    eprintln!("Failed to persist repaired plan: {e}");
                    std::process::exit(1);
                });
            eprintln!(
                "✓ Plan repaired (v{}, {} nodes)",
                version.0, output.compiled.metadata.node_count
            );
            output.fb_binary
        }
        Err(e) => {
            eprintln!("Error loading plan: {e}");
            std::process::exit(1);
        }
    }
}

/// "4" veya "v4" formatında version argümanını parse eder.
fn parse_version_arg(s: &str) -> Option<u64> {
    s.strip_prefix('v').unwrap_or(s).parse::<u64>().ok()
}

/// `tinypipe-cli update <id> <code>` — compile new code and save as a new version.
fn cmd_update(id: &str, code: &str) {
    let output = match compile(code) {
        Ok(output) => output,
        Err(msg) => {
            eprintln!("Compilation failed:");
            eprintln!("  {}", msg);
            std::process::exit(1);
        }
    };

    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);
    match storage.update_graph(&graph_id, code) {
        Ok(version) => {
            storage
                .save_plan(&graph_id, version, &output.fb_binary)
                .expect("Failed to save compiled plan");
            println!("✓ Graph updated: {} (version {})", id, version.0);
            println!(
                "  Nodes: {}, Edges: {}, Binary (FlatBuffers): {} bytes",
                output.compiled.metadata.node_count,
                output.compiled.metadata.edge_count,
                output.fb_binary.len()
            );
        }
        Err(e) => {
            eprintln!("Error updating graph: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli execute <id> [json_input] [--pause-after N]` — load and execute a graph.
/// `--pause-after N` verilirse N node sonra durur ve checkpoint kaydeder (resume edilebilir).

/// `--env K=V` ve `--env-file <path>` flag'lerini toplar.
/// Öncelik (ilk kazanan): CLI override'ları → dosya → OS env.
fn parse_env_args(args: &[String]) -> tinypipe_env::Env {
    let mut overrides: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut file = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--env" {
            if let Some(pair) = args.get(i + 1) {
                if let Some((k, v)) = pair.split_once('=') {
                    overrides.insert(k.to_string(), v.to_string());
                    i += 2;
                    continue;
                }
            }
            eprintln!("Error: --env requires KEY=VALUE");
            std::process::exit(1);
        }
        if args[i] == "--env-file" {
            if let Some(path) = args.get(i + 1) {
                file = Some(path.clone());
                i += 2;
                continue;
            }
            eprintln!("Error: --env-file requires a path");
            std::process::exit(1);
        }
        i += 1;
    }
    let mut providers: Vec<std::sync::Arc<dyn tinypipe_env::EnvProvider>> = Vec::new();
    if !overrides.is_empty() {
        providers.push(std::sync::Arc::new(tinypipe_env::static_provider::StaticEnvProvider::new(overrides)));
    }
    if let Some(path) = file {
        providers.push(std::sync::Arc::new(tinypipe_env::dotenv::DotEnvFileProvider::new(path)));
    }
    providers.push(std::sync::Arc::new(tinypipe_env::os::OsEnvProvider));
    tinypipe_env::Env::new(providers)
}

/// Registry kurar: built-in tool'lar + daemon'dan remote tool'lar (çakışan
/// isimlerde built-in kazanır). Daemon yoksa sessizce built-in'lerle devam eder;
/// `TINYPIPE_NO_DAEMON=1` ile daemon bağlantısı tamamen atlanır.
fn build_registry(storage: SqliteStorage) -> std::sync::Arc<SubgraphToolRegistry<SqliteStorage>> {
    let tools = default_tools();
    if std::env::var("TINYPIPE_NO_DAEMON").is_err() {
        let addr = daemon_addr_from_env();
        match register_daemon_tools(&tools, &addr) {
            Ok(n) if n > 0 => {
                eprintln!("  daemon tools registered: {n} (via {addr})");
            }
            _ => {} // daemon kapalı — sadece built-in'ler
        }
    }
    std::sync::Arc::new(SubgraphToolRegistry::with_tools(storage, tools)).init()
}

/// `tinypipe-cli tools list|test ...`
fn cmd_tools(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("list") => cmd_tools_list(),
        Some("test") => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli tools test <name> '<json args>' [--env KEY=V]");
                std::process::exit(1);
            }
            cmd_tools_test(&args[1], &args[2], &args[3..]);
        }
        Some(other) => {
            eprintln!("Unknown tools subcommand: {other}");
            eprintln!("Commands: list, test");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: tinypipe-cli tools list | test <name> '<json args>'");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli tools list` — built-in + daemon tool'larını listeler.
fn cmd_tools_list() {
    let reg = default_tools();
    let builtin = reg.tool_names();
    println!("Built-in tools ({}):", builtin.len());
    for name in &builtin {
        println!("  {name}");
    }

    let addr = daemon_addr_from_env();
    match list_daemon_tools(&addr) {
        Ok(tools) if !tools.is_empty() => {
            println!();
            println!("Daemon tools ({} via {addr}):", tools.len());
            for t in &tools {
                let timeout = if t.timeout_ms > 0 {
                    format!(" [timeout {}ms]", t.timeout_ms)
                } else {
                    String::new()
                };
                println!("  {} — {}{timeout}", t.name, t.description);
            }
        }
        Ok(_) => {
            println!();
            println!("Daemon: OK ({addr}) but no tools registered — bir worker bağlayın.");
        }
        Err(e) => {
            println!();
            println!("Daemon tools: unreachable ({addr})");
            println!("  {e}");
            println!("  İpucu: `tinypipe-daemon` binary'sini çalıştırın ve worker'ları bağlayın.");
        }
    }
}

/// `tinypipe-cli tools test <name> '<json args>'` — tool'u doğrudan çalıştırır.
fn cmd_tools_test(tool: &str, args_json: &str, rest: &[String]) {
    let (kwargs_json, env_args): (Vec<&String>, Vec<String>) = {
        let mut kwargs = Vec::new();
        let mut env = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            if rest[i] == "--kwargs" && rest.get(i + 1).is_some() {
                kwargs.push(&rest[i + 1]);
                i += 2;
                continue;
            }
            env.push(rest[i].clone());
            i += 1;
        }
        (kwargs, env)
    };
    let env = parse_env_args(&env_args);
    let reg = default_tools();
    if std::env::var("TINYPIPE_NO_DAEMON").is_err() {
        let addr = daemon_addr_from_env();
        match register_daemon_tools(&reg, &addr) {
            Ok(n) if n > 0 => eprintln!("daemon tools registered: {n} (via {addr})"),
            _ => {}
        }
    }
    let args: Vec<Value> = match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(serde_json::Value::Array(arr)) => arr.into_iter().map(json_val_to_tp).collect(),
        Ok(v) => vec![json_val_to_tp(v)],
        Err(e) => {
            eprintln!("Error: invalid JSON args: {e}");
            std::process::exit(1);
        }
    };
    let kwargs = kwargs_json
        .first()
        .map(|json| serde_json::from_str::<serde_json::Value>(json))
        .transpose()
        .unwrap_or_else(|e| {
            eprintln!("Error: invalid JSON kwargs: {e}");
            std::process::exit(1);
        })
        .map(json_val_to_tp)
        .unwrap_or_else(|| Value::Object(Default::default()));
    let kwargs = match kwargs {
        Value::Object(map) => map,
        _ => {
            eprintln!("Error: --kwargs must be a JSON object");
            std::process::exit(1);
        }
    };
    let mut ct = CallTarget::new(tool);
    ct.args = args;
    ct.kwargs = kwargs;
    let start = std::time::Instant::now();
    match reg.dispatch(&ct, &Context::new(), &env) {
        Ok(value) => {
            println!(
                "✓ {tool} → {}",
                serde_json::to_string_pretty(&tp_val_to_json(&value)).unwrap_or_default()
            );
            println!("  duration: {:?}", start.elapsed());
        }
        Err(e) => {
            eprintln!("✗ {tool} failed: {e}");
            eprintln!("  duration: {:?}", start.elapsed());
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli daemon status [addr]`
fn cmd_daemon(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("status") => {
            let addr = args
                .get(1)
                .cloned()
                .unwrap_or_else(daemon_addr_from_env);
            match list_daemon_tools(&addr) {
                Ok(tools) => {
                    println!("Daemon: OK ({addr})");
                    println!("Registered tools: {}", tools.len());
                    for t in &tools {
                        println!("  {} — {}", t.name, t.description);
                    }
                }
                Err(e) => {
                    eprintln!("Daemon: UNREACHABLE ({addr})");
                    eprintln!("  {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("Usage: tinypipe-cli daemon status [addr]");
            std::process::exit(1);
        }
    }
}

/// Execution'dan ÖNCE env bağımlılık kontrolü: kök grafik + transitive
/// subgraph'ların zorunlu değişkenleri ortamda yoksa listeler ve exit(1).
/// `--no-env-check` ile atlanabilir (dinamik key kullanan grafikler için).
fn run_env_check<S: tinypipe_api::storage::GraphStorage>(
    registry: &tinypipe_tools::SubgraphToolRegistry<S>,
    id: &str,
    env: &tinypipe_env::Env,
) {
    let reports = match registry.validate_env(id, env) {
        Ok(reports) => reports,
        Err(e) => {
            eprintln!("Env check failed: {e}");
            std::process::exit(1);
        }
    };
    if reports.is_empty() {
        return;
    }
    eprintln!("✗ Missing environment variables:");
    for report in &reports {
        for key in &report.missing {
            eprintln!("  {}.{}", report.graph_path, key);
        }
    }
    std::process::exit(1);
}

fn cmd_execute(id: &str, input_json: &str, pause_after: Option<u32>, env: tinypipe_env::Env, skip_env_check: bool) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);
    let graph_def = match storage.load_graph(&graph_id) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Error loading graph: {e}");
            std::process::exit(1);
        }
    };

    // Load the stored compiled plan (FlatBuffers, canonical format).
    // Eski DB'lerde plan NULL olabilir — self-heal yeniden derleyip kaydeder.
    let plan_bytes = load_plan_self_heal(&storage, &graph_id);
    let plan = match tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&plan_bytes) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("Failed to decode compiled plan: {e}");
            std::process::exit(1);
        }
    };

    // Parse input JSON into context
    let input_map: HashMap<String, serde_json::Value> = match serde_json::from_str(input_json) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error parsing input JSON: {e}");
            std::process::exit(1);
        }
    };
    let mut context = Context::new();
    for (k, v) in input_map {
        context.set(k, json_val_to_tp(v));
    }

    // Persist execution record
    let now = now_micros();
    let execution_id = uuid::Uuid::new_v4().to_string();
    let mut execution = Execution {
        id: execution_id.clone(),
        graph_id: graph_id.clone(),
        graph_version: graph_def.version,
        input: context.clone(),
        output: None,
        status: ExecutionStatus::Running,
        error: None,
        started_at: now.clone(),
        completed_at: None,
        duration_us: None,
        context: Some(context.clone()),
    };
    storage
        .save_execution(&execution)
        .expect("Failed to save execution");

    let registry = build_registry(storage);
    if !skip_env_check {
        run_env_check(registry.as_ref(), id, &env);
    }
    let executor = CompiledExecutor::with_env(
        &plan,
        registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
        std::sync::Arc::new(env),
    );
    let start = std::time::Instant::now();

    let policy = tinypipe_vm::PausePolicy {
        max_nodes: pause_after,
        ..Default::default()
    };
    match executor.execute_with(context, &policy, None) {
        Ok(tinypipe_vm::ExecutionOutcome::Completed(result)) => {
            let elapsed = start.elapsed();
            execution.status = ExecutionStatus::Completed;
            execution.output = result.output.clone();
            execution.duration_us = Some(result.duration_us);
            execution.completed_at = Some(now_micros());
            execution.context = Some(result.context.clone());
            registry
                .storage()
                .save_execution(&execution)
                .expect("Failed to update execution");
            record_steps(registry.storage(), &execution_id, &plan, &result);

            println!("✓ Execution completed (id: {})", execution_id);
            println!("  Duration: {} μs", result.duration_us);
            println!("  Wall-clock: {:?}", elapsed);
            println!("  Nodes executed: {}", result.node_count);
            if let Some(ref output) = result.output {
                println!();
                println!(
                    "  Output: {}",
                    serde_json::to_string_pretty(&tp_val_to_json(output))
                        .unwrap_or_else(|_| format!("{:?}", output))
                );
            }
            if !result.context.variables.is_empty() {
                println!();
                println!("  Context:");
                for (key, value) in result.context.variables.iter() {
                    println!(
                        "    {}: {}",
                        key,
                        serde_json::to_string(&tp_val_to_json(value))
                            .unwrap_or_else(|_| "?".into())
                    );
                }
            }
        }
        Ok(tinypipe_vm::ExecutionOutcome::Paused(checkpoint)) => {
            execution.status = ExecutionStatus::Paused;
            execution.completed_at = None;
            execution.duration_us = Some(checkpoint.elapsed_us);
            execution.context = Some(checkpoint.context.clone());
            registry
                .storage()
                .save_execution(&execution)
                .expect("Failed to save paused execution");
            let blob = serde_json::to_vec(&checkpoint).expect("Failed to serialize checkpoint");
            registry
                .storage()
                .save_checkpoint(&execution_id, &blob)
                .expect("Failed to save checkpoint");
            println!(
                "⏸ Execution paused at {} nodes (id: {})",
                checkpoint.node_count, execution_id
            );
            println!("  Resume: tinypipe-cli resume {}", execution_id);
        }
        Err(e) => {
            let elapsed = start.elapsed();
            execution.status = ExecutionStatus::Failed;
            execution.error = Some(e.to_string());
            execution.completed_at = Some(now_micros());
            execution.duration_us = Some(elapsed.as_micros() as u64);
            registry
                .storage()
                .save_execution(&execution)
                .expect("Failed to update execution");
            eprintln!(
                "✗ Execution failed after {:?} (id: {})",
                elapsed, execution_id
            );
            eprintln!("  Error: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli resume <execution_id> [--max-nodes N]` — paused execution'ı checkpoint'ten sürdür.
fn cmd_resume(execution_id: &str, max_nodes: Option<u32>, env: tinypipe_env::Env, skip_env_check: bool) {
    let storage = open_storage();

    let mut exec = match storage.load_execution(execution_id) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error loading execution: {e}");
            std::process::exit(1);
        }
    };
    if !matches!(exec.status, ExecutionStatus::Paused) {
        eprintln!(
            "Error: execution '{execution_id}' is not paused (status: {:?})",
            exec.status
        );
        std::process::exit(1);
    }

    // Checkpoint yükle
    let blob = match storage.load_checkpoint(execution_id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error loading checkpoint: {e}");
            std::process::exit(1);
        }
    };
    let checkpoint: tinypipe_vm::Checkpoint = match serde_json::from_slice(&blob) {
        Ok(cp) => cp,
        Err(e) => {
            eprintln!("Error decoding checkpoint: {e}");
            std::process::exit(1);
        }
    };

    // Plan'ı execution'ın versiyonundan yükle (immutable)
    let plan_bytes = match storage.load_plan_version(&exec.graph_id, exec.graph_version) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "Error loading plan (graph {}, v{}): {e}",
                exec.graph_id.0, exec.graph_version.0
            );
            std::process::exit(1);
        }
    };
    let plan = match tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&plan_bytes) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("Failed to decode compiled plan: {e}");
            std::process::exit(1);
        }
    };

    let registry = build_registry(storage);
    if !skip_env_check {
        run_env_check(registry.as_ref(), &exec.graph_id.0, &env);
    }
    let executor = CompiledExecutor::with_env(
        &plan,
        registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
        std::sync::Arc::new(env),
    );
    let policy = tinypipe_vm::PausePolicy {
        max_nodes,
        ..Default::default()
    };

    match executor.resume(&checkpoint, &policy, None) {
        Ok(tinypipe_vm::ExecutionOutcome::Completed(result)) => {
            exec.status = ExecutionStatus::Completed;
            exec.output = result.output.clone();
            exec.duration_us = Some(result.duration_us);
            exec.completed_at = Some(now_micros());
            exec.context = Some(result.context.clone());
            registry
                .storage()
                .save_execution(&exec)
                .expect("Failed to update execution");
            record_steps(registry.storage(), execution_id, &plan, &result);

            println!("✓ Execution completed (id: {})", execution_id);
            println!("  Total duration: {} μs", result.duration_us);
            println!("  Nodes executed: {}", result.node_count);
            if let Some(ref output) = result.output {
                println!(
                    "  Output: {}",
                    serde_json::to_string_pretty(&tp_val_to_json(output))
                        .unwrap_or_else(|_| format!("{:?}", output))
                );
            }
        }
        Ok(tinypipe_vm::ExecutionOutcome::Paused(cp)) => {
            exec.status = ExecutionStatus::Paused;
            exec.duration_us = Some(cp.elapsed_us);
            exec.context = Some(cp.context.clone());
            registry
                .storage()
                .save_execution(&exec)
                .expect("Failed to save paused execution");
            let blob = serde_json::to_vec(&cp).expect("Failed to serialize checkpoint");
            registry
                .storage()
                .save_checkpoint(execution_id, &blob)
                .expect("Failed to save checkpoint");
            println!(
                "⏸ Still paused at {} nodes (id: {})",
                cp.node_count, execution_id
            );
            println!("  Resume again: tinypipe-cli resume {}", execution_id);
        }
        Err(e) => {
            exec.status = ExecutionStatus::Failed;
            exec.error = Some(e.to_string());
            exec.completed_at = Some(now_micros());
            registry
                .storage()
                .save_execution(&exec)
                .expect("Failed to update execution");
            eprintln!("✗ Resume failed (id: {}): {e}", execution_id);
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli scheduler run [--max-nodes N]` — paused execution'ları sürdür.
/// `--max-nodes` verilirse her execution her turda N node ilerler (loop modu);
/// verilmezse tek turda tamamlanır.
fn cmd_scheduler_run(max_nodes: Option<u32>, env: tinypipe_env::Env) {
    let storage = open_storage();
    let scheduler = tinypipe_scheduler::Scheduler::with_env(storage, std::sync::Arc::new(env));
    let summary = match max_nodes {
        Some(n) => scheduler.run_loop(Some(n), 1000),
        None => scheduler.run_once(None),
    };

    match summary {
        Ok(s) => {
            println!("Scheduler run complete:");
            println!("  Processed:  {}", s.processed);
            println!("  Completed:  {}", s.completed);
            println!("  Still paused: {}", s.still_paused);
            println!("  Failed:     {}", s.failed);
        }
        Err(e) => {
            eprintln!("Scheduler failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Record per-node execution steps from an ExecutionResult (basic audit trail).
/// `node_durations` yalnızca bu segmentte gerçekten çalışan node'ları içerir
/// (resume'da execution_order checkpoint'ten eski node'ları da taşır — çift
/// kayıt önlemek için onu değil, durations'ı dolaşırız).
fn record_steps(
    storage: &SqliteStorage,
    execution_id: &str,
    plan: &tinypipe_ir::compiled::CompiledPlan,
    result: &tinypipe_vm::ExecutionResult,
) {
    let node_by_id: HashMap<&str, &tinypipe_ir::compiled::CompiledNode> =
        plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    // Süreleri gerçek zaman çizelgesine oturt: her step önceki bitişten sonra başlar.
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
        storage.save_step(&step).expect("Failed to save step");
    }
}

/// `tinypipe-cli deploy <id> [version]` — deploy a graph version.
fn cmd_deploy(id: &str, version: Option<Version>) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);

    let deploy_version = match version {
        Some(v) => v,
        None => {
            // Default: deploy current version
            let graph = storage.load_graph(&graph_id).unwrap_or_else(|e| {
                eprintln!("Error loading graph: {e}");
                std::process::exit(1);
            });
            graph.version
        }
    };

    match storage.deploy(&graph_id, deploy_version) {
        Ok(()) => {
            println!("✓ Deployed {} (version {})", graph_id.0, deploy_version.0);
            let g = storage.load_graph(&graph_id).unwrap_or_else(|e| {
                eprintln!("Error reloading graph: {e}");
                std::process::exit(1);
            });
            println!(
                "  Status: {} — Active version: v{}",
                g.status, deploy_version.0
            );
        }
        Err(e) => {
            eprintln!("✗ Deploy failed: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli rollback <id> <version>` — rollback to a previous version.
fn cmd_rollback(id: &str, version: Version) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);

    match storage.rollback(&graph_id, version) {
        Ok(()) => {
            let g = storage.load_graph(&graph_id).unwrap_or_else(|e| {
                eprintln!("Error reloading graph: {e}");
                std::process::exit(1);
            });
            println!(
                "✓ Rolled back to v{} — new version is v{}",
                version.0, g.version.0
            );
            println!("  Code: {}", g.code);
            if g.active {
                println!("  Status: deployed (active_version updated)");
            }
        }
        Err(e) => {
            eprintln!("✗ Rollback failed: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli versions <id>` — list all versions of a graph.
fn cmd_versions(id: &str) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);

    // Load graph to get current info
    let graph = match storage.load_graph(&graph_id) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Error loading graph: {e}");
            std::process::exit(1);
        }
    };

    let versions = match storage.list_versions(&graph_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error listing versions: {e}");
            std::process::exit(1);
        }
    };

    if versions.is_empty() {
        println!("No versions found for graph '{}'.", graph.name);
        return;
    }

    println!("Versions for '{}' ({}):", graph.name, graph_id.0);
    println!("{:<8} {:<8} {:<20} Code", "Version", "Active", "Created");
    println!("{}", "-".repeat(90));
    let active_ver = graph.active_version.map(|v| v.0);
    for (ver, code, created) in &versions {
        let is_active = if Some(*ver) == active_ver {
            "◄── DEPLOYED"
        } else if *ver == graph.version.0 {
            "   (latest)"
        } else {
            ""
        };
        let preview: String = code
            .lines()
            .next()
            .unwrap_or(code)
            .chars()
            .take(50)
            .collect();
        println!(
            "  v{:<5} {:<11} {:<20} {}",
            ver, is_active, created, preview
        );
    }
}

/// `tinypipe-cli list` — list all saved graphs.
fn cmd_list() {
    let storage = open_storage();
    match storage.list_all_graphs(None, None) {
        Ok(items) => {
            if items.is_empty() {
                println!("No graphs found.");
                println!("Use 'tinypipe-cli create <name> <code>' to create one.");
                return;
            }
            println!("Graphs:");
            println!(
                "{:<36}  {:<20}  {:>4}  {:<10}  {:>6}B",
                "ID", "Name", "Ver", "Status", "Code"
            );
            println!("{}", "-".repeat(90));
            for item in &items {
                println!(
                    "{:<36}  {:<20}  v{:>2}  {:<10}  {:>6}",
                    item.id,
                    truncate(&item.name, 20),
                    item.version,
                    item.status,
                    item.code_len
                );
            }
        }
        Err(e) => {
            eprintln!("Error listing graphs: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_check(code: &str) {
    match auto_repair::check_code(code, 1, 3) {
        None => {
            println!("✓ Code is valid.");
        }
        Some(report) => {
            println!("{}", report);
        }
    }
}

/// `tinypipe-cli plan <id> [version] [--format ...] [--view ...] [--direction ...]` — compiled plan'ı dump et.
/// Renderer mantığı `tinypipe-ir::plan_view`'da yaşar; CLI sadece kaydedip yazdırır.
fn cmd_plan_dump(
    id: &str,
    version: Option<Version>,
    format: tinypipe_ir::PlanFormat,
    options: tinypipe_ir::plan_view::RenderOptions,
) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);
    let graph_def = match storage.load_graph(&graph_id) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Error loading graph: {e}");
            std::process::exit(1);
        }
    };

    let plan_bytes = match version {
        Some(v) => load_plan_version_self_heal(&storage, &graph_id, v),
        None => load_plan_self_heal(&storage, &graph_id),
    };

    let plan = match tinypipe_ir::compiled::CompiledPlan::from_fb_bytes(&plan_bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to decode compiled plan: {e}");
            std::process::exit(1);
        }
    };

    let header = tinypipe_ir::PlanDumpHeader {
        graph_name: &graph_def.name,
        graph_version: graph_def.version.0,
        encoded_len: plan_bytes.len(),
    };
    print!("{}", format.render(&plan, &header, options));
}

/// `tinypipe-cli executions list <id>` — list executions of a graph.
fn cmd_executions_list(id: &str) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);
    let executions = match storage.list_executions(&graph_id, None) {
        Ok(items) => items,
        Err(e) => {
            eprintln!("Error listing executions: {e}");
            std::process::exit(1);
        }
    };

    if executions.is_empty() {
        println!("No executions found for '{}'.", id);
        return;
    }

    println!("Executions for '{}':", id);
    println!(
        "{:<38}  {:<10}  {:<20}  {:>8}  {}",
        "ID", "Status", "Started", "Dur(μs)", "Output"
    );
    println!("{}", "-".repeat(100));
    for exec in &executions {
        let preview = exec
            .output
            .as_ref()
            .map(|v| serde_json::to_string(&tp_val_to_json(v)).unwrap_or_default())
            .unwrap_or_default();
        let preview: String = preview.chars().take(30).collect();
        println!(
            "{:<38}  {:<10}  {:<20}  {:>8}  {}",
            exec.id,
            format!("{:?}", exec.status).to_lowercase(),
            exec.started_at,
            exec.duration_us.unwrap_or(0),
            preview
        );
    }
}

/// `tinypipe-cli executions show <execution_id>` — show execution + steps.
fn cmd_executions_show(execution_id: &str) {
    let storage = open_storage();
    let exec = match storage.load_execution(execution_id) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error loading execution: {e}");
            std::process::exit(1);
        }
    };

    println!("Execution: {}", exec.id);
    println!("  Graph: {} (v{})", exec.graph_id.0, exec.graph_version.0);
    println!("  Status: {:?}", exec.status);
    println!("  Started: {}", exec.started_at);
    println!(
        "  Completed: {}",
        exec.completed_at.as_deref().unwrap_or("-")
    );
    println!("  Duration: {} μs", exec.duration_us.unwrap_or(0));
    if let Some(ref err) = exec.error {
        println!("  Error: {}", err);
    }
    if let Some(ref output) = exec.output {
        println!(
            "  Output: {}",
            serde_json::to_string_pretty(&tp_val_to_json(output))
                .unwrap_or_else(|_| format!("{:?}", output))
        );
    }

    let steps = match storage.list_steps(execution_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error loading steps: {e}");
            std::process::exit(1);
        }
    };
    if !steps.is_empty() {
        println!();
        println!("  Steps:");
        println!(
            "  {:<36}  {:<12}  {:<8}  {}",
            "Node", "Op", "Status", "Started"
        );
        println!("  {}", "-".repeat(80));
        for step in &steps {
            println!(
                "  {:<36}  {:<12}  {:<8}  {}",
                step.node_id, step.node_op, step.status, step.started_at
            );
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────

/// `tinypipe-cli report [--profile <name>] [--env K=V]` — rol bazlı portföy raporu.
/// Varsayılan profil: senior (solo kullanıcı için dengeli görünüm).
fn cmd_report(profile_name: Option<&str>, args: &[String]) {
    let storage = open_storage();
    if let Err(e) = tinypipe_insight::profile::seed_builtin_profiles(&storage) {
        eprintln!("Error seeding profiles: {e}");
        std::process::exit(1);
    }
    let profile = match profile_name {
        Some(name) => match tinypipe_insight::profile::resolve(&storage, name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                eprintln!("Error: unknown profile '{name}'");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error loading profile '{name}': {e}");
                std::process::exit(1);
            }
        },
        None => tinypipe_insight::profile::builtin_profile("senior").unwrap(),
    };
    let env = parse_env_args(args);
    let metrics = match tinypipe_insight::metrics::collect(&storage, Some(&env)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error collecting metrics: {e}");
            std::process::exit(1);
        }
    };
    print!("{}", tinypipe_insight::report::render(&profile, &metrics));
}

/// `tinypipe-cli profiles list|show|create|delete` — profil CRUD.
fn cmd_profiles(args: &[String]) {
    let storage = open_storage();
    if let Err(e) = tinypipe_insight::profile::seed_builtin_profiles(&storage) {
        eprintln!("Error seeding profiles: {e}");
        std::process::exit(1);
    }
    match args.first().map(String::as_str) {
        Some("list") => {
            match storage.list_profiles() {
                Ok(profiles) => {
                    println!("{:<14} {:<10} {:<10} {:<14} {:<34} {}", "name", "label", "view", "direction", "focus", "description");
                    println!("{}", "-".repeat(120));
                    for p in &profiles {
                        let tag = if p.builtin { "builtin" } else { "custom" };
                        println!(
                            "{:<14} {:<10} {:<10} {:<14} {:<34} {}",
                            p.name,
                            p.label,
                            p.view,
                            p.direction,
                            truncate(&p.focus.join(","), 34),
                            truncate(&p.description, 40)
                        );
                        let _ = tag;
                    }
                }
                Err(e) => {
                    eprintln!("Error listing profiles: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("show") => {
            let name = match args.get(1) {
                Some(n) => n,
                None => {
                    eprintln!("Usage: tinypipe-cli profiles show <name>");
                    std::process::exit(1);
                }
            };
            match storage.load_profile(name) {
                Ok(p) => print_profile(&p),
                Err(_) => match tinypipe_insight::profile::builtin_profile(name) {
                    Some(p) => print_profile(&p),
                    None => {
                        eprintln!("Error: profile '{name}' not found");
                        std::process::exit(1);
                    }
                },
            }
        }
        Some("create") => {
            cmd_profile_create(&storage, &args[1..]);
        }
        Some("delete") => {
            let name = match args.get(1) {
                Some(n) => n,
                None => {
                    eprintln!("Usage: tinypipe-cli profiles delete <name>");
                    std::process::exit(1);
                }
            };
            match storage.delete_profile(name) {
                Ok(()) => println!("Profile '{name}' deleted."),
                Err(e) => {
                    eprintln!("Error deleting profile '{name}': {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("Unknown profiles subcommand: {other}");
            eprintln!("Commands: list, show, create, delete");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: tinypipe-cli profiles list | show <name> | create <name> [...] | delete <name>");
            std::process::exit(1);
        }
    }
}

/// `profiles create` alt komutu — bayraklardan özel profil kurar.
fn cmd_profile_create(storage: &SqliteStorage, args: &[String]) {
    let name = match args.first() {
        Some(n) if !n.starts_with("--") => n,
        _ => {
            eprintln!("Usage: tinypipe-cli profiles create <name> [--label L] [--description D] [--view full|summary|layers] [--direction td|lr] [--focus a,b] [--config <json>]");
            std::process::exit(1);
        }
    };
    if tinypipe_insight::profile::builtin_profile(name).is_some() {
        eprintln!("Error: '{name}' is a builtin profile name — pick a different name");
        std::process::exit(1);
    }
    let flag = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|pos| args.get(pos + 1).cloned())
    };
    let label = flag("--label").unwrap_or_else(|| name.to_string());
    let description = flag("--description").unwrap_or_default();
    let view = flag("--view")
        .filter(|v| tinypipe_ir::plan_view::ViewLevel::parse(v).is_some())
        .unwrap_or_else(|| {
            eprintln!("Error: --view must be one of: full, summary, layers");
            std::process::exit(1);
        });
    let direction = flag("--direction")
        .filter(|d| tinypipe_ir::plan_view::Direction::parse(d).is_some())
        .unwrap_or_else(|| {
            eprintln!("Error: --direction must be one of: td, lr");
            std::process::exit(1);
        });
    let focus: Vec<String> = flag("--focus")
        .map(|f| {
            f.split(',')
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "portfolio".into(),
                "executions".into(),
                "structure".into(),
            ]
        });
    for key in &focus {
        if !VALID_FOCUS_KEYS.contains(&key.as_str()) {
            eprintln!(
                "Error: unknown focus key '{key}' — valid: {}",
                VALID_FOCUS_KEYS.join(", ")
            );
            std::process::exit(1);
        }
    }
    let config: serde_json::Value = flag("--config")
        .map(|c| serde_json::from_str(&c).unwrap_or_else(|e| {
            eprintln!("Error: --config must be valid JSON: {e}");
            std::process::exit(1);
        }))
        .unwrap_or_else(|| serde_json::json!({}));
    let profile = tinypipe_api::types::Profile {
        name: name.clone(),
        label,
        description,
        view,
        direction,
        focus,
        config,
        builtin: false,
    };
    match storage.save_profile(&profile) {
        Ok(()) => println!("Profile '{name}' created."),
        Err(e) => {
            eprintln!("Error creating profile '{name}': {e}");
            std::process::exit(1);
        }
    }
}

/// Rapor bölümü anahtar listesi (profil create validation'ı için).
const VALID_FOCUS_KEYS: [&str; 10] = [
    "portfolio", "executions", "duration", "reliability", "tools", "endpoints", "env",
    "structure", "subgraphs", "churn",
];

/// Profili güzel yazdırır.
fn print_profile(p: &tinypipe_api::types::Profile) {
    println!("name:        {}", p.name);
    println!("label:       {}", p.label);
    println!("description: {}", p.description);
    println!("view:        {} (plan --view)", p.view);
    println!("direction:   {} (plan --direction)", p.direction);
    println!("focus:       {}", p.focus.join(", "));
    println!(
        "config:      {}",
        serde_json::to_string_pretty(&p.config).unwrap_or_default()
    );
    println!("builtin:     {}", p.builtin);
}

fn open_storage() -> SqliteStorage {
    let db_path = std::env::var("TINYPIPE_DB").unwrap_or_else(|_| "./tinypipe.db".to_string());
    SqliteStorage::open(&db_path)
        .unwrap_or_else(|_| panic!("Failed to open storage at '{}'", db_path))
}

/// Current time in microseconds since UNIX epoch (storage timestamp format).
fn now_micros() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// Expand `\n`/`\t` escapes in a shell-passed code argument.
/// Only the literal two-character sequences are replaced; `\\n` stays intact.
fn unescape_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                    continue;
                }
                Some('t') => {
                    chars.next();
                    out.push('\t');
                    continue;
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// UTF-8 güvenli kısaltma (char sınırlarında keser).
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{}...", cut)
    }
}

/// Convert serde_json::Value to tinypipe_api::Value.
fn json_val_to_tp(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(arr) => {
            Value::Array(arr.into_iter().map(json_val_to_tp).collect())
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<String, Value> = obj
                .into_iter()
                .map(|(k, v)| (k, json_val_to_tp(v)))
                .collect();
            Value::Object(map)
        }
    }
}

/// Convert tinypipe_api::Value to serde_json::Value.
fn tp_val_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(tp_val_to_json).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), tp_val_to_json(v));
            }
            serde_json::Value::Object(out)
        }
    }
}
