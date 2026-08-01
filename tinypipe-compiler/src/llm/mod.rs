//! `tinypipe-compiler` LLM Integration — Natural Language → Execution Plan.
//!
//! # Purpose
//!
//! Translates natural language descriptions of workflows into executable
//! `CompiledPlan`s using an LLM backend (OpenAI, Anthropic, Ollama, etc.).
//!
//! # Pipeline
//!
//! ```text
//! Natural language + context → [LLM] → Python code → [compiler pipeline] → CompiledPlan
//!                                                      ↻ auto-repair loop
//! ```
//!
//! # Usage
//!
//! ```ignore
//! // Requires `llm` feature and a configured provider (OpenAI, Anthropic, Ollama, or Mock)
//! use tinypipe_compiler::llm::{compile_from_natural_language, LlmBackend, LlmContext};
//! use tinypipe_compiler::llm::provider::{Provider, MockBackend};
//!
//! let backend = Provider::Mock {
//!     responses: vec!["def graph(x: int):\n    return x".into()],
//! }.into_backend();
//! let context = LlmContext::default();
//! let plan = compile_from_natural_language(
//!     "return the input value",
//!     &*backend,
//!     &context,
//!     3,
//! ).unwrap();
//! println!("Compiled plan v{} ({} nodes)", plan.version, plan.nodes.len());
//! ```

pub mod provider;

use crate::auto_repair;
use std::fmt;
use std::time::{Duration, Instant};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::plan::ExecutionPlan;

// ─── Re-exports ───────────────────────────────────────────────────────

pub use provider::{OllamaConfig, Provider};

// ─── Types ────────────────────────────────────────────────────────────

/// Context passed to the LLM alongside the natural language prompt.
#[derive(Debug, Clone, Default)]
pub struct LlmContext {
    /// Names and schemas of available tools (e.g., `["math.add(a: int, b: int) -> int"]`).
    pub available_tools: Vec<String>,
    /// Input/output variable names the plan should expose.
    pub known_variables: Vec<String>,
    /// Additional system instructions for the LLM.
    pub system_prompt: Option<String>,
    /// Execution constraints (e.g., time limit, memory limit).
    pub constraints: Vec<String>,
}

/// Error type for LLM operations.
#[derive(Debug, Clone)]
pub enum LlmError {
    /// HTTP or network failure calling the LLM API.
    Network(String),
    /// LLM returned an empty or unparseable response.
    EmptyResponse,
    /// LLM returned invalid Python code that couldn't be fixed.
    InvalidCode(String),
    /// Compiler pipeline rejected the code after all auto-repair attempts.
    CompilerRejected(String),
    /// No LLM backend configured.
    NoBackend,
    /// Rate limited by the provider.
    RateLimited(Duration),
    /// Unknown error.
    Other(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Network(msg) => write!(f, "LLM network error: {}", msg),
            LlmError::EmptyResponse => write!(f, "LLM returned empty response"),
            LlmError::InvalidCode(msg) => write!(f, "LLM generated invalid code: {}", msg),
            LlmError::CompilerRejected(msg) => write!(f, "Compiler rejected LLM output: {}", msg),
            LlmError::NoBackend => write!(f, "No LLM backend configured"),
            LlmError::RateLimited(dur) => {
                write!(f, "LLM rate limited, retry in {}s", dur.as_secs())
            }
            LlmError::Other(msg) => write!(f, "LLM error: {}", msg),
        }
    }
}

impl std::error::Error for LlmError {}

/// The core trait for LLM backends.
///
/// Each provider (OpenAI, Anthropic, Ollama) implements this trait.
/// The `generate_code` method takes a natural language prompt and context,
/// and returns executable Python code (Restricted Python) that the tinypipe
/// compiler can transform into an ExecutionPlan.
pub trait LlmBackend: Send + Sync {
    /// Generate Restricted Python code from a natural language description.
    ///
    /// `prompt` is the natural language description of the workflow.
    /// `context` carries additional metadata (available tools, variables, etc.).
    fn generate_code(&self, prompt: &str, context: &LlmContext) -> Result<String, LlmError>;

    /// Generate a structured execution plan directly (JSON-ish format).
    ///
    /// By default, falls back to `generate_code` + compiler pipeline.
    /// Override if the provider supports structured output directly.
    fn generate_plan(&self, prompt: &str, context: &LlmContext) -> Result<ExecutionPlan, LlmError> {
        // Default: generate code, compile it
        let code = self.generate_code(prompt, context)?;
        crate::transform::transform(&code).map_err(|errors| {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| format!("{}:{} — {}", e.line, e.column, e.message))
                .collect();
            LlmError::InvalidCode(msgs.join("\n"))
        })
    }

    /// Provider name for logging/metrics.
    fn name(&self) -> &'static str;
}

// ─── System Prompts ───────────────────────────────────────────────────

