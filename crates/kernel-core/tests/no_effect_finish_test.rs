//! ADR 0033 — "success without action" (K12 acceptance finding N2).
//!
//! On K12 an infeasible task ("send an e-mail", no such tool) ended
//! `completed`: the model called `finish` straight away. These tests drive
//! the real kernel with a scripted oracle and pin the fix: the model may
//! propose `finish`, but only the kernel grants success, and it does so only
//! when at least one dispatched action came back ok.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunEvent, RunMode, RunStatus, RunStore, ValidatedAction,
};
use memory::{EpisodeOutcome, InMemoryProvider, MemoryProvider};

/// `echo` always succeeds; `send_email` always fails (the K12 shape: the
/// capability the task needs is not really there).
struct Tools;

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        let p = json!({
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
            "additionalProperties": false
        });
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "echo".into(),
                    description: "Echo a message".into(),
                    parameters: p.clone(),
                },
                ActionSpec {
                    name: "send_email".into(),
                    description: "Always fails".into(),
                    parameters: p,
                },
            ],
        })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        match action.action() {
            "echo" => Ok(json!({"echoed": action.arguments()["message"]})),
            _ => Err(ExecutionError::Tool("no mail transport configured".into())),
        }
    }
}

/// Step mode: replays a fixed script. Plan mode: answers `propose_plan`
/// with `plan` and every `review_verdict` with approve.
struct Scripted {
    script: Vec<ProposedAction>,
    plan: serde_json::Value,
    cursor: AtomicUsize,
}

