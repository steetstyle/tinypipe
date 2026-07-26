//! Mock ToolRegistry — test'lerde kullanılacak mock implementasyonları.
//!
//! Kullanım:
//! ```rust
//! # use tinypipe_vm::MockToolRegistry;
//! # use tinypipe_api::types::{Value, CallTarget, Context};
//! # use tinypipe_api::tool_registry::ToolRegistry;
//! let reg = MockToolRegistry::new();
//! reg.add("math.add", |args| {
//!     let a = args[0].as_f64().ok_or("not a number")?;
//!     let b = args[1].as_f64().ok_or("not a number")?;
//!     Ok(Value::Float(a + b))
//! });
//! let mut ct = CallTarget::new("math.add");
//! ct.args.push(Value::Float(3.0));
//! ct.args.push(Value::Float(4.0));
//! let result = reg.dispatch(&ct, &Context::new());
//! assert!(result.is_ok());
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

use tinypipe_api::types::{
    CallTarget, Context, DispatchError, RegistryError, ToolSpec, Value,
};
use tinypipe_api::tool_registry::ToolRegistry;

type ToolFn = Box<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

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
        MockToolRegistry { tools: Mutex::new(HashMap::new()) }
    }

    pub fn add<F>(&self, name: &str, exec: F)
    where
        F: Fn(&[Value]) -> Result<Value, String> + Send + Sync + 'static,
    {
        let mut tools = self.tools.lock().unwrap();
        tools.insert(name.to_owned(), MockTool {
            name: name.to_owned(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
            pure: false,
            exec: Box::new(exec),
        });
    }

    pub fn add_with_schema<F>(&self, name: &str, input_schema: serde_json::Value,
                                output_schema: serde_json::Value, pure: bool, exec: F)
    where
        F: Fn(&[Value]) -> Result<Value, String> + Send + Sync + 'static,
    {
        let mut tools = self.tools.lock().unwrap();
        tools.insert(name.to_owned(), MockTool {
            name: name.to_owned(),
            description: String::new(),
            input_schema,
            output_schema,
            pure,
            exec: Box::new(exec),
        });
    }

    pub fn clear(&self) {
        self.tools.lock().unwrap().clear();
    }

    pub fn tool_count(&self) -> usize {
        self.tools.lock().unwrap().len()
    }
}

impl Default for MockToolRegistry {
    fn default() -> Self { Self::new() }
}

impl ToolRegistry for MockToolRegistry {
    fn resolve(&self, name: &str, _version: &str) -> Result<ToolSpec, RegistryError> {
        let tools = self.tools.lock().unwrap();
        tools.get(name).map(|t| ToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            output_schema: t.output_schema.clone(),
            pure: t.pure,
            version: "0.0.0".into(),
            schema_hash: "mock".into(),
        }).ok_or_else(|| RegistryError::NotFound(name.into()))
    }

    fn dispatch(&self, call: &CallTarget, _context: &Context) -> Result<Value, DispatchError> {
        let tools = self.tools.lock().unwrap();
        let tool = tools.get(&call.name)
            .ok_or_else(|| DispatchError::NotFound(call.name.clone()))?;
        (tool.exec)(&call.args).map_err(|e| DispatchError::ExecutionFailed(e))
    }

    fn execute_subgraph(&self, name: &str, _input: Context) -> Result<Context, DispatchError> {
        if name.contains("echo") {
            let tools = self.tools.lock().unwrap();
            let tool = tools.get("echo")
                .ok_or_else(|| DispatchError::NotFound("echo".into()))?;
            let result = (tool.exec)(&[]).map_err(|e| DispatchError::ExecutionFailed(e))?;
            let mut ctx = Context::new();
            ctx.set("output".into(), result);
            Ok(ctx)
        } else {
            Err(DispatchError::NotFound(name.into()))
        }
    }

    fn latest_schema_hash(&self, _name: &str) -> Result<String, RegistryError> {
        Ok("mock".into())
    }
}

// ============ Default mock tools factory ============

/// Varsayılan mock tool'ları içeren bir `MockToolRegistry` döndürür.
pub fn mock_tools() -> MockToolRegistry {
    let reg = MockToolRegistry::new();

    reg.add("math.add", |args| {
        if args.len() < 2 { return Err("math.add requires 2 args".into()); }
        let a = args[0].as_f64().ok_or("arg0 not a number")?;
        let b = args[1].as_f64().ok_or("arg1 not a number")?;
        Ok(Value::Float(a + b))
    });

    reg.add("math.mul", |args| {
        if args.len() < 2 { return Err("math.mul requires 2 args".into()); }
        let a = args[0].as_f64().ok_or("arg0 not a number")?;
        let b = args[1].as_f64().ok_or("arg1 not a number")?;
        Ok(Value::Float(a * b))
    });

    reg.add("string.len", |args| {
        let s = args[0].as_str().ok_or("arg0 not a string")?;
        Ok(Value::Int(s.len() as i64))
    });

    reg.add("echo", |args| {
        args.first().cloned().ok_or_else(|| "echo requires 1 arg".into())
    });

    reg.add("test.sleep", |args| {
        let ms = args[0].as_u64().ok_or("arg0 not a u64")?;
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(Value::Null)
    });

    reg.add("test.error", |_args| {
        Err("simulated tool error".into())
    });

    reg.add("test.large", |args| {
        let n = args[0].as_u64().ok_or("arg0 not a u64")? as usize;
        Ok(Value::String("x".repeat(n)))
    });

    reg
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
        reg.add("math.add", |args| {
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
        reg.add("echo", |args| args.first().cloned().ok_or("no args".into()));

        let mut ct = CallTarget::new("echo");
        ct.args.push(Value::Int(42));
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_mock_registry_dispatch_error() {
        let reg = MockToolRegistry::new();
        reg.add("failing", |_| Err("always fails".into()));

        let ct = CallTarget::new("failing");
        let result = reg.dispatch(&ct, &Context::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_tools_factory() {
        let reg = mock_tools();
        assert_eq!(reg.tool_count(), 7);
    }

    #[test]
    fn test_math_add() {
        let reg = mock_tools();
        let mut ct = CallTarget::new("math.add");
        ct.args.push(Value::Float(3.0));
        ct.args.push(Value::Float(4.0));
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        assert_eq!(result, Value::Float(7.0));
    }

    #[test]
    fn test_string_len() {
        let reg = mock_tools();
        let mut ct = CallTarget::new("string.len");
        ct.args.push(Value::String("hello".into()));
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn test_echo_passthrough() {
        let reg = mock_tools();
        let mut ct = CallTarget::new("echo");
        ct.args.push(Value::String("passthrough".into()));
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        assert_eq!(result, Value::String("passthrough".into()));
    }

    #[test]
    fn test_sleep_tool() {
        let reg = mock_tools();
        let mut ct = CallTarget::new("test.sleep");
        ct.args.push(Value::Int(1)); // 1ms
        let start = std::time::Instant::now();
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() >= 1);
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_error_tool() {
        let reg = mock_tools();
        let ct = CallTarget::new("test.error");
        let result = reg.dispatch(&ct, &Context::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_large_tool_memory() {
        let reg = mock_tools();
        let mut ct = CallTarget::new("test.large");
        ct.args.push(Value::Int(100));
        let result = reg.dispatch(&ct, &Context::new()).unwrap();
        if let Value::String(s) = result {
            assert_eq!(s.len(), 100);
        } else {
            panic!("expected string");
        }
    }
}
