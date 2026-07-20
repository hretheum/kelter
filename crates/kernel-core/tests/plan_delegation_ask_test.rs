//! Plan-mode delegation `AskHuman` pause (ADR 0025 — closes the ADR 0023
//! plan-mode gap). The concurrent admission loop (ADR 0022) now DRAINS
//! already-in-flight siblings, then suspends the run as
//! `AwaitingDelegationDecision` instead of degrading the ask to a deny — with
//! the per-run delegation budget staying single-writer-atomic AND exact
//! across the pause + a restart (the resume seed is re-derived from the
//! journal). Real `Kernel` in plan mode, a scripted `DelegationGate`, and a
//! `TimedTools` that records every execution — no model, no network.
//!
//! Fixtures are duplicated per test file by this repo's convention (each
//! `tests/*.rs` is its own binary).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, AgentDelegationDecision, DelegationDecisionOutcome,
    DelegationGate, DelegationLimits, DelegationRequestMeta, ExecutionContext, ExecutionError,
    Kernel, KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal,
    ProposedAction, RunMode, RunStatus, RunStore,
};
use memory::InMemoryProvider;

struct FixedPlanOracle {
    plan: Value,
}
impl FixedPlanOracle {
    fn new(plan: Value) -> Arc<Self> {
        Arc::new(Self { plan })
    }
}
#[async_trait]
impl PolicyOracle for FixedPlanOracle {
    async fn propose(
        &self,
        _c: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        Ok(ProposedAction {
            action: schema.actions[0].name.clone(),
            arguments: self.plan.clone(),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:fixed-plan".into()
    }
}

struct TimedTools {
    delays: HashMap<String, Duration>,
    origins: HashMap<String, String>,
    executions: Mutex<Vec<String>>,
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
    async fn ran(&self) -> Vec<String> {
        self.executions.lock().await.clone()
    }
}
#[async_trait]
impl ActionSource for TimedTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
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
                delegate("delegate_to_ask"),
                delegate("delegate_to_ok"),
                delegate("delegate_to_ok2"),
                delegate("delegate_to_ok3"),
            ],
        })
    }
    async fn execute(
        &self,
        action: &ValidatedActionAlias,
        _ctx: &ExecutionContext,
    ) -> Result<Value, ExecutionError> {
        self.executions
            .lock()
            .await
            .push(action.action().to_string());
        if let Some(d) = self.delays.get(action.action()) {
            tokio::time::sleep(*d).await;
        }
        Ok(json!({"ran": action.action()}))
    }
    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.origins.get(action).cloned()
    }
}
use kernel_core::ValidatedAction as ValidatedActionAlias;

/// A scripted gate: named actions ask a human; others are allowed. Records
/// every `elevate_trust` so `allow_always` can be verified.
struct ScriptedGate {
    ask_for: HashSet<String>,
    elevated: std::sync::Mutex<Vec<String>>,
}
impl ScriptedGate {
    fn new(ask_for: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            ask_for: ask_for.iter().map(|s| s.to_string()).collect(),
            elevated: std::sync::Mutex::new(Vec::new()),
        })
    }
}
impl DelegationGate for ScriptedGate {
    fn authorize_delegation_to(&self, action: &str, _origin: &str) -> DelegationDecisionOutcome {
        if self.ask_for.contains(action) {
            DelegationDecisionOutcome::AskHuman(DelegationRequestMeta {
                trust: "untrusted".into(),
                description: format!("{action} wants to run"),
                provenance_source_kind: "config".into(),
                provenance_source: "test".into(),
            })
        } else {
            DelegationDecisionOutcome::Allow("trusted".into())
        }
    }
    fn elevate_trust(&self, action: &str) -> Result<(), String> {
        self.elevated.lock().unwrap().push(action.to_string());
        Ok(())
    }
}

fn plan_kernel(
    tools: Arc<dyn ActionSource>,
    store: Arc<dyn RunStore>,
    gate: Arc<dyn DelegationGate>,
    plan: Value,
) -> Kernel {
    Kernel::new(
        FixedPlanOracle::new(plan),
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            latitude: Latitude::Standard,
            max_steps: 12,
            delegation: DelegationLimits {
                max_delegations_per_run: 10,
                max_depth: 3,
            },
            ..KernelConfig::default()
        },
    )
    .with_delegation_gate(gate)
}

