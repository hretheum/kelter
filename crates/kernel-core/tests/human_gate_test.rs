//! The human gate on mutating actions (ADR 0038): `allow` / `ask` / `deny`
//! in step and plan mode, the pause, the decision with its principal, and
//! exactly-once after the decision. The restart and timeout-sweeper legs run
//! against the real SQLite store in `crates/daemon/tests/human_gate_restart_test.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::journal::RejectionSource;
use kernel_core::{
    ActionApprovalDecision, ActionGateConfig, ActionPolicy, ActionSchema, ActionSource, ActionSpec,
    ExecutionContext, ExecutionError, InMemoryRunStore, JournalEntry, Kernel, KernelConfig,
    KernelError, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunEvent, RunMode, RunStatus, RunStore, ValidatedAction, IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;

/// Scripted oracle; records the `last_error` it was shown on every call.
struct Script {
    script: Mutex<Vec<ProposedAction>>,
    calls: AtomicUsize,
    seen_errors: Mutex<Vec<Option<String>>>,
}

impl Script {
    fn new(script: Vec<ProposedAction>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            seen_errors: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PolicyOracle for Script {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_errors
            .lock()
            .await
            .push(context.last_error.clone());
        let mut s = self.script.lock().await;
        if s.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(s.remove(0).into())
    }
    fn describe(&self) -> String {
        "mock:gate".into()
    }
}

/// `write` is mutating (declares `idempotency_key`) and dedupes by key, like
/// a real keyed tool; `read` is read-only.
#[derive(Default)]
struct Tools {
    effects: Mutex<Vec<serde_json::Value>>,
    seen_keys: Mutex<Vec<String>>,
    reads: AtomicUsize,
}

impl Tools {
    async fn effects(&self) -> usize {
        self.effects.lock().await.len()
    }
}

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "write".into(),
                    description: "write an entry (side effect)".into(),
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
                    name: "read".into(),
                    description: "read (no side effect)".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ],
        })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        if action.action() == "read" {
            self.reads.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"read": true}));
        }
        let key = action.arguments()[IDEMPOTENCY_KEY_ARG]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut keys = self.seen_keys.lock().await;
        if !keys.contains(&key) {
            keys.push(key);
            self.effects
                .lock()
                .await
                .push(action.arguments()["entry"].clone());
        }
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

fn gate(entries: &[(&str, ActionPolicy)]) -> ActionGateConfig {
    ActionGateConfig {
        default: ActionPolicy::Allow,
        actions: entries
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<HashMap<_, _>>(),
        ask_timeout_secs: 300,
    }
}

struct H {
    kernel: Kernel,
    store: Arc<InMemoryRunStore>,
    oracle: Arc<Script>,
    tools: Arc<Tools>,
}

fn harness(script: Vec<ProposedAction>, mode: RunMode, g: ActionGateConfig) -> H {
    let store = Arc::new(InMemoryRunStore::new());
    let oracle = Script::new(script);
    let tools = Arc::new(Tools::default());
    let kernel = Kernel::new(
        oracle.clone(),
        tools.clone(),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig {
            max_steps: 6,
            latitude: Latitude::Standard,
            reflection_enabled: false,
            mode,
            ..KernelConfig::default()
        },
    )
    .with_action_gate(g);
    H {
        kernel,
        store,
        oracle,
        tools,
    }
}

fn rejections(journal: &[JournalEntry]) -> Vec<(RejectionSource, Option<String>)> {
    journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Rejected { source, action, .. } => Some((*source, action.clone())),
            _ => None,
        })
        .collect()
}

fn decisions_at(journal: &[JournalEntry], at: u32) -> usize {
    journal
        .iter()
        .filter(|e| matches!(e, JournalEntry::Decision { step, .. } if *step == at))
        .count()
}

// ---------------------------------------------------------------- step mode

