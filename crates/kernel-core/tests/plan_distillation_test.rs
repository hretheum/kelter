//! Plan distillation — the second self-learning slice (ADR 0016 deferred it;
//! design doc `docs/superpowers/specs/2026-08-03-plan-distillation-design.md`).
//! Proven against mocks — no model, no network. The cruxes:
//!   1. A completed plan-mode run distils to its FINAL journaled generation.
//!      Earlier generations failed and triggered a re-plan (ADR 0030), so
//!      learning from them would be learning failure.
//!   2. One run yields one artifact: plan mode a plan, step mode a fact.
//!   3. A learned plan is offered ONLY while it still validates against the
//!      live action space — staleness is handled by the action space, not by a
//!      policy — and only on the first proposal, never on a re-plan.
//!   4. Adoption is journaled BY VALUE, so forgetting the learned plan
//!      afterwards cannot rewrite what a run already did.
//!   5. An unknown plan id costs exactly one re-ask, never the run and never a
//!      loop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ConfirmationDecision, ExecutionContext, ExecutionError,
    InMemoryLearnedStore, JournalEntry, Kernel, KernelConfig, Latitude, LearnedFact, LearnedPlan,
    LearnedProvenance, LearnedStore, Plan, PlanResilience, PolicyContext, PolicyError,
    PolicyOracle, Proposal, ProposedAction, RunMode, RunStatus, RunStore, ValidatedAction,
    LEARNED_LEVEL,
};
use memory::InMemoryProvider;
use uuid::Uuid;

/// Plays a script of proposals in order (the last entry repeats) while
/// recording, per call, which action names it was offered and what retrieved
/// context rode along.
struct ScriptOracle {
    script: Vec<ProposedAction>,
    cursor: AtomicUsize,
    offered: Mutex<Vec<Vec<String>>>,
    seen_memory: Mutex<Vec<String>>,
}

impl ScriptOracle {
    fn new(script: Vec<ProposedAction>) -> Arc<Self> {
        Arc::new(Self {
            script,
            cursor: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
            seen_memory: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }
    async fn offered_at(&self, call: usize) -> Vec<String> {
        self.offered.lock().await[call].clone()
    }
    async fn memory(&self) -> Vec<String> {
        self.seen_memory.lock().await.clone()
    }
}

#[async_trait]
impl PolicyOracle for ScriptOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.offered
            .lock()
            .await
            .push(schema.actions.iter().map(|a| a.name.clone()).collect());
        self.seen_memory
            .lock()
            .await
            .extend(context.memory.iter().cloned());
        let i = self
            .cursor
            .fetch_add(1, Ordering::SeqCst)
            .min(self.script.len() - 1);
        Ok(self.script[i].clone().into())
    }
    fn describe(&self) -> String {
        "mock:script".into()
    }
}

/// `record` always succeeds; `boom` always fails, which is how a plan is driven
/// to exhaust its node retries and force a re-plan.
struct Tools {
    executed: Mutex<Vec<String>>,
}

impl Tools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            executed: Mutex::new(Vec::new()),
        })
    }
    fn spec(name: &str) -> ActionSpec {
        ActionSpec {
            name: name.into(),
            description: name.into(),
            parameters: json!({
                "type": "object",
                "properties": {"entry": {"type": "string"}},
                "required": ["entry"],
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![Tools::spec("record"), Tools::spec("boom")],
        })
    }
    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        self.executed.lock().await.push(action.action().to_string());
        if action.action() == "boom" {
            return Err(ExecutionError::Tool("boom".into()));
        }
        Ok(json!({"ran": action.action()}))
    }
}

fn plan_kernel(
    oracle: Arc<ScriptOracle>,
    tools: Arc<Tools>,
    store: Arc<dyn RunStore>,
    learned: Arc<dyn LearnedStore>,
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
    .with_learned(learned)
}

fn tool_node(id: &str, action: &str, deps: &[&str]) -> serde_json::Value {
    json!({"id": id, "type": "tool", "action": action,
           "arguments": {"entry": id}, "deps": deps})
}

fn propose_plan(nodes: Vec<serde_json::Value>) -> ProposedAction {
    ProposedAction {
        action: "propose_plan".into(),
        arguments: json!({ "nodes": nodes }),
        rationale: None,
    }
}

fn adopt(plan_id: &str) -> ProposedAction {
    ProposedAction {
        action: "adopt_learned_plan".into(),
        arguments: json!({ "plan_id": plan_id }),
        rationale: None,
    }
}

fn learned_plan(task: &str, nodes: Vec<serde_json::Value>) -> LearnedPlan {
    LearnedPlan {
        id: Uuid::new_v4(),
        task_context: task.into(),
        plan: serde_json::from_value(json!({ "nodes": nodes })).unwrap(),
        provenance: LearnedProvenance {
            source_run_ids: vec![Uuid::new_v4()],
            distilled_at: 0,
            model: "mock".into(),
            level: LEARNED_LEVEL.to_string(),
            human_validated: false,
            batch_id: Uuid::new_v4(),
        },
    }
}

