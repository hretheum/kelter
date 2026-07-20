//! ADR 0022, Part 2 — durable multi-branch resume: a crash with MULTIPLE
//! in-flight A2A branches (each with a dangling `Intent`, no `Receipt`)
//! must, on restart, reconnect to ALL of them concurrently — not just the
//! first — and never re-delegate. Mirrors the two-session SQLite idiom
//! used by `apps/desktop/src-tauri/tests/durable_store.rs` and ADR 0021's
//! `paused_run_survives_a_restart` (crates/skills/tests/ask_activation_test.rs),
//! but against TWO real mock A2A HTTP servers (axum) rather than a mock
//! `ActionSource`, so "reconnect, not re-delegate" is proven at the actual
//! wire level, not just reasoned about.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{A2aActionSource, RemoteAgent};
use kernel_core::{
    ActionSchema, Kernel, KernelConfig, PolicyContext, PolicyError, PolicyOracle, Proposal,
    ProposedAction, RunMode, RunStatus, RunStore,
};
use memory::InMemoryProvider;

/// One mock remote agent: reports `TASK_STATE_WORKING` on every `GetTask`
/// until `release` flips true, at which point it reports
/// `TASK_STATE_COMPLETED` — lets the test hold a delegation "in flight"
/// (dangling Intent, no Receipt) across the simulated crash, then let it
/// resolve quickly in the resumed session.
#[derive(Default)]
struct MockAgentState {
    send_calls: AtomicUsize,
    get_calls: AtomicUsize,
    release: AtomicBool,
    endpoint: std::sync::Mutex<String>,
}

async fn card(State(st): State<Arc<MockAgentState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": "durable-agent",
        "description": "mock",
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

async fn rpc(State(st): State<Arc<MockAgentState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    match method {
        "SendMessage" => {
            st.send_calls.fetch_add(1, Ordering::SeqCst);
            Json(reply(json!({
                "task": {"id": "durable-task", "status": {"state": "TASK_STATE_SUBMITTED"}, "artifacts": []}
            })))
        }
        "GetTask" => {
            st.get_calls.fetch_add(1, Ordering::SeqCst);
            let state = if st.release.load(Ordering::SeqCst) {
                "TASK_STATE_COMPLETED"
            } else {
                "TASK_STATE_WORKING"
            };
            let artifacts = if state == "TASK_STATE_COMPLETED" {
                json!([{"artifactId": "a1", "parts": [{"text": "remote result"}]}])
            } else {
                json!([])
            };
            Json(reply(json!({
                "id": "durable-task", "status": {"state": state}, "artifacts": artifacts
            })))
        }
        other => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        })),
    }
}

async fn spawn_agent(state: Arc<MockAgentState>) -> String {
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

/// Oracle that proposes one fixed plan (mirrors `plan_test.rs`'s
/// `PlanOracle` — fixtures are intentionally duplicated per test binary).
struct FixedPlanOracle {
    plan: Value,
}

#[async_trait]
impl PolicyOracle for FixedPlanOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let action = &schema.actions[0].name;
        Ok(ProposedAction {
            action: action.clone(),
            arguments: self.plan.clone(),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:fixed-plan".into()
    }
}

fn tmp_db_path() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kelter-durable-multibranch-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("kelter.db")
}

