//! `MockToolRegistry` — `ToolRegistry` trait'inin in-memory implementasyonu.
//!
//! Varsayılan tool seti için `tools::default_tools()` / `mock_tools()`'a bakın.
//!
//! Kullanım:
//! ```rust
//! # use tinypipe_tools::MockToolRegistry;
//! # use tinypipe_api::types::{Value, CallTarget, Context};
//! # use tinypipe_api::tool_registry::ToolRegistry;
//! let reg = MockToolRegistry::new();
//! reg.add("math.add", |args, _kwargs, _env| {
//!     let a = args[0].as_f64().ok_or("not a number")?;
//!     let b = args[1].as_f64().ok_or("not a number")?;
//!     Ok(Value::Float(a + b))
//! });
//! let mut ct = CallTarget::new("math.add");
//! ct.args.push(Value::Float(3.0));
//! ct.args.push(Value::Float(4.0));
//! let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty());
//! assert!(result.is_ok());
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

use tinypipe_api::tool_registry::{SubgraphResult, ToolRegistry};
use tinypipe_api::types::{CallTarget, Context, DispatchError, RegistryError, ToolSpec, Value};
use tinypipe_env::Env;

/// Tool fonksiyonu: positional args + keyword args (VM `call(...)` çağrıları
/// kwargs üretir; `dispatch` ikisini de iletir) + çözülmüş ortam görünümü.
/// Env'ten bağımsız tool'lar `_env` ile yoksayar.
pub type ToolFn =
    Box<dyn Fn(&[Value], &HashMap<String, Value>, &Env) -> Result<Value, String> + Send + Sync>;

pub struct MockTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub pure: bool,
    pub exec: ToolFn,
}

pub struct MockToolRegistry {
    tools: Mutex<HashMap<String, MockTool>>,
}

impl MockToolRegistry {
    pub fn new() -> Self {
        MockToolRegistry {
            tools: Mutex::new(HashMap::new()),
        }
    }

    pub fn add<F>(&self, name: &str, exec: F)
    where
        F: Fn(&[Value], &HashMap<String, Value>, &Env) -> Result<Value, String> + Send + Sync + 'static,
    {
        let mut tools = self.tools.lock().unwrap();
        tools.insert(
            name.to_owned(),
            MockTool {
                name: name.to_owned(),
                description: String::new(),
                input_schema: serde_json::Value::Null,
                output_schema: serde_json::Value::Null,
                pure: false,
                exec: Box::new(exec),
            },
        );
    }

    pub fn add_with_schema<F>(
        &self,
        name: &str,
        input_schema: serde_json::Value,
        output_schema: serde_json::Value,
        pure: bool,
        exec: F,
    ) where
        F: Fn(&[Value], &HashMap<String, Value>, &Env) -> Result<Value, String>
            + Send
            + Sync
            + 'static,
    {
        let mut tools = self.tools.lock().unwrap();
        tools.insert(
            name.to_owned(),
            MockTool {
                name: name.to_owned(),
                description: String::new(),
                input_schema,
                output_schema,
                pure,
                exec: Box::new(exec),
            },
        );
    }

    pub fn clear(&self) {
        self.tools.lock().unwrap().clear();
    }

    pub fn tool_count(&self) -> usize {
        self.tools.lock().unwrap().len()
    }

    /// Kayıtlı tool isimlerini döndürür.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }
}

impl Default for MockToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry for MockToolRegistry {
    fn resolve(&self, name: &str, _version: &str) -> Result<ToolSpec, RegistryError> {
        let tools = self.tools.lock().unwrap();
        tools
            .get(name)
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
                output_schema: t.output_schema.clone(),
                pure: t.pure,
                version: "0.0.0".into(),
                schema_hash: "mock".into(),
            })
            .ok_or_else(|| RegistryError::NotFound(name.into()))
    }

    fn dispatch(
        &self,
        call: &CallTarget,
        _context: &Context,
        env: &Env,
    ) -> Result<Value, DispatchError> {
        let tools = self.tools.lock().unwrap();
        let tool = tools
            .get(&call.name)
            .ok_or_else(|| DispatchError::NotFound(call.name.clone()))?;
        (tool.exec)(&call.args, &call.kwargs, env).map_err(DispatchError::ExecutionFailed)
    }

    fn execute_subgraph(
        &self,
        name: &str,
        _input: Context,
        _env: &Env,
    ) -> Result<SubgraphResult, DispatchError> {
        if name.contains("echo") {
            let tools = self.tools.lock().unwrap();
            let tool = tools
                .get("echo")
                .ok_or_else(|| DispatchError::NotFound("echo".into()))?;
            let result = (tool.exec)(&[Value::String("echo!".into())], &HashMap::new(), _env)
                .map_err(DispatchError::ExecutionFailed)?;
            let mut ctx = Context::new();
            ctx.set("output".into(), result.clone());
            Ok(SubgraphResult {
                context: ctx,
                output: result,
            })
        } else {
            Err(DispatchError::NotFound(name.into()))
        }
    }

    fn latest_schema_hash(&self, _name: &str) -> Result<String, RegistryError> {
        Ok("mock".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_registry_empty() {
        let reg = MockToolRegistry::new();
        assert!(reg.resolve("nonexistent", "1.0").is_err());
    }

    #[test]
    fn test_mock_registry_add_and_resolve() {
        let reg = MockToolRegistry::new();
        reg.add("math.add", |args, _kwargs, _env| {
            let a = args[0].as_f64().unwrap();
            let b = args[1].as_f64().unwrap();
            Ok(Value::Float(a + b))
        });

        let spec = reg.resolve("math.add", "1.0").unwrap();
        assert_eq!(spec.name, "math.add");
    }

    #[test]
    fn test_mock_registry_dispatch() {
        let reg = MockToolRegistry::new();
        reg.add("echo", |args, _kwargs, _env| {
            args.first().cloned().ok_or("no args".into())
        });

        let mut ct = CallTarget::new("echo");
        ct.args.push(Value::Int(42));
        let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_mock_registry_dispatch_kwargs() {
        let reg = MockToolRegistry::new();
        reg.add("kw", |_args, kwargs, _env| {
            kwargs.get("key").cloned().ok_or("missing key".into())
        });

        let mut ct = CallTarget::new("kw");
        ct.kwargs.insert("key".into(), Value::String("v".into()));
        let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(result, Value::String("v".into()));
    }

    #[test]
    fn test_mock_registry_dispatch_error() {
        let reg = MockToolRegistry::new();
        reg.add("failing", |_, _, _env| Err("always fails".into()));

        let ct = CallTarget::new("failing");
        let result = reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty());
        assert!(result.is_err());
    }
}