/// Default system prompt for the LLM when generating Restricted Python code.
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a workflow generator that outputs Restricted Python code.

## Input
You will receive a natural language description of a workflow.

## Output Rules
1. Output ONLY valid Python code wrapped in ```python...``` blocks.
2. Define a function `graph(a: int, b: str, ...)` with individual named parameters for each input.
   - Use `a`, `b`, `x`, `y` etc. as parameter names matching the input variables.
   - Do NOT use `inputs: dict` — use individual typed parameters.
3. Allowed tools are called via `call("tool:name", key=value)` — returns a value.
4. Use conditionals and loops naturally (they'll be compiled to DECIDE/LOOP/BRANCH ops).
5. NO imports, NO `import`, NO `eval()`, NO `exec()`, NO `__import__`.
6. NO file I/O, NO subprocess, NO network calls (except via registered tools).
7. Strings must be plain text — NO f-strings with complex expressions.
8. Use `x = call("tool:math.add", a=a, b=b)` syntax for tool dispatch.
9. Always `return result_value` at the end (can be any expression).
10. Use `call("subgraph:<name>", input_key=value)` for subgraph dispatch.
11. Every input parameter declared in the function signature MUST be used in the function body.

## Example
Input: "Take a name from input and greet the user"
Output:
```python
def graph(name: str) -> str:
    greeting = call("tool:greet", name=name)
    return greeting
```
"#;

/// Build the full system prompt with context.
pub fn build_system_prompt(context: &LlmContext) -> String {
    let mut parts = vec![DEFAULT_SYSTEM_PROMPT.to_string()];

    if let Some(ref custom) = context.system_prompt {
        parts.push(custom.clone());
    }

    if !context.available_tools.is_empty() {
        parts.push(format!(
            "\n## Available Tools\n{}\n",
            context.available_tools.join("\n")
        ));
    }

    if !context.known_variables.is_empty() {
        parts.push(format!(
            "\n## Available Variables\n{}\n",
            context.known_variables.join("\n")
        ));
    }

    if !context.constraints.is_empty() {
        parts.push(format!(
            "\n## Constraints\n{}\n",
            context.constraints.join("\n")
        ));
    }

    parts.join("\n")
}

/// Extract Python code from LLM response (handles markdown code blocks).
pub fn extract_code_from_response(response: &str) -> Option<String> {
    // Try ```python ... ``` block first
    if let Some(start) = response.find("```python") {
        let after_start = &response[start + 9..];
        if let Some(end) = after_start.find("```") {
            let code = after_start[..end].trim().to_string();
            return Some(code);
        }
    }
    // Try ``` ... ``` block
    if let Some(start) = response.find("```") {
        let after_start = &response[start + 3..];
        if let Some(end) = after_start.find("```") {
            let code = after_start[..end].trim().to_string();
            return Some(code);
        }
    }
    // Fallback: whole response (assume it's code)
    let trimmed = response.trim();
    if !trimmed.is_empty() && !trimmed.contains("```") {
        return Some(trimmed.to_string());
    }
    None
}

// ─── Main Entry Point ────────────────────────────────────────────────

/// Compile a natural language description into a `CompiledPlan` using an LLM.
///
/// # Arguments
///
/// * `description` - Natural language description of the workflow.
/// * `backend` - LLM backend (OpenAI, Anthropic, Ollama, etc.).
/// * `context` - Additional context (available tools, variables, etc.).
/// * `max_repair_attempts` - Maximum auto-repair retries if the compiler rejects the code.
///
/// # Returns
///
/// A `CompiledPlan` ready for storage and execution.
pub fn compile_from_natural_language(
    description: &str,
    backend: &dyn LlmBackend,
    context: &LlmContext,
    max_repair_attempts: u32,
) -> Result<CompiledPlan, LlmError> {
    let system_prompt = build_system_prompt(context);

    let mut last_error: Option<String> = None;

    for attempt in 1..=max_repair_attempts {
        // Build the prompt (include previous error for auto-repair)
        let prompt = if let Some(ref err) = last_error {
            format!(
                "{}\n\nYour previous code had compiler errors:\n{}\n\nPlease fix the code and try again.",
                description, err
            )
        } else {
            format!(
                "{}\n\nGenerate a `graph(...)` function using the available tools.",
                description
            )
        };

        // Build context with system prompt
        let ctx = LlmContext {
            system_prompt: Some(system_prompt.clone()),
            ..context.clone()
        };

        // Call the LLM
        let llm_start = Instant::now();
        let response = backend.generate_code(&prompt, &ctx)?;
        let llm_duration = llm_start.elapsed();
        tracing::info!(
            attempt,
            llm_ms = llm_duration.as_millis() as u64,
            "LLM code generation"
        );

        // Extract Python code from the response
        let extracted = extract_code_from_response(&response);
        let code_str = match extracted.as_ref() {
            Some(c) => c,
            None => {
                last_error = Some("LLM response contained no valid Python code block".into());
                continue;
            }
        };

        // Run through the compiler pipeline
        let plan_result = crate::transform::transform(code_str);
        let plan = match plan_result {
            Ok(p) => p,
            Err(errors) => {
                if let Some(err) = errors.first() {
                    let report = auto_repair::from_transform_error(err, code_str);
                    last_error = Some(report.to_string());
                } else {
                    last_error = Some("Unknown transform error".into());
                }
                continue;
            }
        };

        // Validate
        if let Err(errors) = crate::validator::validate(&plan) {
            let report = auto_repair::from_validation_errors(&errors, code_str);
            last_error = Some(report.to_string());
            continue;
        }

        // Codegen
        match crate::backend::codegen::codegen(plan) {
            Ok(output) => {
                tracing::info!(
                    attempt,
                    nodes = output.compiled.nodes.len(),
                    "Compilation successful"
                );
                return Ok(output.compiled);
            }
            Err(e) => {
                let report = auto_repair::from_codegen_error(&e.message, code_str);
                last_error = Some(report.to_string());
                continue;
            }
        }
    }

    // All attempts exhausted
    Err(LlmError::CompilerRejected(
        last_error.unwrap_or_else(|| "Max repair attempts reached".into()),
    ))
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::MockBackend;

    #[test]
    fn test_extract_code_from_response_python_block() {
        let response = r#"
Here's the code you need:
```python
def graph(inputs: dict) -> dict:
    return {"result": 42}
```
That should do it.
"#;
        let code = extract_code_from_response(response);
        assert_eq!(
            code.as_deref(),
            Some("def graph(inputs: dict) -> dict:\n    return {\"result\": 42}")
        );
    }

    #[test]
    fn test_extract_code_from_response_generic_block() {
        let response = "```\ndef graph(x):\n    return x\n```";
        let code = extract_code_from_response(response);
        assert_eq!(code.as_deref(), Some("def graph(x):\n    return x"));
    }

    #[test]
    fn test_extract_code_from_response_no_block() {
        let response = "def graph():\n    pass";
        let code = extract_code_from_response(response);
        assert_eq!(code.as_deref(), Some("def graph():\n    pass"));
    }

    #[test]
    fn test_extract_code_from_response_empty() {
        assert!(extract_code_from_response("").is_none());
        assert!(extract_code_from_response("   ").is_none());
        // Empty code block should still return Some("") — LLM sent an empty block
        assert!(extract_code_from_response("```\n```").is_some());
        assert_eq!(extract_code_from_response("```\n```").unwrap(), "");
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let context = LlmContext {
            available_tools: vec!["tool:math.add(a: int, b: int) -> int".into()],
            known_variables: vec!["x: int".into()],
            constraints: vec!["Max execution time: 5s".into()],
            ..Default::default()
        };
        let prompt = build_system_prompt(&context);
        assert!(prompt.contains("Available Tools"));
        assert!(prompt.contains("math.add"));
        assert!(prompt.contains("Available Variables"));
        assert!(prompt.contains("x: int"));
        assert!(prompt.contains("Constraints"));
        assert!(prompt.contains("5s"));
    }

    #[test]
    fn test_compile_from_natural_language_mock_ok() {
        // Must have at least one INPUT node and reach a terminal (ACT)
        let backend = MockBackend::new(Ok("def graph(x: int):\n    return x".into()));
        let context = LlmContext::default();
        let result = compile_from_natural_language("return input x", &backend, &context, 3);
        assert!(
            result.is_ok(),
            "Should compile successfully: {:?}",
            result.err()
        );
        let plan = result.unwrap();
        assert!(plan.version >= 1);
        assert!(!plan.nodes.is_empty());
    }

    #[test]
    fn test_compile_from_natural_language_mock_repair() {
        // First call returns invalid code (parse error), second returns valid code
        let backend = MockBackend::new_with_sequence(vec![
            Ok("def graph(x: int):\n    invalid syntax{{{}}".into()),
            Ok("def graph(x: int):\n    return x".into()),
        ]);
        let context = LlmContext::default();
        let result = compile_from_natural_language("return input x", &backend, &context, 3);
        assert!(
            result.is_ok(),
            "Should succeed after repair: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_compile_from_natural_language_mock_all_fail() {
        let backend = MockBackend::new(Ok("def graph(x: int):\n    invalid syntax{{{}}".into()));
        let context = LlmContext::default();
        let result = compile_from_natural_language(
            "return input x",
            &backend,
            &context,
            1, // only 1 attempt
        );
        assert!(result.is_err(), "Should fail after exhausting attempts");
        match result {
            Err(LlmError::CompilerRejected(_)) => {} // expected
            _ => panic!("Expected CompilerRejected error"),
        }
    }
}
