//! Agent trust/policy/ask (ADR 0023, Parts 1-3): real `Kernel` + real mock
//! A2A HTTP servers (axum), no internal-state-only assertions — mirrors
//! `a2a_client_test.rs`'s mock-server idiom and
//! `skills/tests/ask_activation_test.rs`'s ask-decision idiom.

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
use kernel_core::{
    ActionSchema, AgentDelegationDecision, InMemoryRunStore, Kernel, KernelConfig, PolicyContext,
    PolicyError, PolicyOracle, Proposal, ProposedAction, RunEvent, RunStatus,
};
use memory::InMemoryProvider;

// ---- shared fixtures (duplicated per test file by this codebase's own
// convention — see ask_activation_test.rs's fixture-duplication note) -------

fn tmp() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-delegation-gov-{}", uuid_ish()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_ish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as O};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, O::Relaxed)
    )
}

#[derive(Default)]
struct MockAgentState {
    name: String,
    send_calls: AtomicUsize,
    always_fail: bool,
    endpoint: std::sync::Mutex<String>,
    /// SSRF test hook: advertise an endpoint on a different host than the
    /// card was served from.
    endpoint_override: Option<String>,
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
    if st.always_fail {
        Json(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "mock failure"}}),
        )
    } else {
        Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"message": {"parts": [{"text": "ok"}]}}
        }))
    }
}