#[tokio::test]
async fn plan_mode_askhuman_pauses_instead_of_denying() {
    // The ADR 0023 gap closed: an `AskHuman` on a plan-mode a2a node PAUSES
    // the run (durable), it does NOT degrade to a deny/failure.
    let tools = TimedTools::new(&[], &[("delegate_to_ask", "mock://ask")]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "a2a", "action": "delegate_to_ask", "arguments": {"task": "x"}, "deps": []}
    ]});
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = plan_kernel(
        tools.clone(),
        store,
        ScriptedGate::new(&["delegate_to_ask"]),
        plan,
    );

    let run = k.run_task("pause not deny".into()).await.unwrap();

    assert_eq!(
        run.status,
        RunStatus::AwaitingDelegationDecision,
        "an AskHuman node must suspend the run, never fail it"
    );
    let pending = run.pending_delegation.expect("a pending delegation");
    assert_eq!(pending.agent, "delegate_to_ask");
    assert!(
        tools.ran().await.is_empty(),
        "nothing may be dialed while awaiting authorization"
    );
}

#[tokio::test]
async fn plan_mode_allow_once_resumes_and_dispatches() {
    let tools = TimedTools::new(&[], &[("delegate_to_ask", "mock://ask")]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "a2a", "action": "delegate_to_ask", "arguments": {"task": "x"}, "deps": []}
    ]});
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = plan_kernel(
        tools.clone(),
        store,
        ScriptedGate::new(&["delegate_to_ask"]),
        plan,
    );

    let paused = k.run_task("allow once".into()).await.unwrap();
    assert_eq!(paused.status, RunStatus::AwaitingDelegationDecision);

    k.decide_delegation(
        paused.id,
        "delegate_to_ask",
        AgentDelegationDecision::AllowOnce,
        "operator",
    )
    .await
    .unwrap();
    let run = k.execute_run(paused.id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        tools.ran().await,
        vec!["delegate_to_ask".to_string()],
        "after allow_once the delegation dials exactly once"
    );
}

#[tokio::test]
async fn plan_mode_deny_fails_the_node_and_plan() {
    let tools = TimedTools::new(&[], &[("delegate_to_ask", "mock://ask")]);
    let plan = json!({"nodes": [
        {"id": "n1", "type": "a2a", "action": "delegate_to_ask", "arguments": {"task": "x"}, "deps": []}
    ]});
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = plan_kernel(
        tools.clone(),
        store,
        ScriptedGate::new(&["delegate_to_ask"]),
        plan,
    );

    let paused = k.run_task("deny".into()).await.unwrap();
    k.decide_delegation(
        paused.id,
        "delegate_to_ask",
        AgentDelegationDecision::Deny,
        "operator",
    )
    .await
    .unwrap();
    let run = k.execute_run(paused.id).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        tools.ran().await.is_empty(),
        "a denied delegation never dials"
    );
    assert!(run
        .error
        .as_deref()
        .unwrap_or("")
        .contains("denied by the operator"));
}

#[tokio::test]
async fn plan_mode_drains_inflight_siblings_before_pausing() {
    // A delayed Allow sibling (plan order FIRST) is spawned in the same pass
    // the AskHuman node (SECOND) is reached — the loop must DRAIN it to a
    // receipt before suspending, so the resume budget seed is exact and no
    // in-flight work is orphaned.
    let tools = TimedTools::new(
        &[("delegate_to_ok", Duration::from_millis(150))],
        &[
            ("delegate_to_ok", "mock://ok"),
            ("delegate_to_ask", "mock://ask"),
        ],
    );
    let plan = json!({"nodes": [
        {"id": "ok",  "type": "a2a", "action": "delegate_to_ok",  "arguments": {"task": "x"}, "deps": []},
        {"id": "ask", "type": "a2a", "action": "delegate_to_ask", "arguments": {"task": "x"}, "deps": []}
    ]});
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = plan_kernel(
        tools.clone(),
        store,
        ScriptedGate::new(&["delegate_to_ask"]),
        plan,
    );

    let paused = k.run_task("drain".into()).await.unwrap();

    assert_eq!(paused.status, RunStatus::AwaitingDelegationDecision);
    assert_eq!(
        tools.ran().await,
        vec!["delegate_to_ok".to_string()],
        "the in-flight Allow sibling must drain to completion before the run pauses"
    );
    // The drained sibling is a recorded step; the paused ask node is not.
    assert!(paused.steps.iter().any(|s| s.action == "delegate_to_ok"));
    assert!(!paused.steps.iter().any(|s| s.action == "delegate_to_ask"));
}

