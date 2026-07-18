//! Plan-mode execution + durability at the unit level with mocks:
//! - a validated multi-node plan runs each node through the action seam;
//! - an invalid plan (cycle) is rejected before any node executes;
//! - the plan is a memoized decision — resume replays it without querying
//!   the model, and completed nodes are not re-executed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunMode, RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Oracle that returns a fixed plan proposal (and counts calls).
struct PlanOracle {
    plan: serde_json::Value,
    calls: AtomicUsize,
    /// review verdict to return when asked (approve).
    approve: bool,
}

impl PlanOracle {
    fn new(plan: serde_json::Value, approve: bool) -> Arc<Self> {
        Arc::new(Self {
            plan,
            calls: AtomicUsize::new(0),
            approve,
        })
    }
}

#[async_trait]
impl PolicyOracle for PlanOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let action = &schema.actions[0].name;
        if action == "propose_plan" {
            Ok(ProposedAction {
                action: action.clone(),
                arguments: self.plan.clone(),
                rationale: None,
            }
            .into())
        } else if action == "review_verdict" {
            Ok(ProposedAction {
                action: action.clone(),
                arguments: json!({"approve": self.approve, "reason": "test"}),
                rationale: None,
            }
            .into())
        } else {
            Err(PolicyError::Backend(format!("unexpected action {action}")))
        }
    }
    fn describe(&self) -> String {
        "mock:plan".into()
    }
}

/// Tool source with one recording tool + an A2A-shaped delegate action.
struct PlanTools {
    executions: Mutex<Vec<String>>,
}
impl PlanTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            executions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ActionSource for PlanTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "record".into(),
                    description: "record".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"entry": {"type": "string"}, "idempotency_key": {"type":"string"}},
                        "required": ["entry"],
                        "additionalProperties": false
                    }),
                },
                ActionSpec {
                    name: "delegate_to_toy".into(),
                    description: "delegate".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"task": {"type": "string"}},
                        "required": ["task"],
                        "additionalProperties": false
                    }),
                },
            ],
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
            .push(action.action().to_string());
        Ok(json!({"ran": action.action()}))
    }
}

fn kernel(oracle: Arc<PlanOracle>, tools: Arc<PlanTools>, store: Arc<dyn RunStore>) -> Kernel {
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            latitude: Latitude::Standard,
            max_steps: 12,
            ..KernelConfig::default()
        },
    )
}

fn linear_plan() -> serde_json::Value {
    json!({"nodes": [
        {"id": "n1", "type": "tool", "action": "record", "arguments": {"entry": "a"}, "deps": []},
        {"id": "n2", "type": "a2a", "action": "delegate_to_toy", "arguments": {"task": "t"}, "deps": ["n1"]},
        {"id": "n3", "type": "review", "deps": ["n2"]}
    ]})
}

#[tokio::test]
async fn plan_runs_all_node_types_in_order() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = PlanOracle::new(linear_plan(), true);
    let tools = PlanTools::new();
    let k = kernel(oracle.clone(), tools.clone(), store.clone());

    let run = k.run_task("do the plan".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    // record + delegate executed via the tool seam (review is kernel-owned).
    let ex = tools.executions.lock().await.clone();
    assert_eq!(ex, vec!["record", "delegate_to_toy"]);
    // 3 node steps recorded.
    assert_eq!(run.steps.len(), 3);
    // Model queried: once for the plan, once for the review verdict.
    // (Per-agent accounting for a real a2a node is covered in the a2a crate
    // tests + smoke; this mock tool source doesn't attribute.)
    assert_eq!(oracle.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn invalid_plan_cycle_rejected_before_execution() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let cyclic = json!({"nodes": [
        {"id": "a", "type": "tool", "action": "record", "arguments": {"entry":"x"}, "deps": ["b"]},
        {"id": "b", "type": "tool", "action": "record", "arguments": {"entry":"y"}, "deps": ["a"]}
    ]});
    let oracle = PlanOracle::new(cyclic, true);
    let tools = PlanTools::new();
    let k = kernel(oracle, tools.clone(), store);

    let run = k.run_task("cyclic".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.unwrap().contains("cycle"));
    // Nothing executed — rejected before any node ran.
    assert!(tools.executions.lock().await.is_empty());
}

#[tokio::test]
async fn unknown_action_plan_rejected() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let bad = json!({"nodes": [
        {"id": "a", "type": "tool", "action": "nonexistent", "arguments": {}, "deps": []}
    ]});
    let k = kernel(PlanOracle::new(bad, true), PlanTools::new(), store);
    let run = k.run_task("bad".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.unwrap().contains("not in the registry"));
}

#[tokio::test]
async fn review_reject_fails_the_plan() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    // approve=false → the review node fails the plan (fail-on-node-error).
    let oracle = PlanOracle::new(linear_plan(), false);
    let tools = PlanTools::new();
    let k = kernel(oracle, tools, store);
    let run = k.run_task("plan".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.unwrap().contains("n3"));
}

#[tokio::test]
async fn resume_replays_plan_and_skips_completed_nodes() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = PlanOracle::new(linear_plan(), true);
    let tools = PlanTools::new();
    let k = kernel(oracle.clone(), tools.clone(), store.clone());

    // Seed a run whose journal already has the plan + n1's receipt (as if a
    // crash happened after node 0 completed).
    let run_id = k.create_run("resumed plan".into()).await.unwrap();
    let plan: kernel_core::Plan =
        serde_json::from_value(linear_plan()["nodes"].clone().pipe_into_plan()).unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan })
        .await
        .unwrap();
    // n1 is node index 0 in declaration order → journal receipt at step 0.
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 0,
                ok: true,
                result: json!({"ran": "record"}),
            },
        )
        .await
        .unwrap();

    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    // n1 (record) was NOT re-executed; only delegate ran.
    let ex = tools.executions.lock().await.clone();
    assert_eq!(
        ex,
        vec!["delegate_to_toy"],
        "completed node must not re-run"
    );
    // Model NOT queried for the plan (it was journaled); only the review
    // verdict call happens.
    assert_eq!(
        oracle.calls.load(Ordering::SeqCst),
        1,
        "resume must replay the journaled plan without re-proposing it"
    );
}

// Small helper so the test above can turn the plan array back into the
// `{nodes: [...]}` object the Plan type deserializes from.
trait PipeIntoPlan {
    fn pipe_into_plan(self) -> serde_json::Value;
}
impl PipeIntoPlan for serde_json::Value {
    fn pipe_into_plan(self) -> serde_json::Value {
        json!({ "nodes": self })
    }
}
