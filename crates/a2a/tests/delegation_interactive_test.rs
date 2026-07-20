//! Interactive multi-turn delegation (ADR 0026): an authorized delegation
//! whose remote task enters `input-required` PAUSES the run durably
//! (`AwaitingDelegationInput`) instead of failing; a human's continuation
//! resumes it, reconnecting to the SAME remote task (never re-dialing); and
//! the pause survives a restart. Real `Kernel` (step mode) + a real mock A2A
//! server (axum), no network beyond loopback.
//!
//! Fixtures duplicated per test file by this repo's convention.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{action_name, A2aActionSource, RemoteAgent};
use kernel_core::{
    ActionSchema, InMemoryRunStore, Kernel, KernelConfig, PolicyContext, PolicyError, PolicyOracle,
    Proposal, ProposedAction, RunStatus, RunStore,
};
use memory::InMemoryProvider;

#[derive(Default)]
struct MockState {
    /// Initial (fresh) SendMessage calls — proves the delegation dials once.
    send_calls: AtomicUsize,
    /// Continuation SendMessage calls (message carried a `taskId`).
    continuation_calls: AtomicUsize,
    /// Flipped true once a continuation arrives → GetTask then completes.
    got_continuation: AtomicBool,
    endpoint: std::sync::Mutex<String>,
}

async fn card(State(st): State<Arc<MockState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": "Toy Agent",
        "description": "asks one clarifying question, then answers",
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
            let is_continuation = req["params"]["message"].get("taskId").is_some();
            if is_continuation {
                st.continuation_calls.fetch_add(1, Ordering::SeqCst);
                st.got_continuation.store(true, Ordering::SeqCst);
            } else {
                st.send_calls.fetch_add(1, Ordering::SeqCst);
            }
            Json(reply(json!({
                "task": {"id": "task-i", "status": {"state": "TASK_STATE_WORKING"}, "artifacts": []}
            })))
        }
        "GetTask" => {
            let (state, artifacts) = if st.got_continuation.load(Ordering::SeqCst) {
                (
                    "TASK_STATE_COMPLETED",
                    json!([{"artifactId": "a1", "parts": [{"text": "hello Alice, done"}]}]),
                )
            } else {
                (
                    // The remote needs more input — carries its question as an
                    // artifact, which the client surfaces as the prompt.
                    "TASK_STATE_INPUT_REQUIRED",
                    json!([{"artifactId": "q1", "parts": [{"text": "what is your name?"}]}]),
                )
            };
            Json(reply(json!({
                "id": "task-i", "status": {"state": state}, "artifacts": artifacts
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
    *state.endpoint.lock().unwrap() = format!("{base}/rpc");
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Proposes ONE fixed delegation fresh, then `finish` on every later decision.
struct DelegateOnceOracle {
    action: String,
}
#[async_trait]
impl PolicyOracle for DelegateOnceOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        if context.steps.is_empty() && context.last_error.is_none() {
            return Ok(ProposedAction {
                action: self.action.clone(),
                arguments: json!({"task": "greet me"}),
                rationale: None,
            }
            .into());
        }
        Ok(ProposedAction {
            action: "finish".into(),
            arguments: json!({"summary": "done"}),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:delegate-once".into()
    }
}

async fn source_for(base: &str) -> (Arc<A2aActionSource>, String) {
    let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
    source
        .register_runtime(&RemoteAgent {
            name: "toy".into(),
            card_url: base.to_string(),
            token: None,
        })
        .unwrap();
    (source, action_name("toy"))
}

fn kernel(oracle_action: &str, source: Arc<A2aActionSource>, store: Arc<dyn RunStore>) -> Kernel {
    Kernel::new(
        Arc::new(DelegateOnceOracle {
            action: oracle_action.to_string(),
        }),
        source,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig::default(),
    )
}

#[tokio::test]
async fn input_required_pauses_the_run_then_a_continuation_completes_it() {
    let mock = Arc::new(MockState::default());
    let base = spawn_server(mock.clone()).await;
    let (source, action) = source_for(&base).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store);

    // First execution: the remote asks for input → the run PAUSES (not fails).
    let run_id = k.create_run("interactive".into()).await.unwrap();
    let paused = k.execute_run(run_id).await.unwrap();

    assert_eq!(
        paused.status,
        RunStatus::AwaitingDelegationInput,
        "input-required must pause the run, never fail it"
    );
    let pending = paused
        .pending_delegation_input
        .expect("a pending delegation-input request");
    assert_eq!(pending.agent, action);
    assert_eq!(pending.task_id, "task-i");
    assert_eq!(pending.prompt, "what is your name?");
    assert_eq!(mock.send_calls.load(Ordering::SeqCst), 1, "dialed once");
    assert_eq!(mock.continuation_calls.load(Ordering::SeqCst), 0);

    // The human answers → resume → the delegation reconnects to the SAME task
    // and sends the continuation; the remote then completes.
    k.provide_delegation_input(run_id, "Alice", "operator")
        .await
        .unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        1,
        "the continuation must NOT re-dial a fresh task — reconnect only"
    );
    assert_eq!(
        mock.continuation_calls.load(Ordering::SeqCst),
        1,
        "exactly one continuation message sent to the paused task"
    );
    // The delegate step's recorded result carries the remote's final answer.
    let delegate_step = run
        .steps
        .iter()
        .find(|s| s.action == action)
        .expect("the delegate step is recorded");
    assert!(delegate_step.ok);
    assert!(
        delegate_step.result["output"]
            .as_str()
            .unwrap_or("")
            .contains("hello Alice, done"),
        "the completed delegation's output is the remote's post-continuation answer: {:?}",
        delegate_step.result
    );
}

#[tokio::test]
async fn interactive_pause_survives_a_restart() {
    let mock = Arc::new(MockState::default());
    let base = spawn_server(mock.clone()).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());

    // ---- session 1: run to the input-required pause, then drop everything ----
    let run_id = {
        let (source, action) = source_for(&base).await;
        let k = kernel(&action, source, store.clone());
        let id = k.create_run("interactive restart".into()).await.unwrap();
        let paused = k.execute_run(id).await.unwrap();
        assert_eq!(paused.status, RunStatus::AwaitingDelegationInput);
        id
    };
    assert_eq!(mock.send_calls.load(Ordering::SeqCst), 1);

    // ---- session 2: fresh kernel + fresh a2a source over the same store ----
    let (source2, action2) = source_for(&base).await;
    let k2 = kernel(&action2, source2, store.clone());

    // Restart re-surfaces the pending input request WITHOUT dialing.
    let resurfaced = k2.execute_run(run_id).await.unwrap();
    assert_eq!(resurfaced.status, RunStatus::AwaitingDelegationInput);
    assert_eq!(
        resurfaced.pending_delegation_input.unwrap().prompt,
        "what is your name?"
    );
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        1,
        "re-surfacing after a restart dials nothing new"
    );

    // Answer + resume on the new kernel → the continuation goes to the same task.
    k2.provide_delegation_input(run_id, "Alice", "operator")
        .await
        .unwrap();
    let run = k2.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(mock.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(mock.continuation_calls.load(Ordering::SeqCst), 1);
}
