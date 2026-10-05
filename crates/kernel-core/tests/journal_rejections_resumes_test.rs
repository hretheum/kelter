//! ADR 0036: refusals and resumes are journaled — informationally.
//!
//! Every `RunEvent::Rejected` emission site writes a `JournalEntry::Rejected`
//! whose `subject` names what was refused (the refused proposal inline, the
//! step's journaled `Decision`, or a plan node) — never left to `seq + 1`
//! adjacency. A resumed execution writes `JournalEntry::Resumed`. Neither
//! entry may change what resume does: the invariance tests below run the
//! same crashed journal with and without them and compare everything the
//! kernel decides.
//!
//! Fixtures are duplicated per test file by this repo's convention (each
//! `tests/*.rs` is its own binary).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use kernel_core::journal::{
    bounded_rejected_arguments, bounded_rejection_reason, execution_session_id, RejectionSource,
    RejectionSubject, ResumeCause, MAX_REJECTED_ARGUMENTS_BYTES, MAX_REJECTION_REASON_CHARS,
};
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ActivationAuthorization, AgentDelegationDecision,
    CircuitCheck, DelegationDecisionOutcome, DelegationGate, DelegationLimits,
    DelegationRequestMeta, ExecutionContext, ExecutionError, JournalEntry, Kernel, KernelConfig,
    Latitude, PlanResilience, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    ResilienceConfig, RunEvent, RunEventEnvelope, RunMode, RunStatus, RunStore, SkillGate,
    ValidatedAction, IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;

// ---------------------------------------------------------------- fixtures

/// One scripted model turn: a proposal, or a failed model call.
enum Turn {
    Propose(ProposedAction),
    Fail,
}

fn p(action: &str, args: Value) -> Turn {
    Turn::Propose(ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    })
}

/// Pops turns in order and records what the model was shown each call.
struct ScriptOracle {
    script: Mutex<Vec<Turn>>,
    calls: AtomicUsize,
    seen: Mutex<Vec<Value>>,
}

impl ScriptOracle {
    fn new(script: Vec<Turn>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PolicyOracle for ScriptOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Everything that reaches the model, minus nothing: if an
        // informational entry leaked into the context, it would show here.
        self.seen.lock().await.push(json!({
            "task": context.task,
            "steps": context.steps.iter().map(|s| json!({
                "index": s.index, "action": s.action, "arguments": s.arguments,
                "result": s.result, "ok": s.ok,
            })).collect::<Vec<_>>(),
            "memory": context.memory,
            "last_error": context.last_error,
            "guidance": context.guidance,
            "active_skills": context.active_skills,
        }));
        let mut script = self.script.lock().await;
        if script.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        match script.remove(0) {
            Turn::Propose(a) => Ok(a.into()),
            Turn::Fail => Err(PolicyError::Backend("model unreachable".into())),
        }
    }
    fn describe(&self) -> String {
        "mock:script".into()
    }
}

/// A tool `record` (keyed — a mutating action), a tool `fail` that always
/// errs, and `delegate_to_*` agents with configurable origin / failure /
/// open circuit.
struct Tools {
    origins: HashMap<String, String>,
    failing: HashSet<String>,
    open: HashSet<String>,
    executions: Mutex<Vec<(String, Value)>>,
}

impl Tools {
    fn new(failing: &[&str], open: &[&str]) -> Arc<Self> {
        let origins = [
            "delegate_to_primary",
            "delegate_to_open",
            "delegate_to_gated",
            "delegate_to_cyclic",
            "delegate_to_denied",
            "delegate_to_ask",
        ]
        .iter()
        .map(|a| (a.to_string(), format!("https://{a}.example")))
        .collect();
        Arc::new(Self {
            origins,
            failing: failing.iter().map(|s| s.to_string()).collect(),
            open: open.iter().map(|s| s.to_string()).collect(),
            executions: Mutex::new(Vec::new()),
        })
    }
    async fn ran(&self) -> Vec<(String, Value)> {
        self.executions.lock().await.clone()
    }
}

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let mut actions = vec![
            ActionSpec {
                name: "record".into(),
                description: "Record an entry (side effect)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "entry": {"type": "string"},
                        IDEMPOTENCY_KEY_ARG: {"type": "string"}
                    },
                    "required": ["entry"],
                    "additionalProperties": false
                }),
            },
            ActionSpec {
                name: "fail".into(),
                description: "Always fails".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"why": {"type": "string"}},
                    "additionalProperties": false
                }),
            },
        ];
        let mut agents: Vec<&String> = self.origins.keys().collect();
        agents.sort();
        for a in agents {
            actions.push(ActionSpec {
                name: a.clone(),
                description: "a remote agent".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"task": {"type": "string"}},
                    "required": ["task"],
                    "additionalProperties": false
                }),
            });
        }
        Ok(ActionSchema { actions })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<Value, ExecutionError> {
        self.executions
            .lock()
            .await
            .push((action.action().to_string(), action.arguments().clone()));
        if action.action() == "fail" || self.failing.contains(action.action()) {
            return Err(ExecutionError::Tool(format!("{} is down", action.action())));
        }
        Ok(json!({"ran": action.action()}))
    }

    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.origins.get(action).cloned()
    }

    fn circuit_state(&self, action: &str) -> CircuitCheck {
        if self.open.contains(action) {
            CircuitCheck::Open {
                retry_after: std::time::Duration::from_secs(30),
            }
        } else {
            CircuitCheck::Closed
        }
    }
}

