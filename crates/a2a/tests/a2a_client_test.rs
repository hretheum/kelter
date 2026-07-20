//! A2A client + delegation against a mock A2A server (axum), no network.
//! Covers: card resolution + origin pinning, the send→poll→terminal cycle,
//! durable reconnect via a journaled task id, SSRF refusal, and rejection of
//! a hostile (unknown-state) response.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{A2aActionSource, RemoteAgent};
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, InMemoryRunStore, JournalEntry,
    ProposedAction, RunStore, ValidatedAction, A2A_ANCESTRY_ARG, A2A_DEPTH_ARG,
};

#[derive(Default)]
struct MockState {
    /// How many times SendMessage was called — proves no double delegation.
    send_calls: AtomicUsize,
    /// GetTask calls before the task is reported completed.
    polls_before_done: usize,
    get_calls: AtomicUsize,
    /// Emit an unknown task state (hostile server).
    hostile: bool,
    /// The endpoint URL the card advertises. Set once the port is known;
    /// an explicit value (SSRF tests) overrides self-pointing.
    endpoint: std::sync::Mutex<String>,
    endpoint_override: Option<String>,
    /// The `message.metadata` object from the most recent SendMessage call
    /// (ADR 0022) — `None` if no SendMessage carried one.
    last_send_metadata: std::sync::Mutex<Option<Value>>,
}

async fn card(State(st): State<Arc<MockState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": "Toy Agent",
        "description": "echoes",
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
        "version": "1.0.0",
        "capabilities": {"streaming": false},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": []
    }))
}

async fn rpc(State(st): State<Arc<MockState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});

    match method {
        "SendMessage" => {
            st.send_calls.fetch_add(1, Ordering::SeqCst);
            *st.last_send_metadata.lock().unwrap() =
                req["params"]["message"].get("metadata").cloned();
            // Always return a Task to poll (id fixed so reconnect is testable).
            Json(reply(json!({
                "task": {"id": "task-42", "status": {"state": "TASK_STATE_SUBMITTED"}, "artifacts": []}
            })))
        }
        "GetTask" => {
            let n = st.get_calls.fetch_add(1, Ordering::SeqCst);
            let state = if st.hostile {
                "TASK_STATE_TELEPORTING" // not a real state
            } else if n >= st.polls_before_done {
                "TASK_STATE_COMPLETED"
            } else {
                "TASK_STATE_WORKING"
            };
            let artifacts = if state == "TASK_STATE_COMPLETED" {
                json!([{"artifactId": "a1", "parts": [{"text": "the remote answer"}]}])
            } else {
                json!([])
            };
            Json(reply(json!({
                "id": "task-42", "status": {"state": state}, "artifacts": artifacts
            })))
        }
        other => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        })),
    }
}

async fn spawn_server(state: Arc<MockState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    // Point the card at ourselves unless an SSRF-mismatch override is set.
    let endpoint = state
        .endpoint_override
        .clone()
        .unwrap_or_else(|| format!("{base}/rpc"));
    *state.endpoint.lock().unwrap() = endpoint;
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn ctx_with_notes(store: Arc<dyn RunStore>, notes: &[(&str, Value)]) -> ExecutionContext {
    let mut map = std::collections::HashMap::new();
    for (k, v) in notes {
        map.insert(k.to_string(), v.clone());
    }
    ExecutionContext::new(uuid::Uuid::new_v4(), 0, store, map)
}

fn delegate_action(source: &ActionSchema, text: &str) -> ValidatedAction {
    let name = &source.actions[0].name;
    source
        .validate(kernel_core::ProposedAction {
            action: name.clone(),
            arguments: json!({"task": text}),
            rationale: None,
        })
        .unwrap()
}

#[tokio::test]
async fn delegation_send_poll_completes_and_is_validated() {
    let state = Arc::new(MockState {
        polls_before_done: 2,
        ..Default::default()
    });
    let base = spawn_server(state.clone()).await;
    let state2 = state.clone();

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "toy".into(),
        card_url: base.clone(),
        token: None,
    }])
    .unwrap();
    let schema = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap();
    assert_eq!(schema.actions[0].name, "delegate_to_toy");

    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let ctx = ctx_with_notes(store.clone(), &[]);
    let action = delegate_action(&schema, "summarize the news");
    let result = src.execute(&action, &ctx).await.unwrap();

    assert_eq!(result["state"], "TASK_STATE_COMPLETED");
    assert_eq!(result["output"], "the remote answer");
    assert_eq!(result["agent"], "toy");
    assert_eq!(state2.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn resume_reconnects_to_journaled_task_without_redelegating() {
    let state = Arc::new(MockState::default()); // completes on the first GetTask
    let base = spawn_server(state.clone()).await;
    let state2 = state.clone();

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "toy".into(),
        card_url: base.clone(),
        token: None,
    }])
    .unwrap();
    let schema = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap();

    // Simulate a crash-resume: the journal already has the task id, so the
    // client must reconnect (GetTask) and NOT call SendMessage again.
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let ctx = ctx_with_notes(store, &[("a2a_task_id", json!("task-42"))]);
    let action = delegate_action(&schema, "resumed task");
    let result = src.execute(&action, &ctx).await.unwrap();

    assert_eq!(result["state"], "TASK_STATE_COMPLETED");
    assert_eq!(result["a2a_task_id"], "task-42");
    assert_eq!(
        state2.send_calls.load(Ordering::SeqCst),
        0,
        "resume must NOT re-delegate — zero SendMessage calls expected"
    );
    assert!(state2.get_calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn fresh_delegation_journals_the_task_id() {
    let state = Arc::new(MockState::default());
    let base = spawn_server(state).await;

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "toy".into(),
        card_url: base,
        token: None,
    }])
    .unwrap();
    let schema = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap();

    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let run_id = uuid::Uuid::new_v4();
    let ctx = ExecutionContext::new(run_id, 0, store.clone(), Default::default());
    let action = delegate_action(&schema, "fresh");
    src.execute(&action, &ctx).await.unwrap();

    let journal = store.journal(run_id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::Note { key, value, .. }
            if key == "a2a_task_id" && value == &json!("task-42")
    )));
}

