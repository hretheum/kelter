//! Plan-mode resilience (ADR 0030), proven at the unit level with mocks:
//! - bounded per-node retry: a transient failure retries and succeeds, a
//!   completed sibling is never re-executed, exhaustion fails cleanly;
//! - adaptive re-planning: a failed plan is repaired by a REVISED plan that
//!   goes through the SAME validation (an invalid re-plan is rejected — the
//!   floor holds), informed by a BOUNDED failure context;
//! - bounded: re-plan→fail→re-plan terminates after `max_replans`;
//! - durability/exactly-once: a crash around a re-plan resumes
//!   deterministically — journaled plans are not re-queried, nodes completed
//!   in ANY generation are not re-executed (carry-forward).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PlanResilience, PolicyContext, PolicyError, PolicyOracle, Proposal,
    ProposedAction, RunMode, RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Oracle that pops plan proposals from a script (one per `propose_plan`
/// call) and records the guidance each call received.
struct ScriptedPlanOracle {
    plans: Mutex<Vec<serde_json::Value>>,
    plan_calls: AtomicUsize,
    guidance_seen: Mutex<Vec<String>>,
}

impl ScriptedPlanOracle {
    fn new(plans: Vec<serde_json::Value>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(plans),
            plan_calls: AtomicUsize::new(0),
            guidance_seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PolicyOracle for ScriptedPlanOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let action = &schema.actions[0].name;
        if action != "propose_plan" {
            return Err(PolicyError::Backend(format!("unexpected action {action}")));
        }
        self.plan_calls.fetch_add(1, Ordering::SeqCst);
        self.guidance_seen
            .lock()
            .await
            .push(context.guidance.clone().unwrap_or_default());
        let mut plans = self.plans.lock().await;
        if plans.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(ProposedAction {
            action: action.clone(),
            arguments: plans.remove(0),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:scripted-plan".into()
    }
}

/// Tools: `record` always succeeds, `broken` always fails, `flaky` fails its
/// first `flaky_failures` executions then succeeds. Every execution is
/// recorded as (action, arguments) for exactly-once assertions.
struct ResilienceTools {
    executions: Mutex<Vec<(String, serde_json::Value)>>,
    flaky_failures: usize,
    flaky_calls: AtomicUsize,
}

impl ResilienceTools {
    fn new(flaky_failures: usize) -> Arc<Self> {
        Arc::new(Self {
            executions: Mutex::new(Vec::new()),
            flaky_failures,
            flaky_calls: AtomicUsize::new(0),
        })
    }
    async fn executed_actions(&self) -> Vec<String> {
        self.executions
            .lock()
            .await
            .iter()
            .map(|(a, _)| a.clone())
            .collect()
    }
}

#[async_trait]
impl ActionSource for ResilienceTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let obj = |props: serde_json::Value, req: serde_json::Value| {
            json!({"type": "object", "properties": props, "required": req,
                   "additionalProperties": false})
        };
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "record".into(),
                    description: "record".into(),
                    parameters: obj(
                        json!({"entry": {"type": "string"}, "idempotency_key": {"type": "string"}}),
                        json!(["entry"]),
                    ),
                },
                ActionSpec {
                    name: "broken".into(),
                    description: "always fails".into(),
                    parameters: obj(json!({"entry": {"type": "string"}}), json!(["entry"])),
                },
                ActionSpec {
                    name: "flaky".into(),
                    description: "fails then works".into(),
                    parameters: obj(json!({"entry": {"type": "string"}}), json!(["entry"])),
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
            .push((action.action().to_string(), action.arguments().clone()));
        match action.action() {
            "broken" => Err(ExecutionError::Tool("tool exploded (permanent)".into())),
            "flaky" => {
                let n = self.flaky_calls.fetch_add(1, Ordering::SeqCst);
                if n < self.flaky_failures {
                    Err(ExecutionError::Tool(format!("transient failure #{n}")))
                } else {
                    Ok(json!({"ran": "flaky"}))
                }
            }
            other => Ok(json!({"ran": other, "args": action.arguments()})),
        }
    }
}

fn kernel(
    oracle: Arc<ScriptedPlanOracle>,
    tools: Arc<ResilienceTools>,
    store: Arc<dyn RunStore>,
    resilience: PlanResilience,
) -> Kernel {
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            plan_resilience: resilience,
            ..KernelConfig::default()
        },
    )
}

