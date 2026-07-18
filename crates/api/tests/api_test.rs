//! API contract tests against a kernel wired with mocks — no model, no tools.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionError, Kernel, KernelConfig, Latitude,
    PolicyContext, PolicyError, PolicyOracle, ProposedAction, ValidatedAction,
};
use memory::InMemoryProvider;

struct EchoTools;

#[async_trait]
impl ActionSource for EchoTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "Echo".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
                    "additionalProperties": false
                }),
            }],
        })
    }
    async fn execute(&self, a: &ValidatedAction) -> Result<serde_json::Value, ExecutionError> {
        Ok(json!({"echoed": a.arguments()["message"]}))
    }
}

struct OneShotOracle;

#[async_trait]
impl PolicyOracle for OneShotOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<ProposedAction, PolicyError> {
        if context.steps.is_empty() {
            Ok(ProposedAction {
                action: "echo".into(),
                arguments: json!({"message": "hi"}),
                rationale: None,
            })
        } else {
            Ok(ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "echoed"}),
                rationale: None,
            })
        }
    }
    fn describe(&self) -> String {
        "mock:oneshot".into()
    }
}

async fn spawn_server() -> (SocketAddr, String) {
    let kernel = Arc::new(Kernel::new(
        Arc::new(OneShotOracle),
        Arc::new(EchoTools),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    ));
    let token = "test-token-123".to_string();
    let app = api::router(kernel, token.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, token)
}

#[tokio::test]
async fn health_needs_no_auth() {
    let (addr, _) = spawn_server().await;
    let res = reqwest::get(format!("http://{addr}/healthz"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn api_rejects_bad_token() {
    let (addr, _) = spawn_server().await;
    let client = reqwest::Client::new();
    let res = client
        .get(format!("http://{addr}/api/v1/runs"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = client
        .get(format!("http://{addr}/api/v1/runs"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn submit_run_and_poll_to_completion() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();

    let res = client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .json(&json!({"task": "echo hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    let run_id = res.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut status = String::new();
    for _ in 0..50 {
        let run: serde_json::Value = client
            .get(format!("http://{addr}/api/v1/runs/{run_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        status = run["status"].as_str().unwrap().to_string();
        if status == "completed" || status == "failed" {
            assert_eq!(run["steps"][0]["action"], "echo");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(status, "completed");
}

#[tokio::test]
async fn serves_embedded_ui() {
    let (addr, _) = spawn_server().await;
    let body = reqwest::get(format!("http://{addr}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("<title>agentkernel</title>"));
}
