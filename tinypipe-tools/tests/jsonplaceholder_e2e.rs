//! Gerçek dış API'ye karşı e2e test'ler — https://jsonplaceholder.typicode.com
//!
//! `http_request` tool'u gerçek ağ üzerinden doğrulanır:
//!
//! ```bash
//! cargo test -p tinypipe-tools --test jsonplaceholder_e2e -- --ignored
//! ```
//!
//! Ağ gerektirdiğinden varsayılan test koşusunda atlanır (`#[ignore]`).

use tinypipe_api::tool_registry::ToolRegistry;
use tinypipe_api::types::{CallTarget, Context, Value};
use tinypipe_tools::default_tools;

fn dispatch(target: &str, kwargs: &[(&str, Value)]) -> Result<Value, String> {
    let reg = default_tools();
    let mut ct = CallTarget::new(target);
    for (k, v) in kwargs {
        ct.kwargs.insert(k.to_string(), v.clone());
    }
    reg.dispatch(&ct, &Context::new(), &tinypipe_env::Env::empty())
        .map_err(|e| e.to_string())
}

fn get_body(result: Value) -> String {
    let Value::Object(m) = result else {
        panic!("expected object result, got {:?}", result);
    };
    match m.get("body") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected body string, got {:?}", other),
    }
}

fn get_status(result: Value) -> i64 {
    let Value::Object(m) = result else {
        panic!("expected object result, got {:?}", result);
    };
    match m.get("status") {
        Some(Value::Int(i)) => *i,
        other => panic!("expected status int, got {:?}", other),
    }
}

#[test]
#[ignore]
fn get_post_returns_200_with_valid_json() {
    let result = dispatch(
        "http_request",
        &[("url", Value::String("https://jsonplaceholder.typicode.com/posts/1".into()))],
    )
    .expect("http_request should succeed");

    assert_eq!(get_status(result.clone()), 200);
    let body = get_body(result);
    let json: serde_json::Value =
        serde_json::from_str(&body).expect("response body should be valid JSON");
    assert_eq!(json["id"], 1);
    assert!(json["title"].is_string());
    assert!(json["title"].as_str().unwrap().len() > 0);
}

#[test]
#[ignore]
fn post_creates_resource_with_201() {
    let result = dispatch(
        "http_request",
        &[
            ("url", Value::String("https://jsonplaceholder.typicode.com/posts".into())),
            ("method", Value::String("POST".into())),
            (
                "body",
                Value::String("{\"title\": \"tinypipe e2e\", \"body\": \"test\", \"userId\": 1}".into()),
            ),
            (
                "headers",
                Value::Object(
                    [("content-type".to_string(), Value::String("application/json".into()))]
                        .into_iter()
                        .collect(),
                ),
            ),
        ],
    )
    .expect("http_request should succeed");

    assert_eq!(get_status(result.clone()), 201);
    let json: serde_json::Value =
        serde_json::from_str(&get_body(result)).expect("response body should be valid JSON");
    assert_eq!(json["title"], "tinypipe e2e");
    assert!(json["id"].is_number());
}

#[test]
#[ignore]
fn get_user_returns_array() {
    let result = dispatch(
        "http_request",
        &[(
            "url",
            Value::String("https://jsonplaceholder.typicode.com/users".into()),
        )],
    )
    .expect("http_request should succeed");

    assert_eq!(get_status(result.clone()), 200);
    let json: serde_json::Value =
        serde_json::from_str(&get_body(result)).expect("response body should be valid JSON");
    let users = json.as_array().expect("expected array of users");
    assert!(users.len() >= 10);
    assert!(users.iter().any(|u| u["username"].is_string()));
}