#[tokio::test]
async fn allow_policy_and_default_gate_execute_without_pausing() {
    for g in [
        ActionGateConfig::default(),
        gate(&[("write", ActionPolicy::Allow)]),
    ] {
        let h = harness(
            vec![
                p("write", json!({"entry": "a"})),
                p("finish", json!({"summary": "done"})),
            ],
            RunMode::Step,
            g,
        );
        let run = h.kernel.run_task("t".into()).await.unwrap();
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(h.tools.effects().await, 1);
        assert!(run.pending_action_approval.is_none());
    }
}

#[tokio::test]
async fn deny_policy_refuses_journals_and_tells_the_model() {
    let h = harness(
        vec![
            p("write", json!({"entry": "a"})),
            p("read", json!({})),
            p("finish", json!({"summary": "done"})),
        ],
        RunMode::Step,
        gate(&[("write", ActionPolicy::Deny)]),
    );
    let mut rx = h.kernel.subscribe();
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(h.tools.effects().await, 0, "a denied action never executes");
    let journal = h.store.journal(run.id).await.unwrap();
    assert_eq!(
        rejections(&journal),
        vec![(RejectionSource::ActionPolicy, Some("write".into()))]
    );
    // The model's next call carries the refusal as feedback.
    let seen = h.oracle.seen_errors.lock().await.clone();
    assert!(
        seen[1]
            .as_deref()
            .is_some_and(|e| e.contains("action policy denies 'write'")),
        "{seen:?}"
    );
    // And the live stream carried the refusal.
    let mut saw_rejected = false;
    while let Ok(env) = rx.try_recv() {
        if let RunEvent::Rejected { reason, .. } = env.event {
            saw_rejected |= reason.contains("action policy");
        }
    }
    assert!(saw_rejected);
}

#[tokio::test]
async fn read_only_actions_are_never_gated() {
    let h = harness(
        vec![p("read", json!({})), p("finish", json!({"summary": "ok"}))],
        RunMode::Step,
        ActionGateConfig {
            default: ActionPolicy::Deny,
            actions: HashMap::from([("read".to_string(), ActionPolicy::Deny)]),
            ask_timeout_secs: 300,
        },
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(h.tools.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ask_pauses_then_allow_executes_exactly_once_without_requerying_the_model() {
    let h = harness(
        vec![
            p("write", json!({"entry": "approved"})),
            p("finish", json!({"summary": "done"})),
        ],
        RunMode::Step,
        gate(&[("write", ActionPolicy::Ask)]),
    );
    let mut rx = h.kernel.subscribe();
    let id = h.kernel.create_run("t".into()).await.unwrap();
    let paused = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(paused.status, RunStatus::AwaitingActionApproval);
    let pending = paused.pending_action_approval.clone().unwrap();
    assert_eq!((pending.step, pending.action.as_str()), (0, "write"));
    assert_eq!(pending.arguments["entry"], "approved");
    assert_eq!(pending.expires_at, Some(pending.requested_at + 300));
    assert_eq!(
        h.tools.effects().await,
        0,
        "nothing executes before a decision"
    );
    let journal = h.store.journal(id).await.unwrap();
    assert_eq!(decisions_at(&journal, 0), 0, "no Decision before approval");
    assert!(journal.iter().any(
        |e| matches!(e, JournalEntry::ActionApprovalRequested { action, .. } if action == "write")
    ));
    let mut requested_event = false;
    while let Ok(env) = rx.try_recv() {
        requested_event |= matches!(env.event, RunEvent::ActionApprovalRequested { .. });
    }
    assert!(requested_event);

    // Executing again before a decision is idempotent: no model call, no effect.
    let again = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(again.status, RunStatus::AwaitingActionApproval);
    assert_eq!(h.oracle.calls(), 1);

    // A stale decision for another action is refused.
    let wrong = h
        .kernel
        .decide_action_approval(id, "read", ActionApprovalDecision::Allow, "alice")
        .await;
    assert!(matches!(wrong, Err(KernelError::InvalidState(_))));

    let decided = h
        .kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Allow, "alice")
        .await
        .unwrap();
    assert_eq!(decided.status, RunStatus::Running);
    assert!(decided.pending_action_approval.is_none());

    let done = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(done.status, RunStatus::Completed, "{:?}", done.error);
    assert_eq!(h.tools.effects().await, 1);
    assert_eq!(
        h.oracle.calls(),
        2,
        "write once + finish once — never re-asked"
    );
    assert_eq!(done.steps[0].action, "write");

    let journal = h.store.journal(id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::ActionApprovalDecided { principal, decision: ActionApprovalDecision::Allow, .. }
            if principal == "alice"
    )));
    assert_eq!(decisions_at(&journal, 0), 1);
    // The resume after the decision says why it resumed.
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::Resumed { cause: kernel_core::journal::ResumeCause::PauseResolved, replayed_steps, .. }
            if replayed_steps == &vec![0]
    )));

    // A finished run never executes again.
    h.kernel.execute_run(id).await.unwrap();
    assert_eq!(h.tools.effects().await, 1);

    // Deciding a run that is not paused is refused.
    let late = h
        .kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Allow, "alice")
        .await;
    assert!(matches!(late, Err(KernelError::InvalidState(_))));
}