impl Scripted {
    fn steps(script: Vec<ProposedAction>) -> Arc<Self> {
        Arc::new(Self {
            script,
            plan: json!(null),
            cursor: AtomicUsize::new(0),
        })
    }
    fn plan(plan: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            script: Vec::new(),
            plan,
            cursor: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl PolicyOracle for Scripted {
    async fn propose(
        &self,
        _context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        match schema.actions[0].name.as_str() {
            "propose_plan" => {
                return Ok(propose("propose_plan", self.plan.clone()).into());
            }
            "review_verdict" => {
                return Ok(
                    propose("review_verdict", json!({"approve": true, "reason": "ok"})).into(),
                );
            }
            _ => {}
        }
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.script
            .get(i)
            .cloned()
            .map(Proposal::from)
            .ok_or_else(|| PolicyError::Backend("script exhausted".into()))
    }

    fn describe(&self) -> String {
        "mock:scripted".into()
    }
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

fn finish(summary: &str) -> ProposedAction {
    propose("finish", json!({ "summary": summary }))
}

struct Harness {
    kernel: Kernel,
    store: Arc<dyn RunStore>,
    memory: Arc<InMemoryProvider>,
}

fn harness(oracle: Arc<Scripted>, mode: RunMode) -> Harness {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let memory = Arc::new(InMemoryProvider::new());
    let kernel = Kernel::new(
        oracle,
        Arc::new(Tools),
        memory.clone() as Arc<dyn MemoryProvider>,
        store.clone(),
        KernelConfig {
            mode,
            latitude: Latitude::Standard,
            ..KernelConfig::default()
        },
    );
    Harness {
        kernel,
        store,
        memory,
    }
}

async fn terminal_status(store: &Arc<dyn RunStore>, run_id: uuid::Uuid) -> Option<RunStatus> {
    store
        .journal(run_id)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e {
            JournalEntry::Terminal { status, .. } => Some(status),
            _ => None,
        })
}

// ---- step mode -------------------------------------------------------------

#[tokio::test]
async fn immediate_finish_is_incomplete_not_completed() {
    // The exact K12 trace: an infeasible task, and `finish` as step 0.
    let h = harness(
        Scripted::steps(vec![finish("The e-mail has been sent.")]),
        RunMode::Step,
    );
    let mut rx = h.kernel.subscribe();
    let run = h
        .kernel
        .run_task("send an e-mail to the team".into())
        .await
        .unwrap();

    assert_eq!(run.status, RunStatus::Incomplete);
    assert!(run.steps.is_empty());
    // The model's claim survives as output — unverified, not discarded…
    assert_eq!(
        run.output.as_ref().unwrap()["summary"],
        "The e-mail has been sent."
    );
    // …and the kernel says why it withheld success.
    let err = run.error.as_deref().unwrap();
    assert!(err.contains("without executing any action"), "{err}");

    // Durable: the journal's terminal entry is Incomplete too, so a replay
    // or a reader of the journal reaches the same verdict.
    assert_eq!(
        terminal_status(&h.store, run.id).await,
        Some(RunStatus::Incomplete)
    );
    assert_eq!(
        h.store.get(run.id).await.unwrap().status,
        RunStatus::Incomplete
    );

    // The terminal event is `incomplete`, never `completed`.
    let mut saw_incomplete = false;
    while let Ok(env) = rx.try_recv() {
        match env.event {
            RunEvent::Completed { .. } => panic!("an action-less run must not emit Completed"),
            RunEvent::Incomplete { output, reason, .. } => {
                assert_eq!(output.unwrap()["summary"], "The e-mail has been sent.");
                assert!(reason.contains("without executing any action"));
                saw_incomplete = true;
            }
            _ => {}
        }
    }
    assert!(saw_incomplete, "expected an Incomplete event");

    // Reflection records it as a failure, so nothing learns from it as a
    // success (distillation only reads Completed runs).
    let eps = h.memory.episodes().await;
    assert_eq!(eps.len(), 1, "a reflection episode is still written");
    assert_eq!(eps[0].outcome, EpisodeOutcome::Failure);
}

#[tokio::test]
async fn finish_after_only_failed_actions_is_incomplete() {
    let h = harness(
        Scripted::steps(vec![
            propose("send_email", json!({"message": "hi"})),
            finish("Sent."),
        ]),
        RunMode::Step,
    );
    let run = h.kernel.run_task("send an e-mail".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(run.steps.len(), 1);
    assert!(!run.steps[0].ok);
    let err = run.error.as_deref().unwrap();
    assert!(err.contains("1 attempted, all failed"), "{err}");
}

#[tokio::test]
async fn finish_after_a_successful_action_still_completes() {
    // Regression guard: the ordinary path is untouched.
    let h = harness(
        Scripted::steps(vec![
            propose("send_email", json!({"message": "hi"})),
            propose("echo", json!({"message": "fallback"})),
            finish("Echoed instead."),
        ]),
        RunMode::Step,
    );
    let run = h.kernel.run_task("say something".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.error, None);
    assert_eq!(run.output.unwrap()["summary"], "Echoed instead.");
    assert_eq!(
        terminal_status(&h.store, run.id).await,
        Some(RunStatus::Completed)
    );
}

#[tokio::test]
async fn incomplete_run_is_terminal_never_resumed() {
    let h = harness(Scripted::steps(vec![finish("done")]), RunMode::Step);
    let run = h.kernel.run_task("anything".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Incomplete);
    assert!(
        !h.store.unfinished_runs().await.unwrap().contains(&run.id),
        "Incomplete is terminal — a restart must not resume it"
    );
}

// ---- plan mode -------------------------------------------------------------

#[tokio::test]
async fn plan_with_a_successful_tool_node_completes() {
    let h = harness(
        Scripted::plan(json!({"nodes": [
            {"id": "a", "type": "tool", "action": "echo", "arguments": {"message": "x"}, "deps": []},
            {"id": "r", "type": "review", "deps": ["a"]}
        ]})),
        RunMode::Plan,
    );
    let run = h.kernel.run_task("echo x".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
}

#[tokio::test]
async fn review_only_plan_is_incomplete() {
    // A plan that dispatches nothing — only a kernel-owned review verdict —
    // runs "successfully" node by node but has done no work. Same predicate
    // as step mode.
    let h = harness(
        Scripted::plan(json!({"nodes": [
            {"id": "r", "type": "review", "deps": []}
        ]})),
        RunMode::Plan,
    );
    let run = h.kernel.run_task("send an e-mail".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Incomplete, "{:?}", run.error);
    assert!(run.error.unwrap().contains("without executing any action"));
    // The plan summary is preserved as output, as for a completed plan.
    assert!(run.output.unwrap()["summary"]
        .as_str()
        .unwrap()
        .starts_with("plan completed"));
}

#[tokio::test]
async fn review_node_naming_a_tool_does_not_count_as_that_tool() {
    // A review node never dispatches its `action`. It must be recorded under
    // the kernel-owned name, or a plan could launder a verdict into an
    // "executed send_email" step and through the success predicate.
    let h = harness(
        Scripted::plan(json!({"nodes": [
            {"id": "r", "type": "review", "action": "echo", "arguments": {"message": "x"}, "deps": []}
        ]})),
        RunMode::Plan,
    );
    let run = h.kernel.run_task("echo x".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Incomplete, "{:?}", run.error);
    assert_eq!(run.steps.len(), 1);
    assert_eq!(run.steps[0].action, "review");
}
