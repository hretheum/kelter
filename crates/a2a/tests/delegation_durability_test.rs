//! Durability: a paused delegation-authorization ask (ADR 0023) survives a
//! daemon restart. Mirrors `skills/tests/ask_activation_test.rs`'s
//! `paused_run_survives_a_restart` — the two-session real-SQLite idiom —
//! exercising a full `Kernel` (not just `RunStore` CRUD) so it proves the
//! `unfinished_runs` fix (a `RunStatus::AwaitingDelegationDecision` run must
//! be picked up by the resume sweep) and the top-of-`execute_run_inner`
//! early re-surface check work together end to end.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{
    action_name, A2aActionSource, AgentDelegationPolicy, AgentRegistry, RegistryDelegationGate,
    RemoteAgent,
};
use kernel_core::{
    ActionSchema, AgentDelegationDecision, Kernel, KernelConfig, PolicyContext, PolicyError,
    PolicyOracle, Proposal, ProposedAction, RunStatus,
};
use memory::InMemoryProvider;

fn tmp() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-delegation-durability-{}", uuid_ish()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_ish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Default)]
struct MockAgentState {
    endpoint: std::sync::Mutex<String>,
}

async fn card(State(st): State<Arc<MockAgentState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": "durable-worker",
        "description": "mock agent",
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
    }))
}

async fn rpc(_state: State<Arc<MockAgentState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req["id"].clone();
    Json(json!({
        "jsonrpc": "2.0", "id": id,
        "result": {"message": {"parts": [{"text": "ok"}]}}
    }))
}

async fn spawn_agent() -> String {
    let state = Arc::new(MockAgentState::default());
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
async fn paused_delegation_survives_a_restart() {
    let root = tmp();
    let db_path = root.join("kelter.db");
    let registry_path = root.join("agents.toml");

    let base = spawn_agent().await;
    let run_id;

    // --- session 1: the model proposes a delegation to an untrusted agent
    //     under `Ask`; the gate asks; the run pauses. Then "the daemon
    //     crashes/restarts". ---------------------------------------------
    {
        let registry = Arc::new(AgentRegistry::open(&registry_path, &[]).unwrap());
        let manifest = registry
            .register(&base, None, "discovery", "untrusted")
            .await
            .unwrap();
        let action = action_name(&manifest.name);

        let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
        source
            .register_runtime(&RemoteAgent {
                name: manifest.name.clone(),
                card_url: base.clone(),
                token: None,
            })
            .unwrap();

        let gate: Arc<dyn kernel_core::DelegationGate> = Arc::new(
            RegistryDelegationGate::new(registry, AgentDelegationPolicy::Ask)
                .with_ask_fallback(a2a::parse_agent_ask_fallback("queue")),
        );

        let store: Arc<dyn kernel_core::RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));
        let kernel = Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: action.clone(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            store,
            KernelConfig::default(),
        )
        .with_delegation_gate(gate);

        run_id = kernel.create_run("t".into()).await.unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
    } // store + kernel dropped — "the daemon process ended"

    // --- session 2: reopen at the same paths ("daemon restarted") --------
    {
        let registry = Arc::new(AgentRegistry::open(&registry_path, &[]).unwrap());
        let manifest_name = registry
            .list()
            .first()
            .expect("the discovery-registered agent survived the restart")
            .name
            .clone();
        let action = action_name(&manifest_name);

        let store: Arc<dyn kernel_core::RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));

        // THE load-bearing fix: a paused run must be in unfinished_runs(), or
        // the daemon's startup resume sweep would silently orphan it forever.
        let unfinished = store.unfinished_runs().await.unwrap();
        assert!(
            unfinished.contains(&run_id),
            "a paused (AwaitingDelegationDecision) run must be picked up by the resume sweep — \
             got {unfinished:?}"
        );

        let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
        source
            .register_runtime(&RemoteAgent {
                name: manifest_name,
                card_url: base,
                token: None,
            })
            .unwrap();
        let gate: Arc<dyn kernel_core::DelegationGate> = Arc::new(
            RegistryDelegationGate::new(registry, AgentDelegationPolicy::Ask)
                .with_ask_fallback(a2a::parse_agent_ask_fallback("queue")),
        );
        let kernel = Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: action.clone(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            store,
            KernelConfig::default(),
        )
        .with_delegation_gate(gate);

        // Simulate the startup sweep calling execute_run again: must
        // re-surface — not lose the pause, not silently re-decide it, and
        // not consume a step or call the model.
        let mut events = kernel.subscribe();
        let run = kernel.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
        assert!(
            run.steps.is_empty(),
            "re-surfacing on resume must not consume a step"
        );
        let mut saw_request = false;
        while let Ok(env) = events.try_recv() {
            if let kernel_core::RunEvent::AgentDelegationRequested { agent, .. } = &env.event {
                if agent == &action {
                    saw_request = true;
                }
            }
        }
        assert!(
            saw_request,
            "the pause must be re-emitted on resume, so a client reconnecting after a restart \
             sees it"
        );

        // Now decide and confirm the run actually resumes and completes.
        kernel
            .decide_delegation(
                run_id,
                &action,
                AgentDelegationDecision::AllowOnce,
                "operator",
            )
            .await
            .unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::Completed);

        eprintln!(
            "paused-delegation restart-survival OK: run {run_id} re-surfaced after reopen, \
             decided, and completed"
        );
    }
}
