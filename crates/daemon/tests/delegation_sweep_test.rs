//! The delegation-authorization `ask` timeout sweeper (ADR 0023): a paused
//! run must never hang indefinitely. `daemon::delegation_sweep::
//! sweep_timed_out_delegation_decisions` is extracted from `main()`
//! specifically so this guarantee has a real, deterministic unit test rather
//! than only being provable by inspection or a slow wall-clock integration
//! run. Twin of `skill_sweep_test.rs`, using the a2a/delegation-governance
//! mock-agent idiom from `crates/a2a/tests/delegation_governance_test.rs`
//! (audit finding 5-a/#5 — this test did not exist before).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{
    action_name, parse_agent_ask_fallback, A2aActionSource, AgentDelegationPolicy, AgentRegistry,
    RegistryDelegationGate, RemoteAgent,
};
use daemon::delegation_sweep::sweep_timed_out_delegation_decisions;
use kernel_core::{
    ActionSchema, InMemoryRunStore, Kernel, KernelConfig, PolicyContext, PolicyError, PolicyOracle,
    Proposal, ProposedAction, RunStatus,
};
use memory::InMemoryProvider;

fn tmp() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kelter-delegation-sweep-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default)]
struct MockAgentState {
    name: String,
    send_calls: AtomicUsize,
    endpoint: std::sync::Mutex<String>,
}

async fn card(State(st): State<Arc<MockAgentState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": st.name,
        "description": format!("mock agent '{}'", st.name),
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
    }))
}

async fn rpc(State(st): State<Arc<MockAgentState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    if method != "SendMessage" {
        return Json(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "?"}}),
        );
    }
    st.send_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "jsonrpc": "2.0", "id": id,
        "result": {"message": {"parts": [{"text": "ok"}]}}
    }))
}

async fn spawn_agent(name: &str) -> (String, Arc<MockAgentState>) {
    let state = Arc::new(MockAgentState {
        name: name.to_string(),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    *state.endpoint.lock().unwrap() = format!("{base}/rpc");
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state)
}

/// Proposes ONE fixed `delegate_to_*` action on the very first (fresh)
/// decision point, `finish` on every subsequent call.
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
                arguments: json!({"task": "do work"}),
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

#[tokio::test]
async fn timeout_sweeper_denies_and_resumes() {
    let (base, state) = spawn_agent("ask-worker").await;
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
    let manifest = registry
        .register(&base, None, "discovery", "untrusted")
        .await
        .unwrap();
    let action_id = action_name(&manifest.name);

    let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
    source
        .register_runtime(&RemoteAgent {
            name: manifest.name.clone(),
            card_url: base,
            token: None,
        })
        .unwrap();

    let gate = Arc::new(
        RegistryDelegationGate::new(registry, AgentDelegationPolicy::Ask)
            .with_ask_fallback(parse_agent_ask_fallback("queue")),
    );
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let kernel = Arc::new(
        Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: action_id.clone(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig::default(),
        )
        .with_delegation_gate(delegation_gate),
    );

    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
    let requested_at = run
        .pending_delegation
        .expect("pending delegation recorded on the run")
        .requested_at;

    // Before the timeout elapses: the sweep must not act.
    let acted = sweep_timed_out_delegation_decisions(&kernel, 300, requested_at + 10).await;
    assert!(
        acted.is_empty(),
        "must not act before the configured timeout has elapsed"
    );
    let run = kernel.store().get(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
    assert_eq!(
        state.send_calls.load(Ordering::SeqCst),
        0,
        "an undecided delegation must never dial"
    );

    // After the timeout: force-denies (principal system:timeout) and reports
    // the run id for the caller to resume.
    let acted = sweep_timed_out_delegation_decisions(&kernel, 300, requested_at + 301).await;
    assert_eq!(acted, vec![run_id]);

    // The caller (main.rs) re-invokes execute_run for each returned id —
    // mirrored here directly.
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(
        run.status,
        RunStatus::Incomplete,
        "a timed-out ask resolves to Deny and the run finishes without delegating — never hangs \
         (and, having done nothing effective, never as Completed: ADR 0033)"
    );
    assert_eq!(
        state.send_calls.load(Ordering::SeqCst),
        0,
        "a denied delegation never dials the remote agent"
    );

    // De-flake diagnosis (this test's own history): an earlier draft asserted
    // `run.steps` contained a step for the denied delegation, which failed —
    // NOT a flaky async-write race, but a TEST-BUG asserting a non-invariant.
    // A denied step-mode delegation NEVER executes, so it is deliberately not
    // a `StepRecord` (kernel-core/src/engine.rs: a `Deny` on resume only sets
    // `last_error` and journals no `Decision`). The REAL, durable record of
    // the denial is the journaled `AgentDelegationDecided{Deny}` — written
    // synchronously by `decide_delegation` (both `append_journal` and `put`
    // are awaited) BEFORE the sweep returns. Pin both facts so the invariant
    // can't silently drift again:
    assert!(
        !run.steps.iter().any(|s| s.action == action_id),
        "a denied delegation is not a recorded execution step (it never ran)"
    );
    let journal = kernel.store().journal(run_id).await.unwrap();
    assert!(
        journal.iter().any(|e| matches!(
            e,
            kernel_core::JournalEntry::AgentDelegationDecided {
                agent, principal, decision, ..
            } if agent == &action_id
                && principal == "system:timeout"
                && matches!(decision, kernel_core::AgentDelegationDecision::Deny)
        )),
        "the denial is DURABLY journaled (AgentDelegationDecided{{Deny, system:timeout}}) \
         before the sweep returns — the real reversibility-safe record: {journal:?}"
    );
}

#[tokio::test]
async fn timeout_sweeper_ignores_runs_not_awaiting_delegation_decision() {
    // A run that never paused for a delegation ask (e.g. plain Completed) must
    // be silently skipped, not mistaken for an unresolved pause.
    let (base, _state) = spawn_agent("trusted-worker").await;
    let root = tmp();
    let configured = vec![RemoteAgent {
        name: "trusted-worker".into(),
        card_url: base,
        token: None,
    }];
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &configured).unwrap());
    let source = Arc::new(A2aActionSource::new(configured).unwrap());
    let gate = Arc::new(RegistryDelegationGate::new(
        registry,
        AgentDelegationPolicy::TrustedOnly,
    ));
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let kernel = Arc::new(
        Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: "delegate_to_trusted_worker".into(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig::default(),
        )
        .with_delegation_gate(delegation_gate),
    );

    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    let acted = sweep_timed_out_delegation_decisions(&kernel, 0, u64::MAX).await;
    assert!(
        acted.is_empty(),
        "a completed run must never be swept as a timed-out ask"
    );
}
