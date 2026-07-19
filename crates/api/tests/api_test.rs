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

// ---- Phase 11 security: principals + WS auth -----------------------------

async fn spawn_with_principals() -> (SocketAddr, String, String, Arc<dyn kernel_core::RunStore>) {
    let store: Arc<dyn kernel_core::RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let kernel = Arc::new(Kernel::new(
        Arc::new(OneShotOracle),
        Arc::new(EchoTools),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig::default(),
    ));
    let default_token = "default-token".to_string();
    let alice_token = "alice-token".to_string();
    let principals = api::Principals::with_named(
        default_token.clone(),
        vec![("alice".to_string(), alice_token.clone())],
    );
    let app = api::router_with_principals(kernel, principals, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, default_token, alice_token, store)
}

async fn submit_and_wait_journal(
    addr: &SocketAddr,
    token: &str,
    store: &Arc<dyn kernel_core::RunStore>,
) -> String {
    let client = reqwest::Client::new();
    let run_id = client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(token)
        .json(&json!({"task": "echo hi"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The principal Note is journaled synchronously in create_run; give the
    // spawned journal write a moment.
    let id = uuid::Uuid::parse_str(&run_id).unwrap();
    for _ in 0..50 {
        let j = store.journal(id).await.unwrap();
        if j.iter()
            .any(|e| serde_json::to_value(e).unwrap()["key"] == "principal")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    run_id
}

fn journal_principal(entries: &[kernel_core::JournalEntry]) -> Option<String> {
    for e in entries {
        let v = serde_json::to_value(e).unwrap();
        if v["key"] == "principal" {
            return v["value"]["principal"].as_str().map(String::from);
        }
    }
    None
}

#[tokio::test]
async fn two_tokens_get_distinct_per_run_principal_attribution() {
    let (addr, default_token, alice_token, store) = spawn_with_principals().await;

    let alice_run = submit_and_wait_journal(&addr, &alice_token, &store).await;
    let default_run = submit_and_wait_journal(&addr, &default_token, &store).await;

    let a = store
        .journal(uuid::Uuid::parse_str(&alice_run).unwrap())
        .await
        .unwrap();
    let d = store
        .journal(uuid::Uuid::parse_str(&default_run).unwrap())
        .await
        .unwrap();
    assert_eq!(journal_principal(&a).as_deref(), Some("alice"));
    assert_eq!(journal_principal(&d).as_deref(), Some("default"));

    // An unknown token is still rejected (no principal).
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth("not-a-principal")
        .json(&json!({"task": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn ws_events_require_a_valid_token() {
    let (addr, token) = spawn_server().await;

    // No token → the WebSocket handshake is refused before upgrade.
    let no_token = tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/events")).await;
    assert!(no_token.is_err(), "WS without a token must be refused");

    // A wrong token → refused.
    let bad =
        tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/events?token=wrong")).await;
    assert!(bad.is_err(), "WS with a wrong token must be refused");

    // A valid query token → connects.
    let ok =
        tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/events?token={token}")).await;
    assert!(ok.is_ok(), "WS with a valid token must connect");
}

#[tokio::test]
async fn config_endpoint_is_bearer_gated_and_optional() {
    // Phase 8.5: with no controller wired (the bare `router`), the config
    // endpoint reports "not implemented" for an authed caller, and still
    // rejects an anonymous one first.
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();

    let anon = client
        .get(format!("http://{addr}/api/v1/config/model"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let authed = client
        .get(format!("http://{addr}/api/v1/config/model"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 501);
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
    // The embedded UI is the Vite build (design/06 §5). If this fails with
    // "web-ui not built", run `pnpm --dir apps/web-ui build` first.
    assert!(body.contains("<title>Kelter</title>"), "{body}");
}

// ---- web-UI redesign additive endpoints (design/07 D-1 + D-2) -------------

/// An oracle that never answers — a run driven by it stays Running until
/// canceled, which is exactly what the cancel test needs.
struct HangOracle;

#[async_trait]
impl PolicyOracle for HangOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        std::future::pending::<()>().await;
        unreachable!()
    }
    fn describe(&self) -> String {
        "mock:hang".into()
    }
}

async fn spawn_server_with_oracle(oracle: Arc<dyn PolicyOracle>) -> (SocketAddr, String) {
    let kernel = Arc::new(Kernel::new(
        oracle,
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
async fn run_events_replays_the_full_ordered_journal() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();

    // Both new endpoints are bearer-gated: no token → 401.
    let res = client
        .get(format!(
            "http://{addr}/api/v1/runs/00000000-0000-0000-0000-000000000000/events"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // Unknown run → 404 (authenticated).
    let res = client
        .get(format!(
            "http://{addr}/api/v1/runs/00000000-0000-0000-0000-000000000000/events"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // Run to completion, then replay the journal.
    let run_id = client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .json(&json!({"task": "echo for replay"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut body = serde_json::Value::Null;
    for _ in 0..50 {
        body = client
            .get(format!("http://{addr}/api/v1/runs/{run_id}/events"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if body["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(body["status"], "completed", "{body}");
    assert_eq!(body["task"], "echo for replay");
    let journal = body["journal"].as_array().unwrap();
    let kinds: Vec<&str> = journal
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    // Append order: the echo step's decision precedes its receipt, and the
    // terminal record is last.
    let d = kinds.iter().position(|k| *k == "decision").unwrap();
    let r = kinds.iter().position(|k| *k == "receipt").unwrap();
    assert!(d < r, "decision must precede receipt: {kinds:?}");
    assert_eq!(*kinds.last().unwrap(), "terminal", "{kinds:?}");
    assert_eq!(journal[d]["action"], "echo");
    assert_eq!(journal[r]["ok"], true);
}

#[tokio::test]
async fn cancel_stops_a_live_run_and_writes_canceled() {
    let (addr, token) = spawn_server_with_oracle(Arc::new(HangOracle)).await;
    let client = reqwest::Client::new();

    // Bearer-gated: no token → 401.
    let res = client
        .post(format!(
            "http://{addr}/api/v1/runs/00000000-0000-0000-0000-000000000000/cancel"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // Unknown run → 404.
    let res = client
        .post(format!(
            "http://{addr}/api/v1/runs/00000000-0000-0000-0000-000000000000/cancel"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // A run against the hanging oracle stays live until canceled.
    let run_id = client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .json(&json!({"task": "hang forever"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let run: serde_json::Value = client
        .post(format!("http://{addr}/api/v1/runs/{run_id}/cancel"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"], "canceled", "{run}");
    assert_eq!(run["error"], "canceled from the UI");

    // Durable: the store agrees, and the state never flips back (the spawned
    // task was aborted).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let run: serde_json::Value = client
        .get(format!("http://{addr}/api/v1/runs/{run_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"], "canceled");

    // Canceling a terminal run returns it unchanged (idempotent-style: safe
    // to call again, same result, no duplicate terminal writes).
    let again: serde_json::Value = client
        .post(format!("http://{addr}/api/v1/runs/{run_id}/cancel"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["status"], "canceled");
}
