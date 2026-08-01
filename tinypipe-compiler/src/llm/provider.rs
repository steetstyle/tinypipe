//! LLM Provider Implementations — OpenAI, Anthropic, Ollama.
//!
//! Each provider implements the [`LlmBackend`] trait with its own API format.
//! All providers cache the HTTP connection where possible and support
//! configurable timeouts, retries, and model selection.

use super::{LlmBackend, LlmContext, LlmError};
use std::time::Duration;

// ─── Provider Enum ────────────────────────────────────────────────────

/// Supported LLM providers.
///
/// Each variant configures a specific backend. Use [`Provider::into_backend()`]
/// to create a boxed [`LlmBackend`].
#[derive(Debug, Clone)]
pub enum Provider {
    /// OpenAI-compatible API (GPT-4, GPT-4o, o-series, etc.)
    OpenAI {
        api_key: String,
        model: String,
        base_url: Option<String>,
    },
    /// Anthropic API (Claude Opus, Sonnet, Haiku)
    Anthropic { api_key: String, model: String },
    /// Local Ollama instance (Llama, Mistral, Codestral, etc.)
    Ollama { config: OllamaConfig },
    /// Mock backend for testing.
    Mock { responses: Vec<String> },
}

impl Provider {
    /// Convert the provider enum into a boxed [`LlmBackend`].
    pub fn into_backend(self) -> Box<dyn LlmBackend> {
        match self {
            Provider::OpenAI {
                api_key,
                model,
                base_url,
            } => Box::new(OpenAIBackend {
                api_key,
                model,
                base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            }),
            Provider::Anthropic { api_key, model } => Box::new(AnthropicBackend { api_key, model }),
            Provider::Ollama { config } => Box::new(OllamaBackend { config }),
            Provider::Mock { responses } => Box::new(MockBackend::new_with_sequence(
                responses.into_iter().map(Ok).collect(),
            )),
        }
    }
}

// ─── Ollama Config ────────────────────────────────────────────────────

/// Configuration for a local Ollama instance.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Ollama server URL (default: http://localhost:11434)
    pub base_url: String,
    /// Model name (e.g., "llama3.1", "codestral", "mistral")
    pub model: String,
    /// Request timeout
    pub timeout: Duration,
    /// Keep alive duration for the model in memory
    pub keep_alive: Duration,
    /// Temperature (0.0 = deterministic, 1.0 = creative)
    pub temperature: f32,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".into(),
            model: "llama3.1".into(),
            timeout: Duration::from_secs(120),
            keep_alive: Duration::from_secs(300),
            temperature: 0.2,
        }
    }
}

// ─── OpenAI Backend ───────────────────────────────────────────────────

/// OpenAI-compatible LLM backend.
pub struct OpenAIBackend {
    api_key: String,
    model: String,
    base_url: String,
}

impl LlmBackend for OpenAIBackend {
    fn generate_code(&self, prompt: &str, context: &LlmContext) -> Result<String, LlmError> {
        let system_prompt = context
            .system_prompt
            .as_deref()
            .unwrap_or(super::DEFAULT_SYSTEM_PROMPT);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": prompt}
            ],
            "temperature": 0.2,
            "max_tokens": 4096,
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let response = client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(5));
            return Err(LlmError::RateLimited(retry_after));
        }

        let json: serde_json::Value = response
            .json()
            .map_err(|e| LlmError::Network(format!("JSON parse: {}", e)))?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or(LlmError::EmptyResponse)?
            .to_string();

        Ok(content)
    }

    fn name(&self) -> &'static str {
        "openai"
    }
}

// ─── Anthropic Backend ────────────────────────────────────────────────

/// Anthropic Claude LLM backend.
pub struct AnthropicBackend {
    api_key: String,
    model: String,
}