#[tokio::test]
async fn ssrf_refuses_endpoint_outside_declared_host() {
    // Card is served from base, but advertises an endpoint on a DIFFERENT
    // host — the client must refuse to dial it.
    let state = Arc::new(MockState {
        endpoint_override: Some("http://169.254.169.254/latest/meta-data".into()),
        ..Default::default()
    });
    let base = spawn_server(state).await;

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "evil".into(),
        card_url: base,
        token: None,
    }])
    .unwrap();
    let schema = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let ctx = ctx_with_notes(store, &[]);
    let action = delegate_action(&schema, "x");
    let err = src.execute(&action, &ctx).await.unwrap_err();
    assert!(
        err.to_string().contains("SSRF allowlist violation"),
        "expected SSRF refusal, got: {err}"
    );
}

#[tokio::test]
async fn delegation_lineage_rides_the_outbound_a2a_metadata() {
    // ADR 0022: depth/ancestry the kernel injected into the validated
    // action's execution-copy arguments (mirroring
    // `ValidatedAction::inject_idempotency_key`'s precedent — see
    // `kernel_core::action::inject_a2a_lineage`, `pub(crate)` there, so this
    // test constructs an equivalent `ValidatedAction` directly via a schema
    // that allows the reserved keys, exactly what a validated+injected
    // action looks like from `A2aActionSource::execute`'s point of view)
    // must ride the outbound `SendMessage` as a `metadata` object — the
    // ONLY mechanism a downstream Kelter hop has to enforce depth/cycle
    // limits across a multi-hop chain.
    let state = Arc::new(MockState::default());
    let base = spawn_server(state.clone()).await;
    let state2 = state.clone();

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "toy".into(),
        card_url: base,
        token: None,
    }])
    .unwrap();
    let action_name = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap()
        .actions[0]
        .name
        .clone();

    // A schema permissive enough to admit the reserved lineage keys, so the
    // resulting `ValidatedAction` matches what the kernel hands
    // `A2aActionSource::execute` after `inject_a2a_lineage` runs.
    let lineage_schema = ActionSchema {
        actions: vec![ActionSpec {
            name: action_name.clone(),
            description: "".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    A2A_DEPTH_ARG: {"type": "integer"},
                    A2A_ANCESTRY_ARG: {"type": "array"}
                },
                "required": ["task"]
            }),
        }],
    };
    let action = lineage_schema
        .validate(ProposedAction {
            action: action_name,
            arguments: json!({
                "task": "chained sub-task",
                A2A_DEPTH_ARG: 2,
                A2A_ANCESTRY_ARG: ["http://origin-a:9000", "http://origin-b:9000"],
            }),
            rationale: None,
        })
        .unwrap();

    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let ctx = ctx_with_notes(store, &[]);
    let result = src.execute(&action, &ctx).await.unwrap();
    assert_eq!(result["state"], "TASK_STATE_COMPLETED");

    let metadata = state2
        .last_send_metadata
        .lock()
        .unwrap()
        .clone()
        .expect("SendMessage must carry a metadata object when lineage is present");
    assert_eq!(metadata["kelter_a2a_depth"], json!(2));
    assert_eq!(
        metadata["kelter_a2a_ancestry"],
        json!(["http://origin-a:9000", "http://origin-b:9000"])
    );
}

#[tokio::test]
async fn hostile_unknown_task_state_is_rejected() {
    let state = Arc::new(MockState {
        hostile: true,
        ..Default::default()
    });
    let base = spawn_server(state).await;

    let src = A2aActionSource::new(vec![RemoteAgent {
        name: "toy".into(),
        card_url: base,
        token: None,
    }])
    .unwrap();
    let schema = src
        .action_schema(kernel_core::Latitude::Wide)
        .await
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let ctx = ctx_with_notes(store, &[]);
    let action = delegate_action(&schema, "x");
    let err = src.execute(&action, &ctx).await.unwrap_err();
    assert!(
        err.to_string().contains("unknown task state"),
        "hostile state must be rejected, got: {err}"
    );
}
