//! `math.mul` — iki sayıyı çarpar.

use std::collections::HashMap;

use tinypipe_api::types::Value;

use crate::mock::MockToolRegistry;

pub fn register(reg: &MockToolRegistry) {
    reg.add("math.mul", math_mul);
}

fn math_mul(args: &[Value], _kwargs: &HashMap<String, Value>, _env: &tinypipe_env::Env) -> Result<Value, String> {
    if args.len() < 2 {
        return Err("math.mul requires 2 args".into());
    }
    let a = args[0].as_f64().ok_or("arg0 not a number")?;
    let b = args[1].as_f64().ok_or("arg1 not a number")?;
    Ok(Value::Float(a * b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::tool_registry::ToolRegistry;
    use tinypipe_api::types::CallTarget;

    #[test]
    fn test_math_mul() {
        let reg = MockToolRegistry::new();
        register(&reg);
        let mut ct = CallTarget::new("math.mul");
        ct.args.push(Value::Float(3.0));
        ct.args.push(Value::Float(4.0));
        let result = reg
            .dispatch(&ct, &tinypipe_api::types::Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(result, Value::Float(12.0));
    }
}