#[tokio::test]
async fn ask_pause_survives_a_restart_and_budget_stays_exact() {
    // Durable pause: a SECOND Kernel over the SAME store (a daemon restart)
    // re-surfaces the pending delegation from the journal, then resolves +
    // resumes — and the per-run budget, re-seeded from the journal, is not
    // double-counted across the pause.
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let plan = json!({"nodes": [
        {"id": "ok",  "type": "a2a", "action": "delegate_to_ok",  "arguments": {"task": "x"}, "deps": []},
        {"id": "ask", "type": "a2a", "action": "delegate_to_ask", "arguments": {"task": "x"}, "deps": []}
    ]});
    let origins = &[
        ("delegate_to_ok", "mock://ok"),
        ("delegate_to_ask", "mock://ask"),
    ];

    // ---- session 1: run to the pause, then drop the kernel ----
    let tools1 = TimedTools::new(&[], origins);
    let paused = {
        let k1 = plan_kernel(
            tools1.clone(),
            store.clone(),
            ScriptedGate::new(&["delegate_to_ask"]),
            plan.clone(),
        );
        k1.run_task("restart".into()).await.unwrap()
    };
    assert_eq!(paused.status, RunStatus::AwaitingDelegationDecision);
    assert_eq!(tools1.ran().await, vec!["delegate_to_ok".to_string()]);

    // ---- session 2: fresh kernel + fresh tools over the same store ----
    let tools2 = TimedTools::new(&[], origins);
    let k2 = plan_kernel(
        tools2.clone(),
        store.clone(),
        ScriptedGate::new(&["delegate_to_ask"]),
        plan.clone(),
    );

    // Restart re-surfaces the pending delegation without advancing.
    let resurfaced = k2.execute_run(paused.id).await.unwrap();
    assert_eq!(resurfaced.status, RunStatus::AwaitingDelegationDecision);
    assert!(tools2.ran().await.is_empty(), "re-surfacing dials nothing");

    // Resolve + resume on the new kernel.
    k2.decide_delegation(
        paused.id,
        "delegate_to_ask",
        AgentDelegationDecision::AllowOnce,
        "operator",
    )
    .await
    .unwrap();
    let run = k2.execute_run(paused.id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    // Session 2 only ever dials the ask node (the ok node's receipt from
    // session 1 is replayed, NOT re-dialed) — exactly one delegation here.
    assert_eq!(
        tools2.ran().await,
        vec!["delegate_to_ask".to_string()],
        "resume replays the drained sibling's receipt and dials only the authorized node"
    );
    // Both a2a nodes are recorded exactly once — budget consumed twice total
    // (ok in session 1, ask in session 2), never double-counted.
    let a2a_steps = run
        .steps
        .iter()
        .filter(|s| s.action.starts_with("delegate_to_"))
        .count();
    assert_eq!(
        a2a_steps, 2,
        "each delegation recorded exactly once across the restart"
    );
}

#[tokio::test]
async fn budget_is_atomic_across_concurrent_branches_with_a_pause() {
    // The correctness core: budget = 2 with THREE allowed concurrent a2a
    // branches plus one ask branch. The single-writer admission counter must
    // admit EXACTLY 2 dials total — the ask pause must not create a window
    // that lets a third slip through, and resume must not re-admit past 2.
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let plan = json!({"nodes": [
        {"id": "a",   "type": "a2a", "action": "delegate_to_ok",  "arguments": {"task": "x"}, "deps": []},
        {"id": "b",   "type": "a2a", "action": "delegate_to_ok2", "arguments": {"task": "x"}, "deps": []},
        {"id": "c",   "type": "a2a", "action": "delegate_to_ok3", "arguments": {"task": "x"}, "deps": []}
    ]});
    let tools = TimedTools::new(
        &[],
        &[
            ("delegate_to_ok", "mock://a"),
            ("delegate_to_ok2", "mock://b"),
            ("delegate_to_ok3", "mock://c"),
        ],
    );
    let k = Kernel::new(
        FixedPlanOracle::new(plan),
        tools.clone(),
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            mode: RunMode::Plan,
            latitude: Latitude::Standard,
            max_steps: 12,
            delegation: DelegationLimits {
                max_delegations_per_run: 2,
                max_depth: 3,
            },
            ..KernelConfig::default()
        },
    )
    .with_delegation_gate(ScriptedGate::new(&[]));

    let run = k.run_task("atomic budget".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let dialed = tools.ran().await.len();
    assert_eq!(
        dialed, 2,
        "exactly the budgeted 2 delegations may dial under concurrency — never 3"
    );
}