#[tokio::test]
async fn ask_then_deny_tells_the_model_and_never_executes() {
    let h = harness(
        vec![
            p("write", json!({"entry": "x"})),
            p("read", json!({})),
            p("finish", json!({"summary": "done"})),
        ],
        RunMode::Step,
        gate(&[("write", ActionPolicy::Ask)]),
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    h.kernel.execute_run(id).await.unwrap();
    h.kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Deny, "system:timeout")
        .await
        .unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(h.tools.effects().await, 0);
    let seen = h.oracle.seen_errors.lock().await.clone();
    assert!(
        seen[1]
            .as_deref()
            .is_some_and(|e| e.contains("declined 'write'")),
        "{seen:?}"
    );
    let journal = h.store.journal(id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::ActionApprovalDecided { principal, decision: ActionApprovalDecision::Deny, .. }
            if principal == "system:timeout"
    )));
}

#[tokio::test]
async fn crash_after_the_allowed_decision_is_journaled_still_executes_once() {
    let h = harness(
        vec![
            p("write", json!({"entry": "once"})),
            p("finish", json!({"summary": "done"})),
        ],
        RunMode::Step,
        gate(&[("write", ActionPolicy::Ask)]),
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    h.kernel.execute_run(id).await.unwrap();
    h.kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Allow, "alice")
        .await
        .unwrap();
    // A crashed execution that got as far as journaling the Decision AND the
    // Intent (the tool may or may not have run): the resume must take the
    // dangling-intent path with the journaled key, not journal a second
    // Decision and not dispatch a fresh key.
    let key = format!("{id}:0");
    h.store
        .append_journal(
            id,
            JournalEntry::Decision {
                step: 0,
                action: "write".into(),
                arguments: json!({"entry": "once"}),
                usage: None,
            },
        )
        .await
        .unwrap();
    h.store
        .append_journal(
            id,
            JournalEntry::Intent {
                step: 0,
                action: "write".into(),
                arguments: json!({"entry": "once"}),
                idempotency_key: key.clone(),
            },
        )
        .await
        .unwrap();
    // The crashed attempt's effect landed (tool saw the key) …
    h.tools.seen_keys.lock().await.push(key.clone());
    h.tools.effects.lock().await.push(json!("once"));

    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    // … so the re-dispatch deduped: still exactly one effect.
    assert_eq!(h.tools.effects().await, 1);
    assert_eq!(h.tools.seen_keys.lock().await.clone(), vec![key]);
    let journal = h.store.journal(id).await.unwrap();
    assert_eq!(
        decisions_at(&journal, 0),
        1,
        "the Allow Decision is journaled once"
    );
}

// ---------------------------------------------------------------- plan mode

fn plan(nodes: serde_json::Value) -> ProposedAction {
    p("propose_plan", json!({ "nodes": nodes }))
}

