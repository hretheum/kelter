//! ADR 0030 — re-plan durability on the REAL SQLite store, two-session
//! idiom (mirrors `multiagent_durable_resume_test.rs`): session 1 "crashes"
//! exactly at the re-plan boundary — the original plan's failure is durable
//! (journaled receipts), the revised plan is NOT yet journaled — and session
//! 2, a fresh kernel over the same DB file, must resume deterministically:
//! replay the journaled plan and its receipts (model NOT re-queried about
//! them), ask for the re-plan exactly once, carry the completed node forward
//! (NOT re-execute its side effect), and finish the run.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PlanResilience, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunMode, RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Pops plan proposals from a script; an empty script makes the NEXT
/// `propose_plan` call fail — which is exactly how session 1 "crashes" at
/// the re-plan boundary (the failure receipts are already durable, the
/// revised plan is not).
struct ScriptedPlanOracle {
    plans: Mutex<Vec<serde_json::Value>>,
    calls: AtomicUsize,
}

impl ScriptedPlanOracle {
    fn new(plans: Vec<serde_json::Value>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(plans),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl PolicyOracle for ScriptedPlanOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        assert_eq!(schema.actions[0].name, "propose_plan");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut plans = self.plans.lock().await;
        if plans.is_empty() {
            return Err(PolicyError::Backend(
                "simulated crash at the re-plan boundary".into(),
            ));
        }
        Ok(ProposedAction {
            action: "propose_plan".into(),
            arguments: plans.remove(0),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:replan-crash".into()
    }
}

/// `record` succeeds, `broken` always fails; every execution is recorded.
struct RecordingTools {
    executions: Mutex<Vec<(String, serde_json::Value)>>,
}

impl RecordingTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            executions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ActionSource for RecordingTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let spec = |name: &str| ActionSpec {
            name: name.into(),
            description: name.into(),
            parameters: json!({
                "type": "object",
                "properties": {"entry": {"type": "string"}},
                "required": ["entry"],
                "additionalProperties": false
            }),
        };
        Ok(ActionSchema {
            actions: vec![spec("record"), spec("broken")],
        })
    }
    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        self.executions
            .lock()
            .await
            .push((action.action().to_string(), action.arguments().clone()));
        match action.action() {
            "broken" => Err(ExecutionError::Tool("permanent tool failure".into())),
            other => Ok(json!({"ran": other, "args": action.arguments()})),
        }
    }
}

fn kernel(
    oracle: Arc<ScriptedPlanOracle>,
    tools: Arc<RecordingTools>,
    store: Arc<dyn RunStore>,
) -> Kernel {
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            plan_resilience: PlanResilience {
                max_node_retries: 0,
                max_replans: 1,
            },
            ..KernelConfig::default()
        },
    )
}

fn tmp_db_path() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kelter-plan-replan-resume-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("kelter.db")
}

#[tokio::test]
async fn replan_boundary_crash_resumes_deterministically_on_sqlite() {
    let db_path = tmp_db_path();
    let run_id;

    // --- session 1: the original plan runs (n1 succeeds durably, n2 fails
    //     durably), then the process "crashes" exactly when the re-plan is
    //     being proposed — after the failure receipts, before any
    //     PlanProposed for the revised plan. -------------------------------
    {
        let store: Arc<dyn RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));
        let gen0 = json!({"nodes": [
            {"id": "n1", "type": "tool", "action": "record",
             "arguments": {"entry": "a"}, "deps": []},
            {"id": "n2", "type": "tool", "action": "broken",
             "arguments": {"entry": "b"}, "deps": ["n1"]},
        ]});
        let oracle = ScriptedPlanOracle::new(vec![gen0]); // no re-plan scripted
        let tools = RecordingTools::new();
        let k = kernel(oracle.clone(), tools.clone(), store.clone());

        run_id = k.create_run("repair across a crash".into()).await.unwrap();
        let crashed = k.execute_run(run_id).await;
        assert!(
            crashed.is_err(),
            "session 1 must die at the re-plan proposal, not finish"
        );
        // The crash left durable truth behind: n1's success and n2's failure
        // are journaled; the run is NOT terminal.
        let ex = tools.executions.lock().await;
        assert_eq!(ex.len(), 2, "n1 and n2 executed once each: {ex:?}");
        let run = store.get(run_id).await.unwrap();
        assert_ne!(run.status, RunStatus::Completed);
        assert_ne!(run.status, RunStatus::Failed);
    }

    // --- session 2: a fresh kernel over the SAME DB file. Resume must NOT
    //     re-query the model about the journaled plan (only the un-journaled
    //     re-plan is asked for, once), must NOT re-execute n1 (carried), and
    //     must finish the run with only the genuinely new node. ------------
    {
        let store: Arc<dyn RunStore> = Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&db_path).unwrap(),
        ));
        let gen1 = json!({"nodes": [
            {"id": "n1", "type": "tool", "action": "record",
             "arguments": {"entry": "a"}, "deps": []},
            {"id": "n3", "type": "tool", "action": "record",
             "arguments": {"entry": "fixed"}, "deps": ["n1"]},
        ]});
        let oracle = ScriptedPlanOracle::new(vec![gen1]);
        let tools = RecordingTools::new();
        let k = kernel(oracle.clone(), tools.clone(), store.clone());

        let run = k.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
        let summary = run.output.as_ref().unwrap()["summary"].as_str().unwrap();
        assert!(summary.contains("after 1 re-plan"), "summary: {summary}");

        // Exactly one model call: the re-plan (the original plan replayed
        // from the journal).
        assert_eq!(
            oracle.calls.load(Ordering::SeqCst),
            1,
            "resume must not re-query the model about the journaled plan"
        );
        // Exactly-once across the crash: n1's side effect happened in
        // session 1 and is carried forward — session 2 executed ONLY n3.
        let ex = tools.executions.lock().await;
        assert_eq!(ex.len(), 1, "only n3 may execute in session 2: {ex:?}");
        assert_eq!(ex[0].0, "record");
        assert_eq!(ex[0].1["entry"], "fixed");
    }
}