fn node_actions(plan: &Plan) -> Vec<String> {
    plan.nodes
        .iter()
        .map(|n| n.action.clone().unwrap_or_else(|| "review".into()))
        .collect()
}

/// One re-plan: the first plan's node always fails, the revised plan succeeds.
/// Distillation must learn the SECOND plan — the one that actually worked.
#[tokio::test]
async fn distillation_takes_the_final_generation_not_the_first() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
    let oracle = ScriptOracle::new(vec![
        propose_plan(vec![tool_node("bad", "boom", &[])]),
        propose_plan(vec![tool_node("good", "record", &[])]),
    ]);
    let k = plan_kernel(
        oracle,
        Tools::new(),
        store.clone(),
        learned.clone(),
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );

    let run = k.run_task("build the thing".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    // Two generations were journaled — the premise of this test.
    let generations = store
        .journal(run.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e, JournalEntry::PlanProposed { .. }))
        .count();
    assert_eq!(generations, 2);
    // ADR 0037: no contract → unverified → confirmed by a human before it is
    // learnable. Setup only; the assertions below are unchanged.
    k.decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "tester")
        .await
        .unwrap();

    let report = k.distill(10).await.unwrap();
    assert_eq!(report.plans_created, 1);
    assert_eq!(report.facts_created, 0);

    let plans = learned.list_plans().await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(node_actions(&plans[0].plan), vec!["record"]);
    assert_eq!(plans[0].provenance.source_run_ids, vec![run.id]);
    assert!(!plans[0].provenance.human_validated);
    assert_eq!(plans[0].provenance.level, LEARNED_LEVEL);
}

/// A plan-mode run yields a plan, a step-mode run yields a fact, and neither
/// yields both — the two artifact kinds partition the runs, they don't overlap.
#[tokio::test]
async fn plan_mode_yields_a_plan_and_step_mode_a_fact_never_both() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());

    let plan_run = plan_kernel(
        ScriptOracle::new(vec![propose_plan(vec![tool_node("n1", "record", &[])])]),
        Tools::new(),
        store.clone(),
        learned.clone(),
        PlanResilience::default(),
    )
    .run_task("planned task".into())
    .await
    .unwrap();
    assert_eq!(plan_run.status, RunStatus::Completed);

    // Same stores, step mode: one tool call then finish.
    let step_kernel = Kernel::new(
        ScriptOracle::new(vec![
            ProposedAction {
                action: "record".into(),
                arguments: json!({"entry": "x"}),
                rationale: None,
            },
            ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "done"}),
                rationale: None,
            },
        ]),
        Tools::new(),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig::default(),
    )
    .with_learned(learned.clone());
    let step_run = step_kernel.run_task("stepped task".into()).await.unwrap();
    assert_eq!(step_run.status, RunStatus::Completed);
    // ADR 0037: both runs are unverified (no contract); confirm them so they
    // are learnable. Setup only; the assertions below are unchanged.
    for id in [plan_run.id, step_run.id] {
        step_kernel
            .decide_success_confirmation(id, ConfirmationDecision::Confirm, "tester")
            .await
            .unwrap();
    }

    let report = step_kernel.distill(10).await.unwrap();
    assert_eq!(report.plans_created, 1);
    assert_eq!(report.facts_created, 1);
    assert_eq!(report.source_run_ids.len(), 2);

    let plans = learned.list_plans().await.unwrap();
    let facts = learned.list().await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(facts.len(), 1);
    assert_eq!(plans[0].provenance.source_run_ids, vec![plan_run.id]);
    assert_eq!(facts[0].provenance.source_run_ids, vec![step_run.id]);
}

/// A learned plan that still fits the action space is offered as a candidate;
/// one naming an action the registry no longer carries is filtered at retrieval
/// and never reaches the model. This is the structural analogue of forgetting.
#[tokio::test]
async fn only_plans_that_still_validate_are_offered() {
    for (nodes, expect_offered) in [
        (vec![tool_node("n1", "record", &[])], true),
        (vec![tool_node("n1", "long_gone", &[])], false),
    ] {
        let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
        let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
        learned
            .put_plan(learned_plan("ship the report", nodes))
            .await
            .unwrap();

        let oracle = ScriptOracle::new(vec![propose_plan(vec![tool_node("fresh", "record", &[])])]);
        let k = plan_kernel(
            oracle.clone(),
            Tools::new(),
            store,
            learned,
            PlanResilience::default(),
        );
        let run = k.run_task("ship the report".into()).await.unwrap();
        assert_eq!(run.status, RunStatus::Completed);

        let offered = oracle.offered_at(0).await;
        assert_eq!(
            offered.contains(&"adopt_learned_plan".to_string()),
            expect_offered,
            "offered actions were {offered:?}"
        );
        // `propose_plan` is always available — adoption never replaces it.
        assert!(offered.contains(&"propose_plan".to_string()));
    }
}

