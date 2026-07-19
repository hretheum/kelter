//! API contract tests against a kernel wired with mocks — no model, no tools.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, ValidatedAction,
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
    async fn execute(
        &self,
        a: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
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
    ) -> Result<Proposal, PolicyError> {
        if context.steps.is_empty() {
            Ok(ProposedAction {
                action: "echo".into(),
                arguments: json!({"message": "hi"}),
                rationale: None,
            }
            .into())
        } else {
            Ok(ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "echoed"}),
                rationale: None,
            }
            .into())
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

// ---- A2A inbound (ADR 0013) ----------------------------------------------

#[tokio::test]
async fn a2a_card_is_public_and_declares_bearer() {
    let (addr, _) = spawn_server().await;
    // No auth: card discovery must be reachable so a client learns the scheme.
    let card: serde_json::Value =
        reqwest::get(format!("http://{addr}/.well-known/agent-card.json"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(card["securitySchemes"]["bearer"]["scheme"], "bearer");
    assert_eq!(card["security"][0]["bearer"], json!([]));
    // The advertised interface points back at this origin's /a2a/rpc, in both
    // the v1.0 and legacy field names.
    let rpc = format!("http://{addr}/a2a/rpc");
    assert_eq!(card["interfaces"][0]["url"], rpc);
    assert_eq!(card["supportedInterfaces"][0]["url"], rpc);
}

#[tokio::test]
async fn repeated_bad_auth_is_rate_limited() {
    // Item F (Phase 8): after enough failed auth attempts the endpoint returns
    // 429 instead of 401 — a basic brute-force slowdown.
    let (addr, _) = spawn_server().await;
    let client = reqwest::Client::new();
    let mut saw_429 = false;
    for _ in 0..15 {
        let res = client
            .get(format!("http://{addr}/api/v1/runs"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        if res.status() == 429 {
            saw_429 = true;
            break;
        }
        assert_eq!(res.status(), 401);
    }
    assert!(saw_429, "expected a 429 after repeated failed auth");
}

#[tokio::test]
async fn a2a_rpc_rejects_anonymous() {
    let (addr, _) = spawn_server().await;
    let client = reqwest::Client::new();
    // Anonymous task call: never reaches a method — the security surface.
    let res = client
        .post(format!("http://{addr}/a2a/rpc"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"SendMessage",
                      "params":{"message":{"parts":[{"text":"hi"}]}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

async fn a2a_send(
    client: &reqwest::Client,
    addr: &SocketAddr,
    token: &str,
    method: &str,
) -> String {
    let res: serde_json::Value = client
        .post(format!("http://{addr}/a2a/rpc"))
        .bearer_auth(token)
        .json(&json!({"jsonrpc":"2.0","id":"x","method":method,
                      "params":{"message":{"parts":[{"text":"echo hi"}]}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    res["result"]["task"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a2a_message_send_creates_run_and_get_polls_to_completion() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();

    // Both the v1.0 (`SendMessage`) and legacy (`message/send`) names work.
    let id_v1 = a2a_send(&client, &addr, &token, "SendMessage").await;
    let id_legacy = a2a_send(&client, &addr, &token, "message/send").await;
    assert_ne!(id_v1, id_legacy);

    for task_id in [id_v1, id_legacy] {
        let mut state = String::new();
        for _ in 0..50 {
            let r: serde_json::Value = client
                .post(format!("http://{addr}/a2a/rpc"))
                .bearer_auth(&token)
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"tasks/get",
                              "params":{"id": task_id}}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            state = r["result"]["status"]["state"].as_str().unwrap().to_string();
            if state == "TASK_STATE_COMPLETED" || state == "TASK_STATE_FAILED" {
                // A completed task carries its answer as an artifact part.
                assert!(r["result"]["artifacts"][0]["parts"][0]["text"].is_string());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(state, "TASK_STATE_COMPLETED");
    }
}

#[tokio::test]
async fn a2a_get_and_cancel_unknown_task_is_not_found() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();
    let unknown = uuid::Uuid::new_v4();
    for method in ["tasks/get", "tasks/cancel"] {
        let r: serde_json::Value = client
            .post(format!("http://{addr}/a2a/rpc"))
            .bearer_auth(&token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,
                          "params":{"id": unknown.to_string()}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["error"]["code"], -32001, "method {method}");
    }
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
    assert!(body.contains("<title>kelter</title>"));
}