fn tool_node(id: &str, action: &str, entry: &str, deps: &[&str]) -> serde_json::Value {
    json!({"id": id, "type": "tool", "action": action,
           "arguments": {"entry": entry}, "deps": deps})
}

// ---- bounded retry --------------------------------------------------------

#[tokio::test]
async fn transient_failure_retries_and_succeeds_without_reexecuting_sibling() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    // n1 succeeds; n2 (flaky) fails twice, succeeds on the 3rd attempt.
    let plan = json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n2", "flaky", "b", &["n1"]),
    ]});
    let oracle = ScriptedPlanOracle::new(vec![plan]);
    let tools = ResilienceTools::new(2);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 2,
            max_replans: 0,
        },
    );

    let run = k.run_task("retry the flaky node".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    let ex = tools.executed_actions().await;
    // The completed sibling ran exactly once (exactly-once through retry);
    // flaky ran 3 times (1 + 2 retries).
    assert_eq!(ex.iter().filter(|a| *a == "record").count(), 1);
    assert_eq!(ex.iter().filter(|a| *a == "flaky").count(), 3);
    // No re-plan happened: the model proposed exactly one plan.
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_exhaustion_fails_cleanly() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let plan = json!({"nodes": [tool_node("n1", "flaky", "a", &[])]});
    let oracle = ScriptedPlanOracle::new(vec![plan]);
    let tools = ResilienceTools::new(usize::MAX); // never succeeds
    let k = kernel(
        oracle,
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 2,
            max_replans: 0,
        },
    );

    let run = k.run_task("exhaust retries".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.as_deref().unwrap().contains("n1"));
    // Exactly 1 + max_node_retries attempts — bounded.
    assert_eq!(tools.executed_actions().await.len(), 3);
}

#[tokio::test]
async fn defaults_preserve_fail_fast() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let plan = json!({"nodes": [tool_node("n1", "broken", "a", &[])]});
    let oracle = ScriptedPlanOracle::new(vec![plan]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience::default(),
    );

    let run = k.run_task("no resilience configured".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("plan node 'n1' failed"));
    assert_eq!(tools.executed_actions().await.len(), 1);
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 1);
}

// ---- adaptive re-planning -------------------------------------------------

#[tokio::test]
async fn replan_repairs_a_failed_plan_with_bounded_failure_context() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    // Gen 0: n1 succeeds, n2 is permanently broken → plan fails.
    // Gen 1 (revised): carries n1 verbatim, replaces n2 with a working node.
    let gen0 = json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n2", "broken", "b", &["n1"]),
    ]});
    let gen1 = json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n3", "record", "b-fixed", &["n1"]),
    ]});
    let oracle = ScriptedPlanOracle::new(vec![gen0, gen1]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run = k.run_task("repair via re-plan".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    let summary = run.output.as_ref().unwrap()["summary"].as_str().unwrap();
    assert!(summary.contains("after 1 re-plan"), "summary: {summary}");

    // Money-shot assertions:
    // (1) the model was asked exactly twice (original + one re-plan);
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 2);
    // (2) the re-plan request was INFORMED about the failure — bounded
    //     context names the failed node, its error, and the completed node;
    let guidance = oracle.guidance_seen.lock().await;
    let replan_ctx = &guidance[1];
    assert!(replan_ctx.contains("id=n2"), "ctx: {replan_ctx}");
    assert!(replan_ctx.contains("tool exploded"), "ctx: {replan_ctx}");
    assert!(replan_ctx.contains("id=n1"), "ctx: {replan_ctx}");
    assert!(
        replan_ctx.contains("re-plan attempt 1 of 1"),
        "ctx: {replan_ctx}"
    );
    // (3) exactly-once through re-planning: the carried node n1 executed
    //     ONCE across both generations; the broken node once; the fix once.
    let ex = tools.executions.lock().await;
    assert_eq!(
        ex.iter()
            .filter(|(a, args)| a == "record" && args["entry"] == "a")
            .count(),
        1,
        "carried node must not re-execute: {ex:?}"
    );
    assert_eq!(ex.iter().filter(|(a, _)| a == "broken").count(), 1);
    assert_eq!(
        ex.iter()
            .filter(|(a, args)| a == "record" && args["entry"] == "b-fixed")
            .count(),
        1
    );
}

