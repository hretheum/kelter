//! Parallel plan fan-out + governed delegation (ADR 0022). Extends
//! `plan_test.rs` (sequential plan-mode + durability) with: concurrent
//! execution of independent nodes (real wall-clock proof), dependency
//! ordering under the new readiness-driven scheduler, the delegation
//! budget/depth/cycle governance refusals, and the `branch` event
//! dimension. Fixtures are intentionally duplicated from `plan_test.rs`
//! (each `tests/*.rs` file is its own binary, per this repo's convention).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, DelegationLimits, ExecutionContext, ExecutionError,
    Kernel, KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal,
    ProposedAction, RunEvent, RunMode, RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Oracle that returns one fixed plan proposal (mirrors `plan_test.rs`'s
/// `PlanOracle`, minus the review-verdict branch — none of these tests need
/// a review node).
struct FixedPlanOracle {
    plan: serde_json::Value,
}

impl FixedPlanOracle {
    fn new(plan: serde_json::Value) -> Arc<Self> {
        Arc::new(Self { plan })
    }
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

/// A tool source declaring both plain-tool actions (`step_a`/`step_b`) and
/// A2A-shaped `delegate_to_*` actions, each optionally delayed (proves real
/// concurrency via wall-clock timing) and each declaring a fake but stable
/// origin (mirrors `A2aActionSource::a2a_origin` — required for the
/// delegation-governance checks, ADR 0022, to resolve a target at all).
struct TimedTools {
    delays: HashMap<String, Duration>,
    origins: HashMap<String, String>,
    executions: Mutex<Vec<(String, std::time::Instant)>>,
}

impl TimedTools {
    fn new(delays: &[(&str, Duration)], origins: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            delays: delays.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            origins: origins
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            executions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ActionSource for TimedTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let plain = |name: &str| ActionSpec {
            name: name.into(),
            description: "".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        };
        let delegate = |name: &str| ActionSpec {
            name: name.into(),
            description: "".into(),
            parameters: json!({
                "type": "object",
                "properties": {"task": {"type": "string"}},
                "required": ["task"],
                "additionalProperties": false
            }),
        };
        Ok(ActionSchema {
            actions: vec![
                plain("step_a"),
                plain("step_b"),
                delegate("delegate_to_a"),
                delegate("delegate_to_b"),
                delegate("delegate_to_c"),
                delegate("delegate_to_toy"),
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
            .push((action.action().to_string(), std::time::Instant::now()));
        if let Some(d) = self.delays.get(action.action()) {
            tokio::time::sleep(*d).await;
        }
        Ok(json!({"ran": action.action()}))
    }

    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.origins.get(action).cloned()
    }
}

fn kernel_with_limits(
    oracle: Arc<dyn PolicyOracle>,
    tools: Arc<dyn ActionSource>,
    store: Arc<dyn RunStore>,
    delegation: DelegationLimits,
) -> Kernel {
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            latitude: Latitude::Standard,
            max_steps: 12,
            delegation,
            ..KernelConfig::default()
        },
    )
}

#[tokio::test]
async fn parallel_independent_nodes_run_concurrently() {
    let delay = Duration::from_millis(250);
    let tools = TimedTools::new(
        &[
            ("delegate_to_a", delay),
            ("delegate_to_b", delay),
            ("delegate_to_c", delay),
        ],
        &[
            ("delegate_to_a", "mock://a"),
            ("delegate_to_b", "mock://b"),
            ("delegate_to_c", "mock://c"),
        ],
    );
    let plan = json!({"nodes": [
        {"id": "a", "type": "a2a", "action": "delegate_to_a", "arguments": {"task": "x"}, "deps": []},
        {"id": "b", "type": "a2a", "action": "delegate_to_b", "arguments": {"task": "x"}, "deps": []},
        {"id": "c", "type": "a2a", "action": "delegate_to_c", "arguments": {"task": "x"}, "deps": []}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel_with_limits(oracle, tools.clone(), store, DelegationLimits::default());

    let started = std::time::Instant::now();
    let run = k.run_task("parallel fan-out".into()).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(tools.executions.lock().await.len(), 3);
    for (name, t) in tools.executions.lock().await.iter() {
        eprintln!("  {name} started at {:?}", t.duration_since(started));
    }
    eprintln!(
        "parallel_independent_nodes_run_concurrently: 3 nodes x {delay:?} delay each, wall clock \
         = {elapsed:?} (sequential would be ~{:?})",
        delay * 3
    );
    // Threshold: delay + a generous fixed allowance for scheduler/admission
    // overhead (observed ~70ms on a busy dev machine) — comfortably under
    // the 3x-delay sequential bound (750ms here), so only genuine
    // concurrency (not sequential execution with some slack) can pass it.
    let threshold = delay + Duration::from_millis(350);
    assert!(
        elapsed < threshold,
        "expected concurrent completion well under {threshold:?} (sequential would be ~{:?}), \
         got {elapsed:?}",
        delay * 3
    );
}

#[tokio::test]
async fn dependent_node_waits_for_its_dependency() {
    let delay = Duration::from_millis(150);
    let tools = TimedTools::new(&[("step_a", delay)], &[]);
    let plan = json!({"nodes": [
        {"id": "up", "type": "tool", "action": "step_a", "arguments": {}, "deps": []},
        {"id": "down", "type": "tool", "action": "step_b", "arguments": {}, "deps": ["up"]}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel_with_limits(oracle, tools.clone(), store, DelegationLimits::default());

    let run = k.run_task("diamond".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    let ex = tools.executions.lock().await;
    let names: Vec<&str> = ex.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["step_a", "step_b"],
        "downstream node must not start before its dependency completes, even under the \
         readiness-driven concurrent scheduler"
    );
}

#[tokio::test]
async fn delegation_budget_exhausted_is_governed_rejected() {
    let tools = TimedTools::new(
        &[],
        &[
            ("delegate_to_a", "mock://a"),
            ("delegate_to_b", "mock://b"),
            ("delegate_to_c", "mock://c"),
        ],
    );
    let plan = json!({"nodes": [
        {"id": "a", "type": "a2a", "action": "delegate_to_a", "arguments": {"task": "x"}, "deps": []},
        {"id": "b", "type": "a2a", "action": "delegate_to_b", "arguments": {"task": "x"}, "deps": []},
        {"id": "c", "type": "a2a", "action": "delegate_to_c", "arguments": {"task": "x"}, "deps": []}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let limits = DelegationLimits {
        max_delegations_per_run: 2,
        max_depth: 3,
    };
    let k = kernel_with_limits(oracle, tools.clone(), store, limits);

    let mut events = k.subscribe();
    let run = k.run_task("budget".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    assert!(run
        .error
        .as_deref()
        .unwrap_or("")
        .contains("refused by delegation governance"));

    let mut rejected_reason = None;
    let mut executed_ok = 0u32;
    while let Ok(env) = events.try_recv() {
        match env.event {
            RunEvent::Rejected { reason, .. } => rejected_reason = Some(reason),
            RunEvent::Executed {
                ok: true, action, ..
            } if action.starts_with("delegate_to_") => {
                executed_ok += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        executed_ok, 2,
        "exactly the budgeted number of delegations should have executed — no runaway"
    );
    let reason = rejected_reason.expect("a Rejected event for the over-budget node");
    eprintln!("delegation_budget_exhausted_is_governed_rejected: reason = {reason}");
    assert!(
        reason.contains("delegation budget exhausted (2/2 this run)"),
        "got: {reason}"
    );
}

#[tokio::test]
async fn delegation_depth_limit_enforced_on_inbound_seeded_run() {
    let tools = TimedTools::new(&[], &[("delegate_to_toy", "mock://toy")]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "a2a", "action": "delegate_to_toy", "arguments": {"task": "x"}, "deps": []}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let limits = DelegationLimits {
        max_delegations_per_run: 10,
        max_depth: 3,
    };
    let k = kernel_with_limits(oracle, tools.clone(), store, limits);

    let mut events = k.subscribe();
    // Seed a run that arrived as hop 3 of an inbound A2A chain (matching
    // max_depth) — one more delegation would reach depth 4, over the limit.
    // This exercises the SAME code path an inbound `a2a/message-send`
    // carrying `kelter_a2a_depth: 3` would seed via
    // `Kernel::create_run_with_lineage` (server.rs::message_lineage + the
    // api crate's `a2a_send`).
    let run_id = k
        .create_run_with_lineage(
            "deep chain".into(),
            3,
            vec![
                "mock://hop1".into(),
                "mock://hop2".into(),
                "mock://hop3".into(),
            ],
        )
        .await
        .unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    let mut reason = None;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Rejected { reason: r, .. } = env.event {
            reason = Some(r);
        }
    }
    let reason = reason.expect("a Rejected event for the over-depth node");
    eprintln!("delegation_depth_limit_enforced_on_inbound_seeded_run: reason = {reason}");
    assert!(
        reason.contains("delegation depth limit exceeded (would reach 4, max 3)"),
        "got: {reason}"
    );
}

#[tokio::test]
async fn delegation_cycle_detected_via_ancestry() {
    let tools = TimedTools::new(&[], &[("delegate_to_toy", "mock://toy")]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "a2a", "action": "delegate_to_toy", "arguments": {"task": "x"}, "deps": []}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel_with_limits(oracle, tools.clone(), store, DelegationLimits::default());

    let mut events = k.subscribe();
    // Seed a run whose ancestry already contains the target origin — as if
    // this run arrived via an inbound A2A hop FROM 'mock://toy' itself.
    // Delegating back to it would close a loop.
    let run_id = k
        .create_run_with_lineage("cyclic".into(), 1, vec!["mock://toy".to_string()])
        .await
        .unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    let mut reason = None;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Rejected { reason: r, .. } = env.event {
            reason = Some(r);
        }
    }
    let reason = reason.expect("a Rejected event for the cyclic node");
    eprintln!("delegation_cycle_detected_via_ancestry: reason = {reason}");
    assert!(
        reason.contains("delegation cycle detected: 'mock://toy'"),
        "got: {reason}"
    );
}

#[tokio::test]
async fn event_carries_branch_dimension() {
    let tools = TimedTools::new(&[], &[]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "tool", "action": "step_a", "arguments": {}, "deps": []}
    ]});
    let oracle = FixedPlanOracle::new(plan);
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel_with_limits(oracle, tools.clone(), store, DelegationLimits::default());

    let mut events = k.subscribe();
    let run = k.run_task("branch dimension".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    let mut saw_plan_level_none = false;
    let mut saw_node_branch = false;
    while let Ok(env) = events.try_recv() {
        match env.event {
            RunEvent::Proposed {
                ref action, branch, ..
            } if action == "propose_plan" => {
                assert_eq!(
                    branch, None,
                    "the plan-level proposal is not a node — branch is None"
                );
                saw_plan_level_none = true;
            }
            RunEvent::Executed { branch, .. } => {
                assert_eq!(
                    branch,
                    Some("n1".to_string()),
                    "a plan-node event's branch is the node's own id"
                );
                saw_node_branch = true;
            }
            _ => {}
        }
    }
    assert!(saw_plan_level_none && saw_node_branch);
}

/// A trivial step-mode oracle that finishes on the first call — used only to
/// prove ordinary step-mode events carry `branch: None` (unlike plan-node
/// events above).
struct FinishImmediately;

#[async_trait]
impl PolicyOracle for FinishImmediately {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        Ok(ProposedAction {
            action: kernel_core::FINISH_ACTION.into(),
            arguments: json!({"summary": "done"}),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:finish-immediately".into()
    }
}

#[tokio::test]
async fn step_mode_events_carry_no_branch() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = Kernel::new(
        Arc::new(FinishImmediately),
        TimedTools::new(&[], &[]),
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig::default(), // RunMode::Step (default)
    );
    let mut events = k.subscribe();
    let run = k.run_task("step mode".into()).await.unwrap();
    // Finishing with no action behind it is Incomplete (ADR 0033); this test
    // only cares that the run reached a terminal state through step mode.
    assert_eq!(run.status, RunStatus::Incomplete);

    let mut saw_proposed = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Proposed { branch, .. } = env.event {
            assert_eq!(branch, None, "step-mode events are never branch-tagged");
            saw_proposed = true;
        }
    }
    assert!(saw_proposed);
}
