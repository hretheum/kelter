//! The human gate across a daemon restart, on the real SQLite store (ADR
//! 0038): a run paused on an action approval survives two dropped-and-
//! reopened kernels, is re-surfaced (not executed) by the startup resume
//! sweep, and executes its action exactly once after a human's allow. The
//! timeout sweeper turns silence into a deny.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use daemon::approval_sweep::sweep_timed_out_action_approvals;
use kernel_core::{
    ActionApprovalDecision, ActionGateConfig, ActionPolicy, ActionSchema, ActionSource, ActionSpec,
    ExecutionContext, ExecutionError, JournalEntry, Kernel, KernelConfig, Latitude, PolicyContext,
    PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus, RunStore, ValidatedAction,
    IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;

struct Script(Mutex<Vec<ProposedAction>>, AtomicUsize);

#[async_trait]
impl PolicyOracle for Script {
    async fn propose(
        &self,
        _c: &PolicyContext,
        _s: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        let mut s = self.0.lock().await;
        if s.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(s.remove(0).into())
    }
    fn describe(&self) -> String {
        "mock:restart".into()
    }
}

/// A keyed, mutating `write`; the effect counter outlives the kernels, like
/// the outside world does.
struct Write(Arc<AtomicUsize>);

#[async_trait]
impl ActionSource for Write {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "write".into(),
                description: "write (side effect)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"entry": {"type": "string"}, IDEMPOTENCY_KEY_ARG: {"type": "string"}},
                    "required": ["entry"]
                }),
            }],
        })
    }
    async fn execute(
        &self,
        _a: &ValidatedAction,
        _c: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"written": true}))
    }
}

fn p(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

fn kernel(
    db: &std::path::Path,
    script: Vec<ProposedAction>,
    effects: Arc<AtomicUsize>,
) -> (Arc<Kernel>, Arc<dyn RunStore>, Arc<Script>) {
    let store: Arc<dyn RunStore> =
        Arc::new(storage::SqliteRunStore::new(storage::Db::open(db).unwrap()));
    let oracle = Arc::new(Script(Mutex::new(script), AtomicUsize::new(0)));
    let k = Kernel::new(
        oracle.clone(),
        Arc::new(Write(effects)),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig {
            reflection_enabled: false,
            ..KernelConfig::default()
        },
    )
    .with_action_gate(ActionGateConfig {
        default: ActionPolicy::Allow,
        actions: HashMap::from([("write".to_string(), ActionPolicy::Ask)]),
        ask_timeout_secs: 60,
    });
    (Arc::new(k), store, oracle)
}

fn tmp_db() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-gate-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("kelter.db")
}

#[tokio::test]
async fn paused_approval_survives_a_restart_and_executes_once_after_allow() {
    let db = tmp_db();
    let effects = Arc::new(AtomicUsize::new(0));

    // Session 1: the run pauses; the process "dies" (kernel dropped).
    let run_id = {
        let (k, _, _) = kernel(
            &db,
            vec![p("write", json!({"entry": "x"}))],
            effects.clone(),
        );
        let id = k.create_run("t".into()).await.unwrap();
        let run = k.execute_run(id).await.unwrap();
        assert_eq!(run.status, RunStatus::AwaitingActionApproval);
        id
    };
    assert_eq!(effects.load(Ordering::SeqCst), 0);

    // Session 2: the startup sweep picks it up — re-surfaced, not executed,
    // and the model is not asked anything.
    {
        let (k, store, oracle) = kernel(&db, vec![], effects.clone());
        assert!(store.unfinished_runs().await.unwrap().contains(&run_id));
        let run = k.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::AwaitingActionApproval);
        assert_eq!(run.pending_action_approval.unwrap().action, "write");
        assert_eq!(oracle.1.load(Ordering::SeqCst), 0);
    }
    assert_eq!(effects.load(Ordering::SeqCst), 0);

    // Session 3: a human allows; the run resumes and finishes.
    let (k, store, oracle) = kernel(
        &db,
        vec![p("finish", json!({"summary": "done"}))],
        effects.clone(),
    );
    k.decide_action_approval(run_id, "write", ActionApprovalDecision::Allow, "alice")
        .await
        .unwrap();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(effects.load(Ordering::SeqCst), 1, "exactly once");
    assert_eq!(oracle.1.load(Ordering::SeqCst), 1, "only the finish call");
    let journal = store.journal(run_id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::ActionApprovalDecided { principal, .. } if principal == "alice"
    )));

    // A further restart resumes nothing and repeats nothing.
    drop(k);
    let (k, store, _) = kernel(&db, vec![], effects.clone());
    assert!(!store.unfinished_runs().await.unwrap().contains(&run_id));
    k.execute_run(run_id).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timeout_sweeper_denies_and_the_run_resumes_without_the_action() {
    let db = tmp_db();
    let effects = Arc::new(AtomicUsize::new(0));
    let (k, store, _) = kernel(
        &db,
        vec![
            p("write", json!({"entry": "x"})),
            p("finish", json!({"summary": "gave up"})),
        ],
        effects.clone(),
    );
    let id = k.create_run("t".into()).await.unwrap();
    let paused = k.execute_run(id).await.unwrap();
    let requested_at = paused.pending_action_approval.unwrap().requested_at;

    // Before the timeout: untouched.
    assert!(sweep_timed_out_action_approvals(&k, 60, requested_at + 59)
        .await
        .is_empty());
    // At the timeout: denied by system:timeout.
    let swept = sweep_timed_out_action_approvals(&k, 60, requested_at + 60).await;
    assert_eq!(swept, vec![id]);
    let run = k.execute_run(id).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    // No effective action → incomplete (ADR 0033), never completed.
    assert_eq!(run.status, RunStatus::Incomplete);
    let journal = store.journal(id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::ActionApprovalDecided { principal, decision: ActionApprovalDecision::Deny, .. }
            if principal == "system:timeout"
    )));
}