#[tokio::test]
async fn durable_multi_branch_resume_reconnects_all_in_flight() {
    // Two independent remote agents, both held "in flight" (never
    // completing) until the test explicitly releases them.
    let agent_a_state = Arc::new(MockAgentState::default());
    let agent_b_state = Arc::new(MockAgentState::default());
    let agent_a_base = spawn_agent(agent_a_state.clone()).await;
    let agent_b_base = spawn_agent(agent_b_state.clone()).await;

    let plan = json!({"nodes": [
        {"id": "a", "type": "a2a", "action": "delegate_to_agent_a", "arguments": {"task": "branch a"}, "deps": []},
        {"id": "b", "type": "a2a", "action": "delegate_to_agent_b", "arguments": {"task": "branch b"}, "deps": []}
    ]});

    let db_path = tmp_db_path();
    let run_id;

    // --- session 1: both delegations fire, both go durably in-flight, then
    //     "the daemon crashes" (the spawned execute_run task is aborted
    //     while both remote tasks are still TASK_STATE_WORKING). ----------
    {
        let store: Arc<dyn RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));
        let tools = Arc::new(
            A2aActionSource::new(vec![
                RemoteAgent {
                    name: "agent_a".into(),
                    card_url: agent_a_base.clone(),
                    token: None,
                },
                RemoteAgent {
                    name: "agent_b".into(),
                    card_url: agent_b_base.clone(),
                    token: None,
                },
            ])
            .unwrap(),
        );
        let kernel = Kernel::new(
            Arc::new(FixedPlanOracle { plan: plan.clone() }),
            tools,
            Arc::new(InMemoryProvider::new()),
            store.clone(),
            KernelConfig {
                mode: RunMode::Plan,
                ..KernelConfig::default()
            },
        );

        run_id = kernel
            .create_run("durable multi-branch".into())
            .await
            .unwrap();
        let handle = tokio::spawn(async move {
            let _ = kernel.execute_run(run_id).await;
        });

        // Wait until BOTH branches have journaled their remote task id
        // (proof that SendMessage fired for both and the durability pivot
        // note was written) — i.e. both are durably in-flight, not just
        // one.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let journal = store.journal(run_id).await.unwrap();
            let notes: std::collections::HashSet<u32> = journal
                .iter()
                .filter_map(|e| match e {
                    kernel_core::JournalEntry::Note { step, key, .. } if key == "a2a_task_id" => {
                        Some(*step)
                    }
                    _ => None,
                })
                .collect();
            if notes.contains(&0) && notes.contains(&1) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for both branches to go durably in-flight; notes so far: \
                 {notes:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // "The daemon crashes": forcibly cancel the in-flight execution —
        // both branches are left with a journaled Intent + Note, no Receipt.
        handle.abort();
        let _ = handle.await;

        let journal = store.journal(run_id).await.unwrap();
        let receipts = journal
            .iter()
            .filter(|e| matches!(e, kernel_core::JournalEntry::Receipt { .. }))
            .count();
        assert_eq!(
            receipts, 0,
            "both branches must still be dangling (no Receipt) at the simulated crash point"
        );
    } // store + kernel dropped — "the daemon process ended"

    assert_eq!(agent_a_state.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent_b_state.send_calls.load(Ordering::SeqCst), 1);

    // Now let both remote agents resolve promptly, so session 2's reconnect
    // completes quickly instead of polling forever.
    agent_a_state.release.store(true, Ordering::SeqCst);
    agent_b_state.release.store(true, Ordering::SeqCst);

    // --- session 2: reopen at the same path ("daemon restarted") ---------
    {
        let store: Arc<dyn RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));
        let tools = Arc::new(
            A2aActionSource::new(vec![
                RemoteAgent {
                    name: "agent_a".into(),
                    card_url: agent_a_base,
                    token: None,
                },
                RemoteAgent {
                    name: "agent_b".into(),
                    card_url: agent_b_base,
                    token: None,
                },
            ])
            .unwrap(),
        );
        let kernel = Kernel::new(
            Arc::new(FixedPlanOracle { plan }),
            tools,
            Arc::new(InMemoryProvider::new()),
            store,
            KernelConfig {
                mode: RunMode::Plan,
                ..KernelConfig::default()
            },
        );

        // Mirrors the startup resume sweep calling `execute_run` again for
        // every unfinished run.
        let run = kernel.execute_run(run_id).await.unwrap();

        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(
            run.steps.len(),
            2,
            "both branches must have completed after reconnect"
        );
        assert!(run.steps.iter().all(|s| s.ok));

        eprintln!(
            "durable_multi_branch_resume_reconnects_all_in_flight: run {run_id} completed after \
             reconnecting to both in-flight branches; a2a_task_id notes proved both were \
             in-flight at the crash point"
        );
    }

    // The load-bearing assertion: reconnect, never re-delegate — SendMessage
    // was called EXACTLY once per agent across BOTH sessions combined.
    assert_eq!(
        agent_a_state.send_calls.load(Ordering::SeqCst),
        1,
        "agent_a must not have been re-delegated to on resume"
    );
    assert_eq!(
        agent_b_state.send_calls.load(Ordering::SeqCst),
        1,
        "agent_b must not have been re-delegated to on resume"
    );
    assert!(agent_a_state.get_calls.load(Ordering::SeqCst) >= 1);
    assert!(agent_b_state.get_calls.load(Ordering::SeqCst) >= 1);
}