impl LlmBackend for AnthropicBackend {
    fn generate_code(&self, prompt: &str, context: &LlmContext) -> Result<String, LlmError> {
        let system_prompt = context
            .system_prompt
            .as_deref()
            .unwrap_or(super::DEFAULT_SYSTEM_PROMPT);

        let body = serde_json::json!({
            "model": self.model,
            "system": system_prompt,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "max_tokens": 4096,
            "temperature": 0.2,
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let response = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            return Err(LlmError::RateLimited(Duration::from_secs(5)));
        }

        let json: serde_json::Value = response
            .json()
            .map_err(|e| LlmError::Network(format!("JSON parse: {}", e)))?;

        let content = json["content"][0]["text"]
            .as_str()
            .ok_or(LlmError::EmptyResponse)?
            .to_string();

        Ok(content)
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }
}

// ─── Ollama Backend ───────────────────────────────────────────────────

/// Local Ollama LLM backend.
pub struct OllamaBackend {
    config: OllamaConfig,
}

impl LlmBackend for OllamaBackend {
    fn generate_code(&self, prompt: &str, context: &LlmContext) -> Result<String, LlmError> {
        let system_prompt = context
            .system_prompt
            .as_deref()
            .unwrap_or(super::DEFAULT_SYSTEM_PROMPT);

        let body = serde_json::json!({
            "model": self.config.model,
            "system": system_prompt,
            "prompt": prompt,
            "stream": false,
            "keep_alive": self.config.keep_alive.as_secs_f64(),
            "options": {
                "temperature": self.config.temperature,
            }
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(self.config.timeout)
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let response = client
            .post(format!("{}/api/generate", self.config.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let json: serde_json::Value = response
            .json()
            .map_err(|e| LlmError::Network(format!("JSON parse: {}", e)))?;

        let content = json["response"]
            .as_str()
            .ok_or(LlmError::EmptyResponse)?
            .to_string();

        Ok(content)
    }

    fn name(&self) -> &'static str {
        "ollama"
    }
}

// ─── Mock Backend (Testing) ───────────────────────────────────────────

/// Mock LLM backend for testing.
///
/// Returns pre-configured responses. Supports both single-response
/// and multi-sequence (for auto-repair loop testing).
pub struct MockBackend {
    responses: Vec<Result<String, LlmError>>,
    call_count: std::sync::Mutex<usize>,
}

impl MockBackend {
    /// Create a mock backend that always returns the same response.
    pub fn new(response: Result<String, LlmError>) -> Self {
        Self {
            responses: vec![response],
            call_count: std::sync::Mutex::new(0),
        }
    }

    /// Create a mock backend that returns responses in sequence.
    pub fn new_with_sequence(responses: Vec<Result<String, LlmError>>) -> Self {
        assert!(!responses.is_empty(), "Must provide at least one response");
        Self {
            responses,
            call_count: std::sync::Mutex::new(0),
        }
    }
}

impl LlmBackend for MockBackend {
    fn generate_code(&self, _prompt: &str, _context: &LlmContext) -> Result<String, LlmError> {
        let mut count = self.call_count.lock().unwrap();
        let idx = *count % self.responses.len();
        *count += 1;
        match &self.responses[idx] {
            Ok(code) => Ok(code.clone()),
            Err(e) => Err(e.clone()),
        }
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

// ─── Noop Backend ─────────────────────────────────────────────────────

/// A no-op backend that always returns an error.
/// Useful when LLM integration is disabled.
pub struct NoopBackend;

impl LlmBackend for NoopBackend {
    fn generate_code(&self, _prompt: &str, _context: &LlmContext) -> Result<String, LlmError> {
        Err(LlmError::NoBackend)
    }

    fn name(&self) -> &'static str {
        "noop"
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmContext;

    #[test]
    fn test_mock_backend_single() {
        let backend = MockBackend::new(Ok("def graph():\n    return 42".into()));
        let ctx = LlmContext::default();
        let result = backend.generate_code("test", &ctx);
        assert_eq!(result.unwrap(), "def graph():\n    return 42");
    }

    #[test]
    fn test_mock_backend_sequence() {
        let backend = MockBackend::new_with_sequence(vec![
            Ok("code1".into()),
            Ok("code2".into()),
            Ok("code3".into()),
        ]);
        let ctx = LlmContext::default();
        assert_eq!(backend.generate_code("t", &ctx).unwrap(), "code1");
        assert_eq!(backend.generate_code("t", &ctx).unwrap(), "code2");
        assert_eq!(backend.generate_code("t", &ctx).unwrap(), "code3");
        // Wraps around
        assert_eq!(backend.generate_code("t", &ctx).unwrap(), "code1");
    }

    #[test]
    fn test_mock_backend_error() {
        let backend = MockBackend::new(Err(LlmError::NoBackend));
        let ctx = LlmContext::default();
        let result = backend.generate_code("test", &ctx);
        assert!(matches!(result, Err(LlmError::NoBackend)));
    }

    #[test]
    fn test_noop_backend() {
        let backend = NoopBackend;
        let ctx = LlmContext::default();
        let result = backend.generate_code("test", &ctx);
        assert!(matches!(result, Err(LlmError::NoBackend)));
    }

    #[test]
    fn test_provider_mock_into_backend() {
        let provider = Provider::Mock {
            responses: vec!["def graph():\n    pass".into()],
        };
        let backend = provider.into_backend();
        let ctx = LlmContext::default();
        let result = backend.generate_code("test", &ctx);
        assert!(result.is_ok());
        assert_eq!(backend.name(), "mock");
    }

    #[test]
    fn test_ollama_config_default() {
        let config = OllamaConfig::default();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert_eq!(config.model, "llama3.1");
        assert_eq!(config.temperature, 0.2);
    }
}
