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
async fn valid_token_is_served_even_during_a_bad_auth_flood() {
    // Regression test for audit finding 4-b/#10: a valid credential must NEVER
    // be locked out by failed attempts (the self-DoS the old is-locked-first
    // gate caused). Flood with bad tokens to trip the lockout, then confirm a
    // bad token is throttled (429) BUT the valid token is still served (200).
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();
    for _ in 0..15 {
        let _ = client
            .get(format!("http://{addr}/api/v1/runs"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
    }
    // Unauthenticated requests are throttled during the flood.
    let bad = client
        .get(format!("http://{addr}/api/v1/runs"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad.status(),
        429,
        "anonymous requests are throttled during the flood"
    );
    // The VALID token is served regardless — never self-DoS'd by the flood.
    let good = client
        .get(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        good.status(),
        200,
        "a valid credential must never be locked out by failed attempts"
    );
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

/// Proposes invalid `echo` arguments once, then a valid echo, then finish.
struct OneRejectionOracle(std::sync::atomic::AtomicUsize);

#[async_trait]
impl PolicyOracle for OneRejectionOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (action, arguments) = match (n, context.steps.is_empty()) {
            (0, _) => ("echo", json!({"msg": "typo"})),
            (_, true) => ("echo", json!({"message": "hi"})),
            _ => ("finish", json!({"summary": "echoed"})),
        };
        Ok(ProposedAction {
            action: action.into(),
            arguments,
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:one-rejection".into()
    }
}

async fn fetch_events(addr: SocketAddr, token: &str, run_id: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let mut body = serde_json::Value::Null;
    for _ in 0..50 {
        body = client
            .get(format!("http://{addr}/api/v1/runs/{run_id}/events"))
            .bearer_auth(token)
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
    body
}

/// ADR 0036: the run history carries refusals (with the refused proposal
/// inline) and resumes (with session, time and the replayed step).
#[tokio::test]
async fn run_events_include_rejections_and_resumes() {
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let kernel = Arc::new(Kernel::new(
        Arc::new(OneRejectionOracle(Default::default())),
        Arc::new(EchoTools),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig::default(),
    ));
    let token = "test-token-123".to_string();
    let app = api::router(kernel.clone(), token.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // 1. A live run through the API with one refused proposal.
    let run_id = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .json(&json!({"task": "echo with a typo first"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = fetch_events(addr, &token, &run_id).await;
    assert_eq!(body["status"], "completed", "{body}");
    let journal = body["journal"].as_array().unwrap();
    let rejected: Vec<&serde_json::Value> =
        journal.iter().filter(|e| e["kind"] == "rejected").collect();
    assert_eq!(rejected.len(), 1, "{body}");
    let r = rejected[0];
    assert_eq!(r["step"], 0);
    assert_eq!(r["attempt"], 0);
    assert_eq!(r["source"], "schema");
    assert_eq!(r["subject"], "proposal");
    assert_eq!(r["action"], "echo");
    assert_eq!(r["arguments"], json!({"msg": "typo"}));
    assert!(r["at_ms"].as_u64().unwrap() > 0);
    let pos = |kind: &str| journal.iter().position(|e| e["kind"] == kind).unwrap();
    assert!(pos("rejected") < pos("decision"), "{body}");
    // A first execution is not a resume — even with the API's caller note
    // journaled before it.
    assert!(journal.iter().all(|e| e["kind"] != "resumed"), "{body}");

    // 2. A run killed after its side effect, picked up again (as the
    // daemon's startup sweep does).
    let crashed = kernel.create_run("echo then crash".into()).await.unwrap();
    for e in [
        kernel_core::JournalEntry::Decision {
            step: 0,
            action: "echo".into(),
            arguments: json!({"message": "hi"}),
            usage: None,
        },
        kernel_core::JournalEntry::Intent {
            step: 0,
            action: "echo".into(),
            arguments: json!({"message": "hi"}),
            idempotency_key: format!("{crashed}:0"),
        },
        kernel_core::JournalEntry::Receipt {
            step: 0,
            ok: true,
            result: json!({"echoed": "hi"}),
        },
    ] {
        kernel_core::RunStore::append_journal(store.as_ref(), crashed, e)
            .await
            .unwrap();
    }
    kernel.execute_run(crashed).await.unwrap();
    let body = fetch_events(addr, &token, &crashed.to_string()).await;
    assert_eq!(body["status"], "completed", "{body}");
    let journal = body["journal"].as_array().unwrap();
    let resumed: Vec<&serde_json::Value> =
        journal.iter().filter(|e| e["kind"] == "resumed").collect();
    assert_eq!(resumed.len(), 1, "{body}");
    let r = resumed[0];
    assert_eq!(r["at_step"], 0);
    assert_eq!(r["cause"], "interrupted");
    assert_eq!(r["replayed_steps"], json!([0]));
    assert_eq!(
        r["session"],
        kernel_core::journal::execution_session_id().to_string()
    );
    assert!(r["at_ms"].as_u64().unwrap() > 0);
    // Exactly-once: no second receipt for step 0.
    assert_eq!(
        journal
            .iter()
            .filter(|e| e["kind"] == "receipt" && e["step"] == 0)
            .count(),
        1
    );
}

// ---- ADR 0037: success contract + human confirmation ---------------------

async fn wait_terminal(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: &str,
    run_id: &str,
) -> serde_json::Value {
    for _ in 0..100 {
        let run: serde_json::Value = client
            .get(format!("http://{addr}/api/v1/runs/{run_id}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let s = run["status"].as_str().unwrap_or("");
        if !matches!(s, "pending" | "running") {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("run {run_id} never finished");
}

async fn submit_json(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn bad_success_contract_is_a_422_at_creation_and_creates_nothing() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();
    for bad in [
        json!({"require": []}),
        json!({"require": [{"kind": "action_succeeded", "action": "send_email"}]}),
        json!({"require": [{"kind": "action_succeeded", "action": "finish"}]}),
        json!({"require": [{"kind": "model_says_so"}]}),
        json!({"require": [{"kind": "result_equals", "action": "echo", "pointer": "x", "equals": 1}]}),
        json!("send_email"),
    ] {
        let res = submit_json(
            &client,
            addr,
            &token,
            json!({"task": "send an e-mail", "success_contract": bad}),
        )
        .await;
        assert_eq!(res.status(), 422, "{bad}");
        let msg = res.text().await.unwrap();
        assert!(msg.contains("success_contract"), "{bad}: {msg}");
    }
    let runs: serde_json::Value = client
        .get(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runs.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn contract_run_reports_verification_in_run_and_history() {
    let (addr, token) = spawn_server().await;
    let client = reqwest::Client::new();
    // Met: OneShotOracle echoes, then finishes.
    let res = submit_json(
        &client,
        addr,
        &token,
        json!({"task": "echo hi", "success_contract": {"require": [
            {"kind": "result_equals", "action": "echo", "pointer": "/echoed", "equals": "hi"}
        ]}}),
    )
    .await;
    assert_eq!(res.status(), 202);
    let id = res.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = wait_terminal(&client, addr, &token, &id).await;
    assert_eq!(run["status"], "completed");
    assert_eq!(run["verification"], "contract");
    assert_eq!(run["contract_report"]["satisfied"], true);
    assert!(run["confirmation"].is_null());

    let history = fetch_events(addr, &token, &id).await;
    assert_eq!(history["verification"], "contract");
    assert_eq!(
        history["success_contract"]["require"][0]["kind"],
        "result_equals"
    );
    let terminal = history["journal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "terminal")
        .unwrap()
        .clone();
    assert_eq!(terminal["verification"], "contract");

    // Unmet: the echo said "hi", the contract wants "bye".
    let res = submit_json(
        &client,
        addr,
        &token,
        json!({"task": "echo hi", "success_contract": {"require": [
            {"kind": "result_equals", "action": "echo", "pointer": "/echoed", "equals": "bye"}
        ]}}),
    )
    .await;
    let id = res.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = wait_terminal(&client, addr, &token, &id).await;
    assert_eq!(run["status"], "incomplete");
    assert_eq!(run["verification"], "contract");
    assert!(run["error"]
        .as_str()
        .unwrap()
        .contains("success contract not met"));
}

#[tokio::test]
async fn confirmation_endpoint_is_bearer_gated_journaled_and_reversible() {
    let (addr, token, alice, store) = spawn_with_principals().await;
    let client = reqwest::Client::new();
    let res = submit_json(&client, addr, &token, json!({"task": "echo hi"})).await;
    let id = res.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = wait_terminal(&client, addr, &token, &id).await;
    assert_eq!(run["status"], "completed");
    assert_eq!(run["verification"], "none");

    let url = format!("http://{addr}/api/v1/runs/{id}/confirmation");
    // No token, wrong token → 401, nothing recorded.
    assert_eq!(client.post(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .post(&url)
            .bearer_auth("nope")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Revoke before confirm → 409.
    assert_eq!(
        client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );

    // Confirmed by a named principal: attribution comes from the token.
    let res = client.post(&url).bearer_auth(&alice).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["run"]["confirmation"]["principal"], "alice");
    // Twice → 409.
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );

    let res = client
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["run"]["confirmation"].is_null());
    assert_eq!(body["forgotten_learned_ids"], json!([]));

    let uuid: uuid::Uuid = id.parse().unwrap();
    let decisions: Vec<serde_json::Value> = store
        .journal(uuid)
        .await
        .unwrap()
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["kind"] == "success_confirmation_decided")
        .collect();
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0]["decision"], "confirm");
    assert_eq!(decisions[0]["principal"], "alice");
    assert_eq!(decisions[1]["decision"], "revoke");
    assert_eq!(decisions[1]["principal"], "default");

    // Unknown run → 404.
    let res = client
        .post(format!(
            "http://{addr}/api/v1/runs/{}/confirmation",
            uuid::Uuid::new_v4()
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}