/// Spawn one mock agent server. Returns `(base_url, state)`.
async fn spawn_agent(name: &str, always_fail: bool) -> (String, Arc<MockAgentState>) {
    let state = Arc::new(MockAgentState {
        name: name.to_string(),
        always_fail,
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    *state.endpoint.lock().unwrap() = state
        .endpoint_override
        .clone()
        .unwrap_or_else(|| format!("{base}/rpc"));
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state)
}

/// A hostile agent whose card advertises an endpoint OUTSIDE its own host —
/// the SSRF-pinning test fixture.
async fn spawn_hostile_agent() -> String {
    let state = Arc::new(MockAgentState {
        name: "hostile".into(),
        endpoint_override: Some("http://169.254.169.254/latest/meta-data".into()),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    *state.endpoint.lock().unwrap() = state.endpoint_override.clone().unwrap();
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Proposes ONE fixed `delegate_to_*` action on the very first (fresh)
/// decision point, `finish` on every subsequent call — including a re-ask
/// after a rejection, since the kernel's OWN gates/resilience decide
/// whether to retry, never the model.
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

fn kernel_with(
    action_source: Arc<A2aActionSource>,
    gate: Arc<RegistryDelegationGate>,
    action: &str,
) -> Arc<Kernel> {
    let oracle = Arc::new(DelegateOnceOracle {
        action: action.to_string(),
    });
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;
    Arc::new(
        Kernel::new(
            oracle,
            action_source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig::default(),
        )
        .with_delegation_gate(delegation_gate),
    )
}

// ---- (1) TrustedOnly gates untrusted delegation ----------------------------

#[tokio::test]
async fn trusted_only_default_gates_untrusted_delegation() {
    let (trusted_base, trusted_state) = spawn_agent("trusted-worker", false).await;
    let (untrusted_base, untrusted_state) = spawn_agent("untrusted-worker", false).await;

    let root = tmp();
    let configured = vec![RemoteAgent {
        name: "trusted-worker".into(),
        card_url: trusted_base.clone(),
        token: None,
    }];
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &configured).unwrap());
    assert_eq!(
        registry.trust_of("trusted-worker").as_deref(),
        Some("trusted"),
        "a config-declared agent starts trusted (ADR 0023 default-trust split)"
    );

    // Discovery-registered: starts untrusted, SEPARATELY from the config seed.
    let manifest = registry
        .register(&untrusted_base, None, "discovery", "untrusted")
        .await
        .unwrap();
    assert_eq!(manifest.trust, "untrusted");

    let source = Arc::new(A2aActionSource::new(configured).unwrap());
    source
        .register_runtime(&RemoteAgent {
            name: "untrusted-worker".into(),
            card_url: untrusted_base,
            token: None,
        })
        .unwrap();

    let gate = Arc::new(RegistryDelegationGate::new(
        registry.clone(),
        AgentDelegationPolicy::TrustedOnly,
    ));

    // A config-declared (hence pre-trusted) agent delegates freely.
    let kernel = kernel_with(source.clone(), gate.clone(), "delegate_to_trusted_worker");
    let run_id = kernel.create_run("t1".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(run
        .steps
        .iter()
        .any(|s| s.ok && s.action == "delegate_to_trusted_worker"));
    assert_eq!(trusted_state.send_calls.load(Ordering::SeqCst), 1);

    // A SEPARATELY discovery-registered (untrusted) agent's delegation is
    // refused under TrustedOnly — never dialed.
    let kernel2 = kernel_with(source, gate, "delegate_to_untrusted_worker");
    let mut events = kernel2.subscribe();
    let run_id2 = kernel2.create_run("t2".into()).await.unwrap();
    let run2 = kernel2.execute_run(run_id2).await.unwrap();
    assert_eq!(
        run2.status,
        RunStatus::Incomplete,
        "the model falls through to finish — with the delegation refused, nothing effective \
         ran, so the kernel withholds success (ADR 0033)"
    );
    assert_eq!(
        untrusted_state.send_calls.load(Ordering::SeqCst),
        0,
        "TrustedOnly must refuse BEFORE dialing"
    );
    let mut saw_refusal = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Rejected { reason, .. } = &env.event {
            if reason.contains("untrusted-worker") && reason.contains("TrustedOnly") {
                saw_refusal = true;
            }
        }
    }
    assert!(saw_refusal, "must emit a governed, loud Rejected reason");
}

// ---- (2) Ask pauses; all three decisions resolve correctly -----------------

#[tokio::test]
async fn ask_pauses_and_all_three_decisions_resolve_correctly() {
    for decision in [
        AgentDelegationDecision::Deny,
        AgentDelegationDecision::AllowOnce,
        AgentDelegationDecision::AllowAlways,
    ] {
        let (base, state) = spawn_agent("ask-worker", false).await;
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
            RegistryDelegationGate::new(registry.clone(), AgentDelegationPolicy::Ask)
                .with_ask_fallback(parse_agent_ask_fallback("queue")),
        );

        let kernel = kernel_with(source, gate, &action_id);
        let mut events = kernel.subscribe();
        let run_id = kernel.create_run("t".into()).await.unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();

        assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
        let pending = run
            .pending_delegation
            .clone()
            .expect("pending delegation recorded on the run");
        assert_eq!(pending.agent, action_id);
        assert!(run.steps.is_empty(), "nothing executed while undecided");
        let mut saw_request = false;
        while let Ok(env) = events.try_recv() {
            if let RunEvent::AgentDelegationRequested { agent, .. } = &env.event {
                if agent == &action_id {
                    saw_request = true;
                }
            }
        }
        assert!(saw_request, "must emit AgentDelegationRequested");

        kernel
            .decide_delegation(run_id, &action_id, decision, "operator")
            .await
            .unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();
        // Deny means nothing effective ran before `finish` → Incomplete (ADR
        // 0033); an allowed delegation succeeded → Completed.
        let expected = if decision == AgentDelegationDecision::Deny {
            RunStatus::Incomplete
        } else {
            RunStatus::Completed
        };
        assert_eq!(run.status, expected, "{decision:?}");

        match decision {
            AgentDelegationDecision::Deny => {
                assert_eq!(state.send_calls.load(Ordering::SeqCst), 0);
                assert_eq!(
                    registry.trust_of(&manifest.name).as_deref(),
                    Some("untrusted"),
                    "Deny must never change trust"
                );
            }
            AgentDelegationDecision::AllowOnce => {
                assert_eq!(state.send_calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    registry.trust_of(&manifest.name).as_deref(),
                    Some("untrusted"),
                    "AllowOnce must never change trust"
                );
            }
            AgentDelegationDecision::AllowAlways => {
                assert_eq!(state.send_calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    registry.trust_of(&manifest.name).as_deref(),
                    Some("trusted"),
                    "AllowAlways elevates persistent trust"
                );
            }
        }
    }
}

// ---- (3) discovery is untrusted + SSRF-pinned AT REGISTRATION --------------

#[tokio::test]
async fn discovery_registers_untrusted_and_ssrf_pinned() {
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());

    // A hostile/redirecting card is refused AT REGISTRATION, before it is
    // ever persisted.
    let hostile_base = spawn_hostile_agent().await;
    let err = registry
        .register(&hostile_base, None, "discovery", "untrusted")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("SSRF"),
        "expected an SSRF refusal at registration, got: {err}"
    );
    assert!(
        registry.list().is_empty(),
        "a refused registration must never be persisted"
    );

    // A legitimate agent registers fine and starts untrusted.
    let (good_base, _state) = spawn_agent("good-worker", false).await;
    let manifest = registry
        .register(&good_base, None, "discovery", "untrusted")
        .await
        .unwrap();
    assert_eq!(manifest.trust, "untrusted");
    assert_eq!(manifest.source_kind, "discovery");
    assert_eq!(registry.list().len(), 1);
}