/// Denies `deny`, asks for `ask`, and denies `deny_after_first` from its
/// second consultation on (a retry refused by trust after the first dial
/// was allowed).
struct Gate {
    deny: HashSet<String>,
    ask: HashSet<String>,
    deny_after_first: HashSet<String>,
    seen: std::sync::Mutex<HashMap<String, usize>>,
}

impl Gate {
    fn new(deny: &[&str], ask: &[&str], deny_after_first: &[&str]) -> Arc<Self> {
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Arc::new(Self {
            deny: set(deny),
            ask: set(ask),
            deny_after_first: set(deny_after_first),
            seen: std::sync::Mutex::new(HashMap::new()),
        })
    }
}

impl DelegationGate for Gate {
    fn authorize_delegation_to(&self, action: &str, _origin: &str) -> DelegationDecisionOutcome {
        let n = {
            let mut seen = self.seen.lock().unwrap();
            let n = seen.entry(action.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        if self.deny.contains(action) || (self.deny_after_first.contains(action) && n > 1) {
            DelegationDecisionOutcome::Deny(format!("agent '{action}' is not trusted"))
        } else if self.ask.contains(action) {
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
    fn elevate_trust(&self, _action: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Every activation is denied.
struct DenySkills;

impl SkillGate for DenySkills {
    fn any_activatable(&self) -> bool {
        true
    }
    fn authorize_activation(&self, name: &str, _run_active: &[String]) -> ActivationAuthorization {
        ActivationAuthorization::Deny(format!("skill '{name}' may not be activated"))
    }
    fn elevate_trust(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
}

fn kernel(
    oracle: Arc<ScriptOracle>,
    tools: Arc<Tools>,
    store: Arc<dyn RunStore>,
    config: KernelConfig,
) -> Kernel {
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        config,
    )
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<RunEventEnvelope>) -> Vec<RunEventEnvelope> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

/// A journaled rejection, flattened for assertions.
#[derive(Debug, Clone, PartialEq)]
struct Rej {
    step: u32,
    attempt: u32,
    source: RejectionSource,
    subject: RejectionSubject,
    action: Option<String>,
    arguments: Option<Value>,
    branch: Option<String>,
    reason: String,
}

fn rejections(journal: &[JournalEntry]) -> Vec<Rej> {
    journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Rejected {
                step,
                attempt,
                source,
                subject,
                reason,
                action,
                arguments,
                branch,
                at_ms,
            } => {
                assert!(*at_ms > 0, "a rejection carries its wall-clock time");
                Some(Rej {
                    step: *step,
                    attempt: *attempt,
                    source: *source,
                    subject: *subject,
                    action: action.clone(),
                    arguments: arguments.clone(),
                    branch: branch.clone(),
                    reason: reason.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// (step, attempt, proposal_seq, reason) of every live rejection, plus the
/// map seq → (step, action) of every live proposal.
#[allow(clippy::type_complexity)]
fn live(
    events: &[RunEventEnvelope],
) -> (
    Vec<(u32, u32, Option<u64>, String)>,
    HashMap<u64, (u32, String)>,
) {
    let mut rejected = Vec::new();
    let mut proposed = HashMap::new();
    for e in events {
        match &e.event {
            RunEvent::Rejected {
                step,
                attempt,
                proposal_seq,
                reason,
                ..
            } => rejected.push((*step, *attempt, *proposal_seq, reason.clone())),
            RunEvent::Proposed { step, action, .. } => {
                proposed.insert(e.seq, (*step, action.clone()));
            }
            _ => {}
        }
    }
    (rejected, proposed)
}

fn position(journal: &[JournalEntry], pred: impl Fn(&JournalEntry) -> bool) -> usize {
    journal
        .iter()
        .position(pred)
        .expect("expected journal entry is present")
}

// ------------------------------------------------- step mode: proposals

/// All six step-mode proposal paths in one run: policy error, schema,
/// duplicate, trust-gate deny, skill-gate deny, delegation limit. Each
/// journals the refused proposal inline and precedes its step's Decision;
/// each live event points at the very `Proposed` it refuses.
#[tokio::test]
async fn every_step_mode_refusal_journals_the_refused_proposal() {
    let oracle = ScriptOracle::new(vec![
        // step 0
        Turn::Fail,
        p("record", json!({"nope": 1})),
        p("record", json!({"entry": "x"})),
        // step 1
        p("record", json!({"entry": "x"})),
        p("delegate_to_denied", json!({"task": "t"})),
        p("record", json!({"entry": "y"})),
        // step 2
        p("activate_skill", json!({"name": "s"})),
        p("delegate_to_cyclic", json!({"task": "t"})),
        p("finish", json!({"summary": "done"})),
    ]);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        KernelConfig {
            latitude: Latitude::Standard,
            max_steps: 6,
            ..KernelConfig::default()
        },
    )
    .with_delegation_gate(Gate::new(&["delegate_to_denied"], &[], &[]))
    .with_skills(Arc::new(DenySkills));
    // The cyclic agent's origin is already in this run's A2A ancestry.
    let run_id = k
        .create_run_with_lineage(
            "record things".into(),
            0,
            vec!["https://delegate_to_cyclic.example".into()],
        )
        .await
        .unwrap();
    let mut rx = k.subscribe();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);

    let journal = store.journal(run_id).await.unwrap();
    let got = rejections(&journal);
    let expect = [
        (
            0,
            0,
            RejectionSource::PolicyError,
            RejectionSubject::None,
            None,
            None,
        ),
        (
            0,
            1,
            RejectionSource::Schema,
            RejectionSubject::Proposal,
            Some("record"),
            Some(json!({"nope": 1})),
        ),
        (
            1,
            0,
            RejectionSource::Duplicate,
            RejectionSubject::Proposal,
            Some("record"),
            Some(json!({"entry": "x"})),
        ),
        (
            1,
            1,
            RejectionSource::DelegationGate,
            RejectionSubject::Proposal,
            Some("delegate_to_denied"),
            Some(json!({"task": "t"})),
        ),
        (
            2,
            0,
            RejectionSource::SkillGate,
            RejectionSubject::Proposal,
            Some("activate_skill"),
            Some(json!({"name": "s"})),
        ),
        (
            2,
            1,
            RejectionSource::DelegationLimit,
            RejectionSubject::Proposal,
            Some("delegate_to_cyclic"),
            Some(json!({"task": "t"})),
        ),
    ];
    assert_eq!(got.len(), expect.len(), "{got:#?}");
    for (r, (step, attempt, source, subject, action, args)) in got.iter().zip(expect) {
        assert_eq!(
            (r.step, r.attempt, r.source, r.subject),
            (step, attempt, source, subject),
            "{r:?}"
        );
        assert_eq!(r.action.as_deref(), action, "{r:?}");
        assert_eq!(r.arguments, args, "{r:?}");
        assert_eq!(r.branch, None);
        assert!(!r.reason.is_empty());
    }

    // Each step's rejections precede that step's Decision in the journal.
    for step in 0..3u32 {
        let decision = position(
            &journal,
            |e| matches!(e, JournalEntry::Decision { step: s, .. } if *s == step),
        );
        for (i, e) in journal.iter().enumerate() {
            if let JournalEntry::Rejected { step: s, .. } = e {
                if *s == step {
                    assert!(i < decision, "rejection after its step's decision");
                }
            }
        }
    }

    // Live: each rejection names the Proposed it refuses (same step, same
    // action) — or none, for the failed model call.
    let (live_rej, proposed) = live(&drain(&mut rx));
    assert_eq!(live_rej.len(), got.len());
    for ((step, attempt, proposal_seq, reason), r) in live_rej.iter().zip(&got) {
        assert_eq!((*step, *attempt), (r.step, r.attempt));
        match r.subject {
            RejectionSubject::None => assert_eq!(*proposal_seq, None),
            _ => {
                let seq = proposal_seq.expect("a refused proposal names its Proposed seq");
                let (pstep, paction) = &proposed[&seq];
                assert_eq!(*pstep, r.step);
                assert_eq!(Some(paction.as_str()), r.action.as_deref());
            }
        }
        // The live reason is unchanged by journaling.
        assert_eq!(reason, &r.reason);
    }
}

// --------------------------------------- step mode: delegation resilience

struct ResilienceCase {
    budget: u32,
    retries: u32,
    fallback: Vec<&'static str>,
    gate_deny_after_first: Vec<&'static str>,
    gate_deny: Vec<&'static str>,
    expect: Vec<(u32, RejectionSource, &'static str)>,
}

async fn run_resilience(case: ResilienceCase) {
    let oracle = ScriptOracle::new(vec![
        p("delegate_to_primary", json!({"task": "work"})),
        p("finish", json!({"summary": "gave up"})),
    ]);
    let tools = Tools::new(&["delegate_to_primary"], &["delegate_to_open"]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle,
        tools,
        store.clone(),
        KernelConfig {
            latitude: Latitude::Standard,
            max_steps: 4,
            delegation: DelegationLimits {
                max_delegations_per_run: case.budget,
                max_depth: 3,
            },
            resilience: ResilienceConfig {
                max_retries_per_delegation: case.retries,
                fallback: HashMap::from([(
                    "delegate_to_primary".to_string(),
                    case.fallback.iter().map(|s| s.to_string()).collect(),
                )]),
            },
            ..KernelConfig::default()
        },
    )
    .with_delegation_gate(Gate::new(&case.gate_deny, &[], &case.gate_deny_after_first));
    let run_id = k.create_run("delegate".into()).await.unwrap();
    let mut rx = k.subscribe();
    k.execute_run(run_id).await.unwrap();

    let journal = store.journal(run_id).await.unwrap();
    let got = rejections(&journal);
    assert_eq!(got.len(), case.expect.len(), "{got:#?}");
    for (r, (attempt, source, agent)) in got.iter().zip(&case.expect) {
        // Refers to the ACCEPTED decision of step 0 — not to a proposal.
        assert_eq!(r.step, 0);
        assert_eq!(r.subject, RejectionSubject::Decision, "{r:?}");
        assert_eq!((r.attempt, r.source), (*attempt, *source), "{r:?}");
        assert_eq!(r.action.as_deref(), Some(*agent), "{r:?}");
        assert_eq!(
            r.arguments, None,
            "the decision's arguments live in the Decision"
        );
    }

    // The decision they refer to is journaled — and accepted: Decision and
    // Intent precede every one of them, its Receipt follows.
    let decision = position(
        &journal,
        |e| matches!(e, JournalEntry::Decision { step: 0, action, .. } if action == "delegate_to_primary"),
    );
    let intent = position(&journal, |e| {
        matches!(e, JournalEntry::Intent { step: 0, .. })
    });
    let receipt = position(&journal, |e| {
        matches!(e, JournalEntry::Receipt { step: 0, .. })
    });
    for (i, e) in journal.iter().enumerate() {
        if matches!(e, JournalEntry::Rejected { .. }) {
            assert!(
                decision < i && intent < i && i < receipt,
                "order: {journal:#?}"
            );
        }
    }

    // Live: every refusal points at the ACCEPTED proposal of step 0, however
    // many events lie between them — the pairing `seq + 1` got wrong.
    let events = drain(&mut rx);
    let (live_rej, proposed) = live(&events);
    let accepted_seq = proposed
        .iter()
        .find(|(_, (step, action))| *step == 0 && action == "delegate_to_primary")
        .map(|(seq, _)| *seq)
        .unwrap();
    assert_eq!(live_rej.len(), got.len());
    for (_, _, proposal_seq, _) in &live_rej {
        assert_eq!(*proposal_seq, Some(accepted_seq));
    }
    // Why adjacency was the wrong rule: the first refusal sits at exactly
    // `accepted_seq + 1`, yet the proposal there was ACCEPTED (its Decision
    // is journaled and it was dialed). `seq + 1` pairing would have marked an
    // accepted proposal as rejected.
    let first_rejected_seq = events
        .iter()
        .find(|e| matches!(e.event, RunEvent::Rejected { .. }))
        .map(|e| e.seq)
        .unwrap();
    assert_eq!(first_rejected_seq, accepted_seq + 1);
}

#[tokio::test]
async fn retry_failures_and_fallback_refusals_refer_to_the_accepted_decision() {
    // Two failed retries, an open-circuit fallback skipped, then a fallback
    // the trust gate refuses.
    run_resilience(ResilienceCase {
        budget: 10,
        retries: 2,
        fallback: vec!["delegate_to_open", "delegate_to_gated"],
        gate_deny_after_first: vec![],
        gate_deny: vec!["delegate_to_gated"],
        expect: vec![
            (1, RejectionSource::DelegationRetry, "delegate_to_primary"),
            (2, RejectionSource::DelegationRetry, "delegate_to_primary"),
            (1, RejectionSource::CircuitOpen, "delegate_to_open"),
            (2, RejectionSource::DelegationGate, "delegate_to_gated"),
        ],
    })
    .await;
}

#[tokio::test]
async fn fallback_refused_by_budget_refers_to_the_accepted_decision() {
    // Budget 3: first dial + two retries spend it; the fallback is refused.
    run_resilience(ResilienceCase {
        budget: 3,
        retries: 2,
        fallback: vec!["delegate_to_open", "delegate_to_gated"],
        gate_deny_after_first: vec![],
        gate_deny: vec![],
        expect: vec![
            (1, RejectionSource::DelegationRetry, "delegate_to_primary"),
            (2, RejectionSource::DelegationRetry, "delegate_to_primary"),
            (1, RejectionSource::CircuitOpen, "delegate_to_open"),
            (2, RejectionSource::DelegationLimit, "delegate_to_gated"),
        ],
    })
    .await;
}

#[tokio::test]
async fn retry_refused_by_budget_refers_to_the_accepted_decision() {
    run_resilience(ResilienceCase {
        budget: 1,
        retries: 2,
        fallback: vec![],
        gate_deny_after_first: vec![],
        gate_deny: vec![],
        expect: vec![(1, RejectionSource::DelegationLimit, "delegate_to_primary")],
    })
    .await;
}

#[tokio::test]
async fn retry_refused_by_trust_refers_to_the_accepted_decision() {
    // The first (model-proposed) dial was allowed; the gate refuses the retry.
    run_resilience(ResilienceCase {
        budget: 10,
        retries: 2,
        fallback: vec![],
        gate_deny_after_first: vec!["delegate_to_primary"],
        gate_deny: vec![],
        expect: vec![(1, RejectionSource::DelegationGate, "delegate_to_primary")],
    })
    .await;
}

// ------------------------------------------------------------ plan mode

fn plan(nodes: Value) -> Turn {
    p("propose_plan", json!({ "nodes": nodes }))
}

fn plan_config(max_node_retries: u32, max_replans: u32) -> KernelConfig {
    KernelConfig {
        mode: RunMode::Plan,
        latitude: Latitude::Standard,
        plan_resilience: PlanResilience {
            max_node_retries,
            max_replans,
        },
        ..KernelConfig::default()
    }
}

/// A failing node is retried (node_retry), the plan fails and a revised
/// plan is requested; the first revision is invalid (plan_validation, the
/// refused plan carried inline), the second completes.
#[tokio::test]
async fn plan_node_retry_and_rejected_replan_are_journaled() {
    let invalid = json!([
        {"id": "n1", "type": "tool", "action": "record",
         "arguments": {"entry": "x"}, "deps": ["ghost"]}
    ]);
    let oracle = ScriptOracle::new(vec![
        plan(json!([
            {"id": "n1", "type": "tool", "action": "fail", "arguments": {}, "deps": []}
        ])),
        plan(invalid.clone()),
        plan(json!([
            {"id": "n1", "type": "tool", "action": "record",
             "arguments": {"entry": "x"}, "deps": []}
        ])),
    ]);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(oracle, tools, store.clone(), plan_config(1, 2));
    let run_id = k.create_run("plan it".into()).await.unwrap();
    let mut rx = k.subscribe();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);

    let journal = store.journal(run_id).await.unwrap();
    let got = rejections(&journal);
    assert_eq!(got.len(), 2, "{got:#?}");

    let retry = &got[0];
    assert_eq!(
        (retry.step, retry.attempt, retry.source, retry.subject),
        (0, 1, RejectionSource::NodeRetry, RejectionSubject::PlanNode)
    );
    assert_eq!(retry.branch.as_deref(), Some("n1"));
    assert_eq!(retry.action.as_deref(), Some("fail"));
    // The node it refers to is in the generation-0 plan.
    let first_plan = position(&journal, |e| matches!(e, JournalEntry::PlanProposed { .. }));
    assert!(first_plan < position(&journal, |e| matches!(e, JournalEntry::Rejected { .. })));

    let replan = &got[1];
    assert_eq!(
        (replan.step, replan.source, replan.subject),
        (
            kernel_core::engine::PLAN_GENERATION_STRIDE,
            RejectionSource::PlanValidation,
            RejectionSubject::Proposal
        )
    );
    assert_eq!(replan.action.as_deref(), Some("propose_plan"));
    let carried = replan
        .arguments
        .clone()
        .expect("the refused plan is carried");
    assert_eq!(carried["nodes"][0]["deps"], json!(["ghost"]));
    // The refused revision never became a PlanProposed.
    let plans = journal
        .iter()
        .filter(|e| matches!(e, JournalEntry::PlanProposed { .. }))
        .count();
    assert_eq!(plans, 2, "generation 0 and the accepted revision only");

    let (live_rej, proposed) = live(&drain(&mut rx));
    assert_eq!(live_rej.len(), 2);
    let plan0_seq = proposed
        .iter()
        .find(|(_, (step, a))| *step == 0 && a == "propose_plan")
        .map(|(s, _)| *s)
        .unwrap();
    assert_eq!(
        live_rej[0].2,
        Some(plan0_seq),
        "node refusal → its plan's Proposed"
    );
    assert_eq!(
        live_rej[1].2, None,
        "a rejected revision was never announced"
    );
}

/// Plan-node governance refusals, one single-node plan each (fail-fast
/// admits nothing after the first refusal).
#[tokio::test]
async fn plan_node_governance_refusals_refer_to_the_node() {
    let cases = [
        ("delegate_to_cyclic", RejectionSource::DelegationLimit),
        ("delegate_to_open", RejectionSource::CircuitOpen),
        ("delegate_to_denied", RejectionSource::DelegationGate),
    ];
    for (agent, source) in cases {
        let oracle = ScriptOracle::new(vec![plan(json!([
            {"id": "a", "type": "a2a", "action": agent, "arguments": {"task": "t"}, "deps": []}
        ]))]);
        let tools = Tools::new(&[], &["delegate_to_open"]);
        let store = Arc::new(kernel_core::InMemoryRunStore::new());
        let k = kernel(oracle, tools.clone(), store.clone(), plan_config(0, 0))
            .with_delegation_gate(Gate::new(&["delegate_to_denied"], &[], &[]));
        let run_id = k
            .create_run_with_lineage(
                "plan it".into(),
                0,
                vec!["https://delegate_to_cyclic.example".into()],
            )
            .await
            .unwrap();
        let mut rx = k.subscribe();
        let run = k.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::Failed, "{agent}");
        assert!(tools.ran().await.is_empty(), "{agent}: nothing dialed");

        let got = rejections(&store.journal(run_id).await.unwrap());
        assert_eq!(got.len(), 1, "{agent}: {got:#?}");
        let r = &got[0];
        assert_eq!(
            (r.step, r.attempt, r.source, r.subject),
            (0, 0, source, RejectionSubject::PlanNode),
            "{agent}"
        );
        assert_eq!(r.branch.as_deref(), Some("a"));
        assert_eq!(r.action.as_deref(), Some(agent));
        let (live_rej, proposed) = live(&drain(&mut rx));
        let seq = live_rej[0].2.expect("names the plan's Proposed");
        assert_eq!(proposed[&seq], (0, "propose_plan".to_string()));
    }
}

/// An operator Deny recorded for a plan-mode ask is consumed on resume and
/// journaled (operator_denied); that resume is a `pause_resolved` one.
#[tokio::test]
async fn plan_operator_deny_on_resume_is_journaled_with_a_pause_resolved_resume() {
    let oracle = ScriptOracle::new(vec![plan(json!([
        {"id": "a", "type": "a2a", "action": "delegate_to_ask",
         "arguments": {"task": "t"}, "deps": []}
    ]))]);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        plan_config(0, 0),
    )
    .with_delegation_gate(Gate::new(&[], &["delegate_to_ask"], &[]));
    let run_id = k.create_run("plan it".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::AwaitingDelegationDecision);
    k.decide_delegation(
        run_id,
        "delegate_to_ask",
        AgentDelegationDecision::Deny,
        "eryk",
    )
    .await
    .unwrap();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(oracle.calls.load(Ordering::SeqCst), 1, "plan replayed");

    let journal = store.journal(run_id).await.unwrap();
    let got = rejections(&journal);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(
        (got[0].source, got[0].subject, got[0].branch.as_deref()),
        (
            RejectionSource::OperatorDenied,
            RejectionSubject::PlanNode,
            Some("a")
        )
    );
    let resumed: Vec<_> = journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Resumed {
                at_step,
                cause,
                replayed_steps,
                ..
            } => Some((*at_step, *cause, replayed_steps.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(resumed, vec![(0, ResumeCause::PauseResolved, vec![0])]);
}

// --------------------------------------------------------------- resume

fn resumed_entries(journal: &[JournalEntry]) -> Vec<(u32, String, ResumeCause, Vec<u32>, u64)> {
    journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Resumed {
                at_step,
                session,
                cause,
                replayed_steps,
                at_ms,
            } => Some((
                *at_step,
                session.clone(),
                *cause,
                replayed_steps.clone(),
                *at_ms,
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_first_execution_writes_no_resume_even_after_a_caller_note() {
    let oracle = ScriptOracle::new(vec![
        p("record", json!({"entry": "x"})),
        p("finish", json!({"summary": "done"})),
    ]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle,
        Tools::new(&[], &[]),
        store.clone(),
        KernelConfig::default(),
    );
    let run_id = k.create_run("record".into()).await.unwrap();
    // What the API does before spawning the first execution.
    store
        .append_journal(
            run_id,
            JournalEntry::Note {
                step: 0,
                key: "principal".into(),
                value: json!({"principal": "default", "via": "api/runs"}),
            },
        )
        .await
        .unwrap();
    k.execute_run(run_id).await.unwrap();
    assert!(resumed_entries(&store.journal(run_id).await.unwrap()).is_empty());
}

/// Killed after the side effect (Decision + Intent + Receipt journaled, run
/// record never updated): the resume replays the decision, does NOT re-run
/// the tool, and says so in the journal.
#[tokio::test]
async fn a_resume_after_the_side_effect_is_journaled_with_its_replayed_step() {
    let oracle = ScriptOracle::new(vec![p("finish", json!({"summary": "done"}))]);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        KernelConfig::default(),
    );
    let run_id = k.create_run("record".into()).await.unwrap();
    for e in crashed_after_effect(run_id) {
        store.append_journal(run_id, e).await.unwrap();
    }
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(
        tools.ran().await.is_empty(),
        "exactly-once: no re-execution"
    );
    assert_eq!(
        oracle.calls.load(Ordering::SeqCst),
        1,
        "only `finish` asked"
    );

    let journal = store.journal(run_id).await.unwrap();
    let r = resumed_entries(&journal);
    assert_eq!(r.len(), 1);
    let (at_step, session, cause, replayed, at_ms) = &r[0];
    assert_eq!(*at_step, 0);
    assert_eq!(*session, execution_session_id().to_string());
    assert_eq!(*cause, ResumeCause::Interrupted);
    assert_eq!(*replayed, vec![0]);
    assert!(*at_ms >= before);
    // Written before anything this execution decides.
    let resumed_at = position(&journal, |e| matches!(e, JournalEntry::Resumed { .. }));
    let terminal = position(&journal, |e| matches!(e, JournalEntry::Terminal { .. }));
    assert!(resumed_at < terminal);
}

#[tokio::test]
async fn a_plan_mode_resume_marks_the_replayed_plan_generation() {
    let oracle = ScriptOracle::new(vec![]);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        plan_config(0, 0),
    );
    let run_id = k.create_run("plan it".into()).await.unwrap();
    let plan: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        {"id": "n1", "type": "tool", "action": "record", "arguments": {"entry": "x"}, "deps": []}
    ]}))
    .unwrap();
    store
        .append_journal(run_id, JournalEntry::PlanProposed { plan })
        .await
        .unwrap();
    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(oracle.calls.load(Ordering::SeqCst), 0, "plan replayed");
    let r = resumed_entries(&store.journal(run_id).await.unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0].0, r[0].2, r[0].3.clone()),
        (0, ResumeCause::Interrupted, vec![0])
    );
}

/// A run that crashed BEFORE its decision was journaled (only a refusal
/// survived) resumes with nothing replayed: the model is asked again.
#[tokio::test]
async fn a_resume_with_no_journaled_decision_replays_nothing() {
    let oracle = ScriptOracle::new(vec![
        p("record", json!({"entry": "x"})),
        p("finish", json!({"summary": "done"})),
    ]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(
        oracle.clone(),
        Tools::new(&[], &[]),
        store.clone(),
        KernelConfig::default(),
    );
    let run_id = k.create_run("record".into()).await.unwrap();
    store
        .append_journal(run_id, rejected_entry(0, 0))
        .await
        .unwrap();
    k.execute_run(run_id).await.unwrap();
    let r = resumed_entries(&store.journal(run_id).await.unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].0, r[0].3.clone()), (0, Vec::<u32>::new()));
    assert_eq!(oracle.calls.load(Ordering::SeqCst), 2);
}

// ------------------------------------------- informational invariance

fn crashed_after_effect(run_id: Uuid) -> Vec<JournalEntry> {
    vec![
        JournalEntry::Decision {
            step: 0,
            action: "record".into(),
            arguments: json!({"entry": "x"}),
            usage: None,
        },
        JournalEntry::Intent {
            step: 0,
            action: "record".into(),
            arguments: json!({"entry": "x"}),
            idempotency_key: format!("{run_id}:0"),
        },
        JournalEntry::Receipt {
            step: 0,
            ok: true,
            result: json!({"ran": "record"}),
        },
    ]
}

fn rejected_entry(step: u32, attempt: u32) -> JournalEntry {
    JournalEntry::Rejected {
        step,
        attempt,
        source: RejectionSource::Schema,
        subject: RejectionSubject::Proposal,
        reason: "bogus".into(),
        action: Some("record".into()),
        arguments: Some(json!({"entry": "x"})),
        branch: None,
        at_ms: 1,
    }
}

fn resumed_entry(at_step: u32) -> JournalEntry {
    JournalEntry::Resumed {
        at_step,
        session: "00000000-0000-0000-0000-000000000000".into(),
        cause: ResumeCause::PauseResolved,
        replayed_steps: vec![at_step, 7],
        at_ms: 1,
    }
}

/// Insert informational entries at every position of `base` — including
/// ones that name the same step, and a stale `Resumed` claiming a replay.
fn with_informational(base: Vec<JournalEntry>) -> Vec<JournalEntry> {
    let mut out = vec![resumed_entry(0), rejected_entry(0, 0)];
    for e in base {
        out.push(e);
        out.push(rejected_entry(0, 1));
        out.push(rejected_entry(1, 0));
        out.push(resumed_entry(1));
    }
    out
}

/// Everything the kernel decides in one resume, for comparison.
#[derive(Debug, PartialEq)]
struct Outcome {
    status: RunStatus,
    output: Option<Value>,
    error: Option<String>,
    steps: Vec<(u32, String, Value, Value, bool)>,
    model_calls: usize,
    model_saw: Vec<Value>,
    executions: Vec<(String, Value)>,
    /// The journal with informational entries removed, serialized.
    journal: Vec<Value>,
}

async fn resume_once(seed: Vec<JournalEntry>, script: Vec<Turn>, config: KernelConfig) -> Outcome {
    let oracle = ScriptOracle::new(script);
    let tools = Tools::new(&[], &[]);
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = kernel(oracle.clone(), tools.clone(), store.clone(), config);
    let run_id = k.create_run("record things".into()).await.unwrap();
    // Seeds use a fixed placeholder id in idempotency keys; rewrite to this run.
    for e in seed {
        let e = match e {
            JournalEntry::Intent {
                step,
                action,
                arguments,
                ..
            } => JournalEntry::Intent {
                step,
                action,
                arguments,
                idempotency_key: format!("{run_id}:{step}"),
            },
            other => other,
        };
        store.append_journal(run_id, e).await.unwrap();
    }
    let run = k.execute_run(run_id).await.unwrap();
    let journal = store
        .journal(run_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_informational())
        .map(|e| {
            let mut v = serde_json::to_value(e).unwrap();
            // Idempotency keys embed the (random) run id.
            if let Some(k) = v.get_mut("idempotency_key") {
                *k = json!("<key>");
            }
            v
        })
        .collect();
    let model_saw = oracle.seen.lock().await.clone();
    Outcome {
        status: run.status,
        output: run.output,
        error: run.error,
        steps: run
            .steps
            .into_iter()
            .map(|s| (s.index, s.action, s.arguments, s.result, s.ok))
            .collect(),
        model_calls: oracle.calls.load(Ordering::SeqCst),
        model_saw,
        executions: tools
            .ran()
            .await
            .into_iter()
            .map(|(a, mut args)| {
                if let Some(k) = args.get_mut(IDEMPOTENCY_KEY_ARG) {
                    *k = json!("<key>");
                }
                (a, args)
            })
            .collect(),
        journal,
    }
}

fn step_script() -> Vec<Turn> {
    vec![
        p("record", json!({"entry": "y"})),
        p("finish", json!({"summary": "done"})),
    ]
}

/// The same crashed journals, resumed with and without informational
/// entries interleaved: identical decisions, executions, model context,
/// run record and (non-informational) journal.
#[tokio::test]
async fn informational_entries_never_change_a_step_mode_resume() {
    let placeholder = Uuid::nil();
    let crash_points: Vec<Vec<JournalEntry>> = vec![
        // after the decision, before the side effect
        crashed_after_effect(placeholder)[..1].to_vec(),
        // in the intent → receipt window (keyed tool: dedupes)
        crashed_after_effect(placeholder)[..2].to_vec(),
        // after the side effect
        crashed_after_effect(placeholder),
    ];
    for base in crash_points {
        let plain = resume_once(base.clone(), step_script(), KernelConfig::default()).await;
        let noisy = resume_once(
            with_informational(base.clone()),
            step_script(),
            KernelConfig::default(),
        )
        .await;
        assert_eq!(plain, noisy, "crash point with {} entries", base.len());
    }
}

#[tokio::test]
async fn informational_entries_never_change_a_plan_mode_resume() {
    let plan: kernel_core::Plan = serde_json::from_value(json!({"nodes": [
        {"id": "n1", "type": "tool", "action": "record", "arguments": {"entry": "x"}, "deps": []},
        {"id": "n2", "type": "tool", "action": "record", "arguments": {"entry": "y"}, "deps": ["n1"]}
    ]}))
    .unwrap();
    let base = vec![
        JournalEntry::PlanProposed { plan },
        JournalEntry::Intent {
            step: 0,
            action: "record".into(),
            arguments: json!({"entry": "x"}),
            idempotency_key: "<rewritten>".into(),
        },
        JournalEntry::Receipt {
            step: 0,
            ok: true,
            result: json!({"ran": "record"}),
        },
    ];
    let plain = resume_once(base.clone(), vec![], plan_config(1, 1)).await;
    let noisy = resume_once(with_informational(base), vec![], plan_config(1, 1)).await;
    assert_eq!(plain.status, RunStatus::Completed, "{:?}", plain.error);
    assert_eq!(plain.executions.len(), 1, "n1 not re-run, n2 run once");
    assert_eq!(plain, noisy);
}

// ------------------------------------------------- compatibility, shape

/// A pre-ADR-0036 journal, byte-for-byte as an older kernel wrote it, still
/// parses — the new variants are purely additive.
#[test]
fn a_pre_0036_journal_still_parses() {
    let rows = [
        r#"{"kind":"trust_posture","entries":[["ext","wasm-sandboxed"]]}"#,
        r#"{"kind":"note","step":0,"key":"principal","value":{"principal":"default","via":"api/runs"}}"#,
        r#"{"kind":"decision","step":0,"action":"record","arguments":{"entry":"x"},"usage":{"prompt_tokens":3,"completion_tokens":2,"calls":1}}"#,
        r#"{"kind":"intent","step":0,"action":"record","arguments":{"entry":"x"},"idempotency_key":"k"}"#,
        r#"{"kind":"receipt","step":0,"ok":true,"result":{"ran":"record"}}"#,
        r#"{"kind":"terminal","status":"completed","output":{"summary":"done"}}"#,
    ];
    for row in rows {
        let e: JournalEntry = serde_json::from_str(row).unwrap();
        assert!(!e.is_informational());
        // Unchanged shape on the way back out.
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::from_str::<Value>(row).unwrap()
        );
    }
}

/// The exact wire shape a reader (Glassbox) parses — pinned.
#[test]
fn new_entries_have_the_documented_shape() {
    let rejected = JournalEntry::Rejected {
        step: 2,
        attempt: 1,
        source: RejectionSource::DelegationRetry,
        subject: RejectionSubject::Decision,
        reason: "delegation attempt failed — retrying (1/2)".into(),
        action: Some("delegate_to_primary".into()),
        arguments: None,
        branch: None,
        at_ms: 1_759_651_200_000,
    };
    assert_eq!(
        serde_json::to_value(&rejected).unwrap(),
        json!({
            "kind": "rejected",
            "step": 2,
            "attempt": 1,
            "source": "delegation_retry",
            "subject": "decision",
            "reason": "delegation attempt failed — retrying (1/2)",
            "action": "delegate_to_primary",
            "at_ms": 1_759_651_200_000u64,
        })
    );
    let resumed = JournalEntry::Resumed {
        at_step: 3,
        session: "6f1c2b9e-0000-4000-8000-000000000000".into(),
        cause: ResumeCause::Interrupted,
        replayed_steps: vec![3],
        at_ms: 1_759_651_200_000,
    };
    assert_eq!(
        serde_json::to_value(&resumed).unwrap(),
        json!({
            "kind": "resumed",
            "at_step": 3,
            "session": "6f1c2b9e-0000-4000-8000-000000000000",
            "cause": "interrupted",
            "replayed_steps": [3],
            "at_ms": 1_759_651_200_000u64,
        })
    );
    // Round-trip.
    for e in [rejected, resumed] {
        let v = serde_json::to_value(&e).unwrap();
        let back: JournalEntry = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), v);
    }
}

#[test]
fn refused_arguments_and_reasons_are_bounded_and_scrubbed() {
    let huge = json!({"blob": "x".repeat(MAX_REJECTED_ARGUMENTS_BYTES * 2)});
    let bounded = bounded_rejected_arguments(&huge);
    assert_eq!(bounded["truncated"], json!(true));
    assert!(bounded["bytes"].as_u64().unwrap() > MAX_REJECTED_ARGUMENTS_BYTES as u64);
    assert!(bounded.to_string().len() < 2 * 1024);

    let small = json!({"entry": "x"});
    assert_eq!(bounded_rejected_arguments(&small), small);

    let long = "r".repeat(MAX_REJECTION_REASON_CHARS * 3);
    assert_eq!(
        bounded_rejection_reason(&long).chars().count(),
        MAX_REJECTION_REASON_CHARS + 1 // the ellipsis
    );

    let secret = "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef";
    assert!(!bounded_rejection_reason(&format!("bad key {secret}")).contains(secret));
    assert!(!bounded_rejected_arguments(&json!({ "k": secret }))
        .to_string()
        .contains(secret));
}