#[tokio::test]
async fn invalid_replan_is_rejected_and_a_corrected_one_accepted() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let gen0 = json!({"nodes": [tool_node("n1", "broken", "a", &[])]});
    // First re-plan proposes an action OUTSIDE the action space → the
    // validation floor must reject it (never execute it).
    let bad = json!({"nodes": [tool_node("n1", "rm_rf_everything", "x", &[])]});
    let good = json!({"nodes": [tool_node("n2", "record", "fixed", &[])]});
    let oracle = ScriptedPlanOracle::new(vec![gen0, bad, good]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 0,
            max_replans: 2,
        },
    );

    let run = k
        .run_task("invalid re-plan then corrected".into())
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    // The invalid action NEVER executed — the floor held through re-planning.
    let ex = tools.executed_actions().await;
    assert!(!ex.iter().any(|a| a == "rm_rf_everything"), "ex: {ex:?}");
    // 3 proposals: original, rejected re-plan, corrected re-plan.
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 3);
    // The corrected proposal was told WHY the previous one was rejected.
    let guidance = oracle.guidance_seen.lock().await;
    assert!(
        guidance[2].contains("REJECTED by validation"),
        "ctx: {}",
        guidance[2]
    );
    assert!(
        guidance[2].contains("rm_rf_everything"),
        "ctx: {}",
        guidance[2]
    );
}

#[tokio::test]
async fn invalid_replan_with_exhausted_budget_fails_cleanly() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let gen0 = json!({"nodes": [tool_node("n1", "broken", "a", &[])]});
    let bad = json!({"nodes": [tool_node("n1", "rm_rf_everything", "x", &[])]});
    let oracle = ScriptedPlanOracle::new(vec![gen0, bad]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run = k
        .run_task("invalid re-plan, no budget left".into())
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let err = run.error.unwrap();
    assert!(err.contains("plan rejected"), "err: {err}");
    assert!(err.contains("budget exhausted"), "err: {err}");
    let ex = tools.executed_actions().await;
    assert!(!ex.iter().any(|a| a == "rm_rf_everything"));
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn replanning_is_bounded_no_infinite_loop() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    // Every generation proposes a permanently-broken plan.
    let broken_plan = || json!({"nodes": [tool_node("n1", "broken", "a", &[])]});
    let oracle = ScriptedPlanOracle::new(vec![broken_plan(), broken_plan(), broken_plan()]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store,
        PlanResilience {
            max_node_retries: 0,
            max_replans: 2,
        },
    );

    let run = k.run_task("never converges".into()).await.unwrap();
    // Terminates — cleanly failed, with the budget spelled out.
    assert_eq!(run.status, RunStatus::Failed);
    let err = run.error.unwrap();
    assert!(err.contains("2 re-plan(s) exhausted"), "err: {err}");
    // Exactly 1 + max_replans proposals and as many executions — no loop.
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 3);
    assert_eq!(tools.executed_actions().await.len(), 3);
}

// ---- durability / exactly-once through re-planning ------------------------

