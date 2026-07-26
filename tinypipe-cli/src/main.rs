//! `tinypipe-cli` — CLI for the tinypipe execution graph platform.
//!
//! Usage:
//!   tinypipe-cli create <name> <code>         Create a new graph from code
//!   tinypipe-cli create --from-llm <name> <description>
//!                                            Create a graph from natural language
//!   tinypipe-cli update <id> <code>           Update a graph (new version)
//!   tinypipe-cli execute <id> [json_input]    Execute a graph
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
use tinypipe_api::types::{GraphId, Version};
use tinypipe_api::types::{Context, Value};
use tinypipe_compiler::{auto_repair, compile, transform};
use tinypipe_storage::SqliteStorage;
use tinypipe_vm::CompiledExecutor;
use tinypipe_vm::MockToolRegistry;

// LLM integration (requires `llm` feature)
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::LlmContext;
#[cfg(feature = "llm")]
use tinypipe_compiler::llm::provider::{OllamaConfig, Provider};

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
        eprintln!("  update <id> <code>                Update a graph (new version)");
        eprintln!("  execute <id> [json_input]         Execute a graph");
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
                cmd_create(&args[2], &args[3]);
            }
        }
        "update" => {
            if args.len() < 4 {
                eprintln!("Usage: tinypipe-cli update <id> <code>");
                std::process::exit(1);
            }
            cmd_update(&args[2], &args[3]);
        }
        "execute" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli execute <id> [json_input]");
                std::process::exit(1);
            }
            let input_json = args.get(3).map(|s| s.as_str()).unwrap_or("{}");
            cmd_execute(&args[2], input_json);
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
        "check" => {
            if args.len() < 3 {
                eprintln!("Usage: tinypipe-cli check <code>");
                std::process::exit(1);
            }
            cmd_check(&args[2]);
        }
        _ => {
            eprintln!("Unknown command: {command}");
            eprintln!("Commands: create, deploy, rollback, versions, update, execute, list, check");
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
    let graph_id = storage.create_graph(name, code)
        .expect("Failed to save graph");

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
    println!("  Node sayısı: {}, Edge sayısı: {}",
        output.compiled.metadata.node_count,
        output.compiled.metadata.edge_count);
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
            }.into_backend();
        }
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            eprintln!("  🔑 Using Anthropic (claude-sonnet-4-20250514)");
            return Provider::Anthropic {
                api_key: key,
                model: "claude-sonnet-4-20250514".into(),
            }.into_backend();
        }
    }

    // Default: Ollama (local)
    eprintln!("  🦙 Using local Ollama (llama3.2 at http://localhost:11434)");
    eprintln!("  (Set OPENAI_API_KEY or ANTHROPIC_API_KEY env var to use cloud)");
    Provider::Ollama {
        config: OllamaConfig {
            base_url: std::env::var("OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434".into()),
            model: std::env::var("OLLAMA_MODEL")
                .unwrap_or_else(|_| "llama3.2".into()),
            timeout: std::time::Duration::from_secs(120),
            keep_alive: std::time::Duration::from_secs(300),
            temperature: 0.1,
        },
    }.into_backend()
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
            println!("✓ Graph updated: {} (version {})", id, version.0);
            println!("  Nodes: {}, Edges: {}, Binary (bincode): {} bytes, FB: {} bytes",
                output.compiled.metadata.node_count,
                output.compiled.metadata.edge_count,
                output.binary.len(),
                output.fb_binary.len());
        }
        Err(e) => {
            eprintln!("Error updating graph: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli execute <id> [json_input]` — load and execute a graph.
fn cmd_execute(id: &str, input_json: &str) {
    let storage = open_storage();
    let graph_id = resolve_graph_id(&storage, id);
    let graph_def = match storage.load_graph(&graph_id) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Error loading graph: {e}");
            std::process::exit(1);
        }
    };

    let plan = match transform::transform(&graph_def.code) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("Failed to re-compile graph code: {:?}", e);
            std::process::exit(1);
        }
    };

    // Validate
    if let Err(errors) = tinypipe_compiler::validator::validate(&plan) {
        eprintln!("Validation failed:");
        for err in &errors {
            eprintln!("  {}: {}", err.node_id, err.message);
        }
        std::process::exit(1);
    }

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

    let registry = MockToolRegistry::new();
    let compiled = tinypipe_ir::compiled::CompiledPlan::from_execution_plan(&plan, vec![]);
    let executor = CompiledExecutor::new(&compiled, &registry);
    let start = std::time::Instant::now();
    match executor.execute(context) {
        Ok(result) => {
            let elapsed = start.elapsed();
            println!("✓ Execution completed");
            println!("  Duration: {} μs", result.duration_us);
            println!("  Wall-clock: {:?}", elapsed);
            println!("  Nodes executed: {}", result.node_count);
            if let Some(ref output) = result.output {
                println!();
                println!("  Output: {}", serde_json::to_string_pretty(&tp_val_to_json(output))
                    .unwrap_or_else(|_| format!("{:?}", output)));
            }
            if !result.context.variables.is_empty() {
                println!();
                println!("  Context:");
                for (key, value) in result.context.variables.iter() {
                    println!("    {}: {}", key, serde_json::to_string(&tp_val_to_json(value))
                        .unwrap_or_else(|_| "?".into()));
                }
            }
        }
        Err(e) => {
            let elapsed = start.elapsed();
            eprintln!("✗ Execution failed after {:?}", elapsed);
            eprintln!("  Error: {e}");
            std::process::exit(1);
        }
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
            println!("  Status: {} — Active version: v{}", g.status, deploy_version.0);
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
            println!("✓ Rolled back to v{} — new version is v{}", version.0, g.version.0);
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
        let preview: String = code.lines().next()
            .unwrap_or(code)
            .chars().take(50).collect();
        println!("  v{:<5} {:<11} {:<20} {}", ver, is_active, created, preview);
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
            println!("{:<36}  {:<20}  {:>4}  {:<10}  {:>6}B", "ID", "Name", "Ver", "Status", "Code");
            println!("{}", "-".repeat(90));
            for item in &items {
                println!("{:<36}  {:<20}  v{:>2}  {:<10}  {:>6}",
                    item.id, truncate(&item.name, 20), item.version,
                    item.status, item.code_len);
            }
        }
        Err(e) => {
            eprintln!("Error listing graphs: {e}");
            std::process::exit(1);
        }
    }
}

/// `tinypipe-cli check <code>` — check code and show auto-repair report.
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

// ─── Helpers ───────────────────────────────────────────────────────

fn open_storage() -> SqliteStorage {
    let db_path = std::env::var("TINYPIPE_DB")
        .unwrap_or_else(|_| "./tinypipe.db".to_string());
    SqliteStorage::open(&db_path)
        .unwrap_or_else(|_| panic!("Failed to open storage at '{}'", db_path))
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
            let map: HashMap<String, Value> = obj.into_iter()
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
