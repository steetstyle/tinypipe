//! `test.sleep` — N milisaniye bekler (test/bench yardımcısı).

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("test.sleep", test_sleep);
}

fn test_sleep(args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    let ms = args[0].as_u64().ok_or("arg0 not a u64")?;
    std::thread::sleep(std::time::Duration::from_millis(ms));
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_sleep_tool() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("test.sleep");
        ct.args.push(Value::Int(1)); // 1ms
        let start = std::time::Instant::now();
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() >= 1);
        assert_eq!(result, Value::Null);
    }
}