/// Crash AFTER a re-plan was journaled: resume must replay BOTH journaled
/// plans (model NOT queried at all), carry the prior generation's completed
/// node forward (NOT re-execute it), and run only the genuinely new work.
#[tokio::test]
async fn resume_after_journaled_replan_is_deterministic_and_exactly_once() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = ScriptedPlanOracle::new(vec![]); // any propose call would fail the run
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run_id = k.create_run("resume mid-re-plan".into()).await.unwrap();
    // Generation 0 (journaled plan + its receipts): n1 ok, n2 failed.
    let gen0: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n2", "broken", "b", &["n1"]),
    ]}))
    .unwrap();
    // Generation 1 (the revised plan, journaled BEFORE the crash): carries
    // n1 verbatim, adds n3.
    let gen1: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n3", "record", "fixed", &["n1"]),
    ]}))
    .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan: gen0 })
        .await
        .unwrap();
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 0, // generation 0, node 0 (n1)
                ok: true,
                result: json!({"ran": "record", "args": {"entry": "a"}}),
            },
        )
        .await
        .unwrap();
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 1, // generation 0, node 1 (n2) — the failure
                ok: false,
                result: json!({"error": "tool exploded (permanent)"}),
            },
        )
        .await
        .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan: gen1 })
        .await
        .unwrap();

    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    // Deterministic resume: the model was NEVER queried — both plans
    // replayed from the journal.
    assert_eq!(
        oracle.plan_calls.load(Ordering::SeqCst),
        0,
        "resume must not re-query the model about journaled plans"
    );
    // Exactly-once: n1 (completed in generation 0) was NOT re-executed;
    // only the genuinely new node n3 ran in this session.
    let ex = tools.executions.lock().await;
    assert_eq!(ex.len(), 1, "only n3 may execute: {ex:?}");
    assert_eq!(ex[0].0, "record");
    assert_eq!(ex[0].1["entry"], "fixed");
}

/// Crash BEFORE the re-plan was journaled (the failure is durable, the
/// revised plan is not): resume re-asks the model for the re-plan — that
/// proposal was never journaled, so asking again IS the deterministic
/// behavior — but the prior generation's completed node still carries
/// forward and is not re-executed.
#[tokio::test]
async fn resume_before_replan_journaled_reasks_but_carries_completed_work() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let gen1 = json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n3", "record", "fixed", &["n1"]),
    ]});
    let oracle = ScriptedPlanOracle::new(vec![gen1]);
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run_id = k
        .create_run("crash before re-plan journal".into())
        .await
        .unwrap();
    let gen0: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        tool_node("n1", "record", "a", &[]),
        tool_node("n2", "broken", "b", &["n1"]),
    ]}))
    .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan: gen0 })
        .await
        .unwrap();
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 0,
                ok: true,
                result: json!({"ran": "record", "args": {"entry": "a"}}),
            },
        )
        .await
        .unwrap();
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 1,
                ok: false,
                result: json!({"error": "tool exploded (permanent)"}),
            },
        )
        .await
        .unwrap();

    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    // The re-plan was never journaled → exactly one propose call on resume.
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 1);
    // n1 still carries forward from the journaled generation-0 receipt.
    let ex = tools.executions.lock().await;
    assert_eq!(ex.len(), 1, "only n3 may execute: {ex:?}");
    assert_eq!(ex[0].1["entry"], "fixed");
}

/// The re-plan budget survives a crash: with max_replans=1 already consumed
/// by a journaled generation-1 plan that ALSO fails, resume must fail
/// cleanly instead of asking for a third plan.
#[tokio::test]
async fn replan_budget_holds_across_resume() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = ScriptedPlanOracle::new(vec![]); // any propose call would fail the run
    let tools = ResilienceTools::new(0);
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run_id = k.create_run("budget across resume".into()).await.unwrap();
    let gen0: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        tool_node("n1", "broken", "a", &[]),
    ]}))
    .unwrap();
    let gen1: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        tool_node("n2", "broken", "b", &[]),
    ]}))
    .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan: gen0 })
        .await
        .unwrap();
    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step: 0,
                ok: false,
                result: json!({"error": "tool exploded (permanent)"}),
            },
        )
        .await
        .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan: gen1 })
        .await
        .unwrap();

    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let err = run.error.unwrap();
    assert!(err.contains("re-plan(s) exhausted"), "err: {err}");
    // The model was never asked for a THIRD plan.
    assert_eq!(oracle.plan_calls.load(Ordering::SeqCst), 0);
    // Only generation 1's node executed in this session (gen 0's failure
    // replayed from its receipt).
    let ex = tools.executions.lock().await;
    assert_eq!(ex.len(), 1, "ex: {ex:?}");
    assert_eq!(ex[0].1["entry"], "b");
}
