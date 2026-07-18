//! The OpenRouter leg on mocked HTTP: a local server stands in for
//! OpenRouter, asserts the request contract (auth header, tools payload),
//! and returns a canned native tool call. Runs with no key and no network.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{ActionSchema, ActionSpec, Latitude, PolicyContext, PolicyOracle};
use policy::OpenRouterAdapter;

#[derive(Default)]
struct Captured {
    auth: Option<String>,
    body: Option<serde_json::Value>,
}

async fn mock_endpoint(
    State(captured): State<Arc<Mutex<Captured>>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut c = captured.lock().await;
    c.auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    c.body = Some(body);
    // Canned response in the documented shape: function.arguments is a
    // JSON-encoded STRING.
    Json(json!({
        "id": "gen-123",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_source",
                        "arguments": "{\"input\": \"https://example.org\", \"save_as\": \"raw.html\"}"
                    }
                }]
            }
        }]
    }))
}

fn schema() -> ActionSchema {
    ActionSchema {
        actions: vec![ActionSpec {
            name: "read_source".into(),
            description: "Read a URL or file".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"},
                    "save_as": {"type": "string"}
                },
                "required": ["input", "save_as"],
                "additionalProperties": false
            }),
        }],
    }
}

fn context() -> PolicyContext {
    PolicyContext {
        task: "read example.org".into(),
        steps: vec![],
        memory: vec![],
        last_error: None,
        latitude: Latitude::Wide,
        guidance: None,
    }
}

#[tokio::test]
async fn openrouter_adapter_speaks_the_documented_contract() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_endpoint))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let adapter = OpenRouterAdapter::new(
        format!("http://{addr}"),
        "anthropic/claude-sonnet-5",
        "sk-or-test-not-a-real-key",
    );

    let proposed = adapter.propose(&context(), &schema()).await.unwrap().action;

    // The adapter parsed the JSON-encoded arguments string into a value.
    assert_eq!(proposed.action, "read_source");
    assert_eq!(proposed.arguments["input"], "https://example.org");
    assert_eq!(proposed.arguments["save_as"], "raw.html");

    // And the kernel-side validation accepts it (full contract round-trip).
    assert!(schema().validate(proposed).is_ok());

    // Request contract assertions.
    let c = captured.lock().await;
    assert_eq!(
        c.auth.as_deref(),
        Some("Bearer sk-or-test-not-a-real-key"),
        "auth must be the documented bearer header"
    );
    let body = c.body.as_ref().unwrap();
    assert_eq!(body["model"], "anthropic/claude-sonnet-5");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read_source");
    assert!(body["messages"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn openrouter_adapter_fails_loudly_on_http_error() {
    // Point at a closed port: the adapter must return a backend error,
    // never a fabricated action.
    let adapter = OpenRouterAdapter::new("http://127.0.0.1:1", "x/y", "sk-or-test");
    let err = adapter.propose(&context(), &schema()).await.unwrap_err();
    assert!(err.to_string().contains("unreachable"));
}