#[tokio::test]
async fn plan_ask_drains_siblings_pauses_and_allow_executes_once() {
    let h = harness(
        vec![plan(json!([
            {"id": "r", "type": "tool", "action": "read", "arguments": {}},
            {"id": "w", "type": "tool", "action": "write", "arguments": {"entry": "p"}},
        ]))],
        RunMode::Plan,
        gate(&[("write", ActionPolicy::Ask)]),
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    let paused = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(paused.status, RunStatus::AwaitingActionApproval);
    let pending = paused.pending_action_approval.unwrap();
    assert_eq!(pending.branch.as_deref(), Some("w"));
    assert_eq!(pending.action, "write");
    assert_eq!(
        h.tools.reads.load(Ordering::SeqCst),
        1,
        "the sibling drained"
    );
    assert_eq!(h.tools.effects().await, 0);

    h.kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Allow, "bob")
        .await
        .unwrap();
    let done = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(done.status, RunStatus::Completed, "{:?}", done.error);
    assert_eq!(h.tools.effects().await, 1);
    assert_eq!(
        h.tools.reads.load(Ordering::SeqCst),
        1,
        "the drained sibling is not re-run"
    );
    assert_eq!(
        h.oracle.calls(),
        1,
        "the plan is replayed, never re-proposed"
    );
}

#[tokio::test]
async fn plan_deny_policy_and_human_deny_fail_the_node() {
    // Policy deny: refused at admission, journaled, plan fails.
    let h = harness(
        vec![plan(json!([
            {"id": "w", "type": "tool", "action": "write", "arguments": {"entry": "p"}},
        ]))],
        RunMode::Plan,
        gate(&[("write", ActionPolicy::Deny)]),
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.tools.effects().await, 0);
    let journal = h.store.journal(run.id).await.unwrap();
    assert_eq!(
        rejections(&journal),
        vec![(RejectionSource::ActionPolicy, Some("write".into()))]
    );

    // Human deny after an ask: refused on resume as operator_denied.
    let h = harness(
        vec![plan(json!([
            {"id": "w", "type": "tool", "action": "write", "arguments": {"entry": "p"}},
        ]))],
        RunMode::Plan,
        gate(&[("write", ActionPolicy::Ask)]),
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    h.kernel.execute_run(id).await.unwrap();
    h.kernel
        .decide_action_approval(id, "write", ActionApprovalDecision::Deny, "bob")
        .await
        .unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.tools.effects().await, 0);
    let journal = h.store.journal(id).await.unwrap();
    assert_eq!(
        rejections(&journal),
        vec![(RejectionSource::OperatorDenied, Some("write".into()))]
    );
}

#[tokio::test]
async fn plan_review_node_can_require_a_human_verdict() {
    for (decision, status) in [
        (ActionApprovalDecision::Allow, RunStatus::Completed),
        (ActionApprovalDecision::Deny, RunStatus::Failed),
    ] {
        let h = harness(
            vec![plan(json!([
                {"id": "r", "type": "tool", "action": "read", "arguments": {}},
                {"id": "ok", "type": "review", "deps": ["r"]},
            ]))],
            RunMode::Plan,
            gate(&[("review_verdict", ActionPolicy::Ask)]),
        );
        let id = h.kernel.create_run("t".into()).await.unwrap();
        let paused = h.kernel.execute_run(id).await.unwrap();
        assert_eq!(paused.status, RunStatus::AwaitingActionApproval);
        let pending = paused.pending_action_approval.unwrap();
        assert_eq!(pending.action, "review_verdict");
        assert_eq!(pending.branch.as_deref(), Some("ok"));
        assert_eq!(pending.arguments["upstream"][0]["read"], true);

        h.kernel
            .decide_action_approval(id, "review_verdict", decision, "carol")
            .await
            .unwrap();
        let run = h.kernel.execute_run(id).await.unwrap();
        assert_eq!(run.status, status, "{:?}", run.error);
        assert_eq!(
            h.oracle.calls(),
            1,
            "the human verdict replaces the model's"
        );
        let review = run.steps.iter().find(|s| s.action == "review").unwrap();
        assert_eq!(review.result["reviewer"], "carol");
        assert_eq!(review.ok, decision == ActionApprovalDecision::Allow);
    }
}
