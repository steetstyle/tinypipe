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
use tinypipe_api::types::{Context, Value};
use tinypipe_api::types::{Execution, ExecutionStatus, ExecutionStep};
use tinypipe_api::types::{GraphId, Version};
use tinypipe_compiler::{auto_repair, compile};
use tinypipe_storage::SqliteStorage;
use tinypipe_vm::CompiledExecutor;
use tinypipe_vm::MockToolRegistry;

// LLM integration (requires `llm` feature)
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::provider::{OllamaConfig, Provider};
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::LlmContext;

fn main() {
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
        eprintln!("                                   Dump the compiled plan");
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
                eprintln!("Usage: tinypipe-cli execute <id> [json_input] [--pause-after N]");
                std::process::exit(1);
            }
            let input_json = args.get(3).map(|s| s.as_str()).unwrap_or("{}");
            let mut pause_after = None;
            if let Some(pos) = args.iter().position(|a| a == "--pause-after") {
                pause_after = args.get(pos + 1).and_then(|v| v.parse::<u32>().ok());
                if pause_after.is_none() {
                    eprintln!("Error: --pause-after requires a number");
                    std::process::exit(1);
                }
            }
            cmd_execute(&args[2], input_json, pause_after);
        }
        "resume" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli resume <execution_id> [--max-nodes N]");
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
            cmd_resume(&args[2], max_nodes);
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
            cmd_scheduler_run(max_nodes);
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
                eprintln!("Usage: tinypipe-cli plan <id> [version] [--format text|mermaid|dot]");
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
            cmd_plan_dump(&args[2], version, format);
        }
        "check" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli check <code>");
                std::process::exit(1);
            }
            cmd_check(&unescape_code(&args[2]));
        }
        _ => {
            eprintln!("Unknown command: {command}");
            eprintln!("Commands: create, deploy, rollback, versions, update, execute, resume, scheduler, list, check");
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
    println!("  Binary (bincode): {} bytes", output.binary.len());
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
                "  Nodes: {}, Edges: {}, Binary (bincode): {} bytes, FB: {} bytes",
                output.compiled.metadata.node_count,
                output.compiled.metadata.edge_count,
                output.binary.len(),
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
fn cmd_execute(id: &str, input_json: &str, pause_after: Option<u32>) {
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

    let registry = MockToolRegistry::new();
    let executor = CompiledExecutor::new(&plan, &registry);
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
            storage
                .save_execution(&execution)
                .expect("Failed to update execution");
            record_steps(&storage, &execution_id, &plan, &result);

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
            storage
                .save_execution(&execution)
                .expect("Failed to save paused execution");
            let blob = serde_json::to_vec(&checkpoint).expect("Failed to serialize checkpoint");
            storage
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
            storage
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
fn cmd_resume(execution_id: &str, max_nodes: Option<u32>) {
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

    let registry = MockToolRegistry::new();
    let executor = CompiledExecutor::new(&plan, &registry);
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
            storage
                .save_execution(&exec)
                .expect("Failed to update execution");
            record_steps(&storage, execution_id, &plan, &result);

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
            storage
                .save_execution(&exec)
                .expect("Failed to save paused execution");
            let blob = serde_json::to_vec(&cp).expect("Failed to serialize checkpoint");
            storage
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
            storage
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
fn cmd_scheduler_run(max_nodes: Option<u32>) {
    let storage = open_storage();
    let scheduler = tinypipe_scheduler::Scheduler::new(storage);
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
fn record_steps(
    storage: &SqliteStorage,
    execution_id: &str,
    plan: &tinypipe_ir::compiled::CompiledPlan,
    result: &tinypipe_vm::ExecutionResult,
) {
    let node_by_id: HashMap<&str, &tinypipe_ir::compiled::CompiledNode> =
        plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut base = now_micros();
    for node_id in &result.execution_order {
        let op = node_by_id
            .get(node_id.as_str())
            .map(|n| format!("{:?}", n.op))
            .unwrap_or_else(|| "unknown".into());
        let step = ExecutionStep {
            id: uuid::Uuid::new_v4().to_string(),
            execution_id: execution_id.to_string(),
            node_id: node_id.clone(),
            node_op: op,
            status: "completed".into(),
            error: None,
            started_at: base.clone(),
            completed_at: Some(base.clone()),
            duration_us: Some(0),
            context_before: None,
            context_after: None,
            parent_step_id: None,
        };
        base = now_micros();
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

/// `tinypipe-cli plan <id> [version] [--format text|mermaid|dot]` — compiled plan'ı dump et.
/// Renderer mantığı `tinypipe-ir::plan_dump`'da yaşar; CLI sadece kaydedip yazdırır.
fn cmd_plan_dump(id: &str, version: Option<Version>, format: tinypipe_ir::PlanFormat) {
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
    print!("{}", format.render(&plan, &header));
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

fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        format!("{}...", &s[..max_chars.saturating_sub(3)])
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