/// Adoption is a first-proposal choice only. A plan that just failed must not
/// return as a candidate on the re-plan.
#[tokio::test]
async fn adoption_is_not_offered_on_a_replan() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
    learned
        .put_plan(learned_plan(
            "retry me",
            vec![tool_node("n1", "record", &[])],
        ))
        .await
        .unwrap();

    let oracle = ScriptOracle::new(vec![
        propose_plan(vec![tool_node("bad", "boom", &[])]),
        propose_plan(vec![tool_node("good", "record", &[])]),
    ]);
    let k = plan_kernel(
        oracle.clone(),
        Tools::new(),
        store,
        learned,
        PlanResilience {
            max_node_retries: 0,
            max_replans: 1,
        },
    );
    let run = k.run_task("retry me".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    assert!(oracle
        .offered_at(0)
        .await
        .contains(&"adopt_learned_plan".to_string()));
    assert!(!oracle
        .offered_at(1)
        .await
        .contains(&"adopt_learned_plan".to_string()));
}

/// An adopted plan executes, and the journal records it BY VALUE. Forgetting
/// the learned plan afterwards removes future influence without rewriting what
/// the run already did — the journal is history, not residue.
#[tokio::test]
async fn adopted_plan_executes_and_is_journaled_by_value() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
    let candidate = learned_plan(
        "assemble it",
        vec![
            tool_node("a", "record", &[]),
            tool_node("b", "record", &["a"]),
        ],
    );
    let candidate_id = candidate.id;
    let batch_id = candidate.provenance.batch_id;
    learned.put_plan(candidate).await.unwrap();

    let tools = Tools::new();
    let oracle = ScriptOracle::new(vec![adopt(&candidate_id.to_string())]);
    let k = plan_kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        learned.clone(),
        PlanResilience::default(),
    );

    let run = k.run_task("assemble it".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    // The learned plan's nodes actually ran — the model was asked once and
    // never composed a plan of its own.
    assert_eq!(*tools.executed.lock().await, vec!["record", "record"]);
    assert_eq!(oracle.calls(), 1);

    let journaled: Vec<Plan> = store
        .journal(run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e {
            JournalEntry::PlanProposed { plan } => Some(plan),
            _ => None,
        })
        .collect();
    assert_eq!(journaled.len(), 1);
    assert_eq!(node_actions(&journaled[0]), vec!["record", "record"]);

    // Forget the whole batch: the learned layer is empty, the journal is not.
    assert_eq!(learned.forget_batch(batch_id).await.unwrap(), 1);
    assert!(learned.list_plans().await.unwrap().is_empty());
    let after = store
        .journal(run.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e, JournalEntry::PlanProposed { .. }))
        .count();
    assert_eq!(after, 1, "forgetting must not rewrite a completed run");
}

/// A hallucinated plan id costs exactly one re-ask — not the run, and not a
/// loop. The retry is bounded by the schema itself: the second ask does not
/// offer adoption, so the model cannot name a bad id twice.
#[tokio::test]
async fn unknown_plan_id_costs_one_reask_not_the_run() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
    learned
        .put_plan(learned_plan("do it", vec![tool_node("n1", "record", &[])]))
        .await
        .unwrap();

    let oracle = ScriptOracle::new(vec![
        adopt(&Uuid::new_v4().to_string()),
        propose_plan(vec![tool_node("fresh", "record", &[])]),
    ]);
    let tools = Tools::new();
    let k = plan_kernel(
        oracle.clone(),
        tools.clone(),
        store,
        learned,
        PlanResilience::default(),
    );

    let run = k.run_task("do it".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(oracle.calls(), 2, "exactly one re-ask");
    assert!(!oracle
        .offered_at(1)
        .await
        .contains(&"adopt_learned_plan".to_string()));
    assert_eq!(*tools.executed.lock().await, vec!["record"]);
}

/// Plan mode had no retrieved context at all: `execute_plan` branches before
/// step mode's memory block, so plans were composed against an empty
/// `PolicyContext.memory`. Learned facts must now reach the planner, tagged
/// `[learned]` so they stay visibly lower-trust.
#[tokio::test]
async fn plan_mode_receives_learned_facts_as_context() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned: Arc<dyn LearnedStore> = Arc::new(InMemoryLearnedStore::new());
    learned
        .put(LearnedFact {
            id: Uuid::new_v4(),
            fact: "record before finishing".into(),
            task_context: "ship the report".into(),
            provenance: LearnedProvenance {
                source_run_ids: vec![Uuid::new_v4()],
                distilled_at: 0,
                model: "mock".into(),
                level: LEARNED_LEVEL.to_string(),
                human_validated: false,
                batch_id: Uuid::new_v4(),
            },
        })
        .await
        .unwrap();

    let oracle = ScriptOracle::new(vec![propose_plan(vec![tool_node("n1", "record", &[])])]);
    let k = plan_kernel(
        oracle.clone(),
        Tools::new(),
        store,
        learned,
        PlanResilience::default(),
    );
    let run = k.run_task("ship the report".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    let seen = oracle.memory().await;
    assert!(
        seen.iter()
            .any(|c| c == "[learned] record before finishing"),
        "planner saw {seen:?}"
    );
}
