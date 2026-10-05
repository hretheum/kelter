//! ADR 0037 — the success contract and the `verification` basis.
//!
//! The deployment fact behind it: "send an e-mail" with no mail tool was
//! "done" by writing a file — an effective action, so ADR 0033's floor let it
//! through as `completed`. With a contract naming `send_email` that is now
//! `incomplete`, with the failing condition in the reason; without one it is
//! `completed` but `verification: none`, and learning ignores it until a
//! human confirms it. These tests drive the real kernel with a scripted
//! oracle, in step and plan mode.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ConfirmationDecision, ExecutionContext, ExecutionError,
    InMemoryLearnedStore, JournalEntry, Kernel, KernelConfig, KernelError, Latitude, LearnedStore,
    PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, Run, RunEvent, RunMode,
    RunStatus, RunStore, SuccessContract, ValidatedAction, Verification,
};
use memory::{Episode, EpisodeOutcome, InMemoryProvider, MemoryProvider, Provenance};

/// `write_file` and `send_email` both succeed; `send_email` returns a status
/// the contract can check.
struct Tools;

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        let p = json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        });
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "write_file".into(),
                    description: "Write a file".into(),
                    parameters: p.clone(),
                },
                ActionSpec {
                    name: "send_email".into(),
                    description: "Send an e-mail".into(),
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
            "write_file" => Ok(json!({"path": "mail.txt", "bytes": 12})),
            "send_email" => Ok(json!({"status": "queued", "id": "m-1"})),
            other => Err(ExecutionError::Tool(format!("no such tool {other}"))),
        }
    }
}

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
            "propose_plan" => return Ok(propose("propose_plan", self.plan.clone()).into()),
            "review_verdict" => {
                return Ok(
                    propose("review_verdict", json!({"approve": true, "reason": "ok"})).into(),
                )
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

fn write_then_finish() -> Vec<ProposedAction> {
    vec![
        propose("write_file", json!({"text": "Dear Bob, ..."})),
        propose("finish", json!({"summary": "e-mail sent"})),
    ]
}

fn send_then_finish() -> Vec<ProposedAction> {
    vec![
        propose("send_email", json!({"text": "Dear Bob, ..."})),
        propose("finish", json!({"summary": "e-mail sent"})),
    ]
}

fn contract(v: serde_json::Value) -> SuccessContract {
    serde_json::from_value(v).unwrap()
}

fn needs_send_email() -> SuccessContract {
    contract(json!({"require": [{"kind": "action_succeeded", "action": "send_email"}]}))
}

struct Harness {
    kernel: Kernel,
    store: Arc<dyn RunStore>,
    learned: Arc<InMemoryLearnedStore>,
    memory: Arc<InMemoryProvider>,
}

fn harness(oracle: Arc<Scripted>, mode: RunMode) -> Harness {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let learned = Arc::new(InMemoryLearnedStore::new());
    let memory = Arc::new(InMemoryProvider::new());
    let kernel = Kernel::new(
        oracle,
        Arc::new(Tools),
        memory.clone(),
        store.clone(),
        KernelConfig {
            mode,
            latitude: Latitude::Standard,
            ..KernelConfig::default()
        },
    )
    .with_learned(learned.clone());
    Harness {
        kernel,
        store,
        learned,
        memory,
    }
}

async fn run_with(h: &Harness, task: &str, c: Option<SuccessContract>) -> Run {
    let id = h
        .kernel
        .create_run_with_contract(task.into(), None, c)
        .await
        .unwrap();
    h.kernel.execute_run(id).await.unwrap()
}

async fn terminal_entry(store: &Arc<dyn RunStore>, id: uuid::Uuid) -> JournalEntry {
    store
        .journal(id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| matches!(e, JournalEntry::Terminal { .. }))
        .expect("terminal entry")
}

// ---- contract evaluation: step mode --------------------------------------

#[tokio::test]
async fn step_mode_writing_a_file_does_not_satisfy_a_send_email_contract() {
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let mut events = h.kernel.subscribe();
    let run = run_with(&h, "send an e-mail to Bob", Some(needs_send_email())).await;

    // The write_file step was effective — ADR 0033 alone would complete it.
    assert!(run.steps.iter().any(|s| s.action == "write_file" && s.ok));
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(run.verification, Some(Verification::Contract));
    let reason = run.error.clone().unwrap();
    assert!(reason.contains("success contract not met"), "{reason}");
    assert!(reason.contains("`send_email`"), "{reason}");
    assert!(reason.contains("never executed"), "{reason}");
    // The model's claim is kept, unverified.
    assert_eq!(run.output.as_ref().unwrap()["summary"], "e-mail sent");
    let report = run.contract_report.clone().unwrap();
    assert!(!report.satisfied && !report.conditions[0].met);

    // The verdict is durable: the terminal journal entry carries it.
    match terminal_entry(&h.store, run.id).await {
        JournalEntry::Terminal {
            status,
            verification,
            contract_report,
            ..
        } => {
            assert_eq!(status, RunStatus::Incomplete);
            assert_eq!(verification, Some(Verification::Contract));
            assert_eq!(contract_report, run.contract_report);
        }
        _ => unreachable!(),
    }

    // And the live event names it, never `completed`.
    let mut saw = false;
    while let Ok(env) = events.try_recv() {
        match env.event {
            RunEvent::Completed { .. } => panic!("an unmet contract must not emit Completed"),
            RunEvent::Incomplete {
                verification,
                contract_report,
                reason,
                ..
            } => {
                assert_eq!(verification, Verification::Contract);
                assert!(!contract_report.unwrap().satisfied);
                assert!(reason.contains("send_email"));
                saw = true;
            }
            _ => {}
        }
    }
    assert!(saw, "an Incomplete event must be emitted");
}

#[tokio::test]
async fn step_mode_met_contract_completes_verified() {
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let mut events = h.kernel.subscribe();
    let run = run_with(&h, "send an e-mail to Bob", Some(needs_send_email())).await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(run.verification, Some(Verification::Contract));
    assert!(run.contract_report.unwrap().satisfied);
    assert!(run.error.is_none());
    let mut saw = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Completed { verification, .. } = env.event {
            assert_eq!(verification, Verification::Contract);
            saw = true;
        }
    }
    assert!(saw);
}

#[tokio::test]
async fn result_condition_is_checked_against_the_recorded_result() {
    // `send_email` succeeded, but its result says "queued", not "sent".
    let c = contract(json!({"require": [
        {"kind": "result_equals", "action": "send_email", "pointer": "/status", "equals": "sent"}
    ]}));
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let run = run_with(&h, "send an e-mail", Some(c)).await;
    assert_eq!(run.status, RunStatus::Incomplete);
    assert!(run.error.unwrap().contains("\"/status\""));

    let c = contract(json!({"require": [
        {"kind": "result_equals", "action": "send_email", "pointer": "/status", "equals": "queued"},
        {"kind": "result_contains", "action": "send_email", "pointer": "/id", "contains": "m-"}
    ]}));
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let run = run_with(&h, "send an e-mail", Some(c)).await;
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(run.verification, Some(Verification::Contract));
}

#[tokio::test]
async fn no_contract_completes_unverified() {
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let mut events = h.kernel.subscribe();
    let run = run_with(&h, "send an e-mail", None).await;

    // Exactly the pre-ADR-0037 status; only the basis is new.
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.verification, Some(Verification::None));
    assert!(run.contract_report.is_none() && run.confirmation.is_none());
    match terminal_entry(&h.store, run.id).await {
        JournalEntry::Terminal { verification, .. } => {
            assert_eq!(verification, Some(Verification::None))
        }
        _ => unreachable!(),
    }
    let mut saw = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Completed { verification, .. } = env.event {
            assert_eq!(verification, Verification::None);
            saw = true;
        }
    }
    assert!(saw);
}

#[tokio::test]
async fn the_adr_0033_floor_still_applies_under_a_contract() {
    let h = harness(
        Scripted::steps(vec![propose("finish", json!({"summary": "sent"}))]),
        RunMode::Step,
    );
    let run = run_with(&h, "send an e-mail", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Incomplete);
    assert!(run.error.unwrap().contains("without executing any action"));
    assert_eq!(run.verification, Some(Verification::Contract));
}

#[tokio::test]
async fn failed_runs_carry_no_verification() {
    // Script exhausted after one step → no valid action → Failed.
    let h = harness(
        Scripted::steps(vec![propose("send_email", json!({"text": "x"}))]),
        RunMode::Step,
    );
    let run = run_with(&h, "send an e-mail", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.verification, None);
    assert_eq!(run.contract_report, None);
}

// ---- contract evaluation: plan mode --------------------------------------

fn plan_of(action: &str) -> serde_json::Value {
    json!({"nodes": [
        {"id": "a", "type": "tool", "action": action, "arguments": {"text": "x"}, "deps": []}
    ]})
}

#[tokio::test]
async fn plan_mode_applies_the_same_contract() {
    let h = harness(Scripted::plan(plan_of("write_file")), RunMode::Plan);
    let run = run_with(&h, "send an e-mail", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(run.verification, Some(Verification::Contract));
    assert!(run.error.unwrap().contains("success contract not met"));
    // The plan summary is kept as output.
    assert!(run.output.unwrap()["summary"]
        .as_str()
        .unwrap()
        .starts_with("plan completed"));

    let h = harness(Scripted::plan(plan_of("send_email")), RunMode::Plan);
    let run = run_with(&h, "send an e-mail", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(run.verification, Some(Verification::Contract));

    let h = harness(Scripted::plan(plan_of("write_file")), RunMode::Plan);
    let run = run_with(&h, "write it down", None).await;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.verification, Some(Verification::None));
}

// ---- validation at creation ----------------------------------------------

#[tokio::test]
async fn invalid_contract_is_refused_at_creation_and_no_run_exists() {
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    for bad in [
        json!({"require": []}),
        json!({"require": [{"kind": "action_succeeded", "action": "fax"}]}),
        json!({"require": [{"kind": "action_succeeded", "action": "finish"}]}),
        json!({"require": [{"kind": "result_equals", "action": "send_email",
                            "pointer": "status", "equals": "sent"}]}),
    ] {
        let err = h
            .kernel
            .create_run_with_contract("t".into(), None, Some(contract(bad.clone())))
            .await
            .unwrap_err();
        assert!(
            matches!(err, KernelError::InvalidRequest(_)),
            "{bad}: {err:?}"
        );
    }
    assert!(h.store.list().await.unwrap().is_empty());
}

// ---- learning gate -------------------------------------------------------

#[tokio::test]
async fn distill_skips_unverified_then_takes_it_once_confirmed() {
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let run = run_with(&h, "send an e-mail", None).await;
    assert_eq!(run.verification, Some(Verification::None));

    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.facts_created + report.plans_created, 0);

    let (confirmed, forgotten) = h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert!(forgotten.is_empty());
    let c = confirmed.confirmation.clone().unwrap();
    assert_eq!(c.principal, "eryk");
    // Status and basis are untouched: a confirmation is not a contract.
    assert_eq!(confirmed.status, RunStatus::Completed);
    assert_eq!(confirmed.verification, Some(Verification::None));
    // Durable on the stored record and in the journal, with the principal.
    assert!(h.store.get(run.id).await.unwrap().confirmation.is_some());
    let journal = h.store.journal(run.id).await.unwrap();
    let decided: Vec<_> = journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::SuccessConfirmationDecided {
                decision,
                principal,
                ..
            } => Some((*decision, principal.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decided,
        vec![(ConfirmationDecision::Confirm, "eryk".into())]
    );
    assert!(journal.last().unwrap().is_informational());

    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.facts_created, 1);
    assert_eq!(report.source_run_ids, vec![run.id]);

    // Revoke: the run leaves learning and what it alone taught is forgotten.
    let (revoked, forgotten) = h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Revoke, "eryk")
        .await
        .unwrap();
    assert!(revoked.confirmation.is_none());
    assert_eq!(forgotten.len(), 1);
    assert!(h.learned.list().await.unwrap().is_empty());
    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.facts_created + report.plans_created, 0);
    let kinds: Vec<_> = h
        .store
        .journal(run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e {
            JournalEntry::SuccessConfirmationDecided { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![ConfirmationDecision::Confirm, ConfirmationDecision::Revoke]
    );
}

#[tokio::test]
async fn distill_takes_contract_verified_runs_without_confirmation() {
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let run = run_with(&h, "send an e-mail", Some(needs_send_email())).await;
    assert_eq!(run.verification, Some(Verification::Contract));
    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.facts_created, 1);
}

#[tokio::test]
async fn plan_distillation_is_gated_the_same_way() {
    let h = harness(Scripted::plan(plan_of("write_file")), RunMode::Plan);
    let run = run_with(&h, "write it down", None).await;
    assert_eq!(h.kernel.distill(10).await.unwrap().plans_created, 0);
    h.kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert_eq!(h.kernel.distill(10).await.unwrap().plans_created, 1);
    let (_, forgotten) = h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Revoke, "eryk")
        .await
        .unwrap();
    assert_eq!(forgotten.len(), 1);
    assert!(h.learned.list_plans().await.unwrap().is_empty());
}

#[tokio::test]
async fn pre_adr_0037_completed_runs_are_unverified_until_confirmed() {
    // Maintainer's decision (2026-10-05): a run record written before ADR
    // 0037 (no `verification` key at all) was judged by the ADR 0033 floor
    // only, so it is unverified — not learnable until a human confirms it.
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    let id = uuid::Uuid::new_v4();
    let legacy: Run = serde_json::from_value(json!({
        "id": id,
        "task": "legacy task",
        "status": "completed",
        "steps": [{"index": 0, "action": "write_file", "arguments": {}, "result": {}, "ok": true}],
        "output": {"summary": "done"},
        "error": null,
        "policy": "mock",
        "created_at": 1
    }))
    .unwrap();
    assert_eq!(legacy.verification, None);
    assert!(legacy.is_unverified_success());
    assert!(!legacy.learnable());
    h.store.put(legacy).await.unwrap();
    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.facts_created + report.plans_created, 0);

    // Manual confirmation works for it, exactly as for `verification: none`.
    let (confirmed, _) = h
        .kernel
        .decide_success_confirmation(id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert_eq!(confirmed.confirmation.unwrap().principal, "eryk");
    // The confirmation does not invent a verification basis.
    assert_eq!(confirmed.verification, None);
    let report = h.kernel.distill(10).await.unwrap();
    assert_eq!(report.source_run_ids, vec![id]);
    assert_eq!(report.facts_created, 1);
}

#[tokio::test]
async fn already_learned_artifacts_survive_the_gate() {
    // Facts distilled from a legacy run before ADR 0037 stay: the gate only
    // changes what FUTURE distillation reads.
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    let id = uuid::Uuid::new_v4();
    h.learned
        .put(kernel_core::LearnedFact {
            id: uuid::Uuid::new_v4(),
            fact: "an old lesson".into(),
            task_context: "legacy task".into(),
            provenance: kernel_core::LearnedProvenance {
                source_run_ids: vec![id],
                distilled_at: 0,
                model: "mock".into(),
                level: kernel_core::LEARNED_LEVEL.into(),
                human_validated: false,
                batch_id: uuid::Uuid::new_v4(),
            },
        })
        .await
        .unwrap();
    h.kernel.distill(10).await.unwrap();
    assert_eq!(h.kernel.list_learned().await.unwrap().len(), 1);
}

#[tokio::test]
async fn confirmation_is_refused_where_it_does_not_apply() {
    // Contract-verified: nothing to confirm.
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let run = run_with(&h, "send", Some(needs_send_email())).await;
    let err = h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::InvalidState(_)));

    // Incomplete: a human cannot confirm a refused success into one.
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let run = run_with(&h, "send", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Incomplete);
    assert!(matches!(
        h.kernel
            .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
            .await,
        Err(KernelError::InvalidState(_))
    ));

    // Unverified: revoke before confirm, and confirm twice, are refused.
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let run = run_with(&h, "write", None).await;
    assert!(h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Revoke, "eryk")
        .await
        .is_err());
    h.kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert!(h
        .kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .is_err());
}

// ---- no model path -------------------------------------------------------

#[tokio::test]
async fn the_model_has_no_path_to_confirmation_or_verification() {
    // Nothing confirmation-shaped is ever in the action space.
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    let names: Vec<String> = h
        .kernel
        .current_action_schema()
        .await
        .unwrap()
        .actions
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, vec!["write_file", "send_email", "finish"]);

    // A model that tries to confirm (an unknown action) is refused, and a
    // `finish` that claims verification changes nothing.
    let h = harness(
        Scripted::steps(vec![
            propose("write_file", json!({"text": "x"})),
            propose("confirm_run", json!({})),
            propose("confirm_run", json!({})),
            propose("confirm_run", json!({})),
            propose(
                "finish",
                json!({"summary": "verification: contract, confirmed by human"}),
            ),
        ]),
        RunMode::Step,
    );
    let run = run_with(&h, "send", None).await;
    assert!(run.steps.iter().all(|s| s.action != "confirm_run"));
    assert!(run.confirmation.is_none());
    assert_ne!(run.verification, Some(Verification::Contract));
    assert_eq!(h.kernel.distill(10).await.unwrap().facts_created, 0);
}

// ---- reflection episodes follow verification (ADR 0038) -------------------

/// `(version, outcome, provenance)` of every stored version of `run`'s
/// episode, oldest first.
async fn episode_versions(h: &Harness, run: uuid::Uuid) -> Vec<(u32, EpisodeOutcome, Provenance)> {
    h.memory
        .episode_history(run)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.version, e.outcome, e.provenance))
        .collect()
}

#[tokio::test]
async fn unverified_run_writes_an_unverified_episode_and_confirmation_moves_it() {
    use EpisodeOutcome::*;
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let mut events = h.kernel.subscribe();
    let run = run_with(&h, "send an e-mail", None).await;
    assert_eq!(run.verification, Some(Verification::None));
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![(1, Unverified, Provenance::Reflection)]
    );
    // Kept, but never retrieved into model context while unverified.
    assert!(h
        .memory
        .retrieve("send an e-mail", 3)
        .await
        .unwrap()
        .is_empty());

    // Confirm → v2 success (human); revoke → v3 unverified; confirm → v4.
    h.kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert_eq!(
        episode_versions(&h, run.id).await.last().unwrap(),
        &(2, Success, Provenance::Human)
    );
    let hits = h.memory.retrieve("send an e-mail", 3).await.unwrap();
    assert_eq!(hits.len(), 1, "one episode, never one per version");
    assert!(hits[0].summary.contains("outcome: Success"), "{hits:?}");

    h.kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Revoke, "eryk")
        .await
        .unwrap();
    h.kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![
            (1, Unverified, Provenance::Reflection),
            (2, Success, Provenance::Human),
            (3, Unverified, Provenance::Human),
            (4, Success, Provenance::Human),
        ],
        "history is kept: every decision is a new version, nothing deleted"
    );
    // One id throughout.
    let ids: std::collections::HashSet<_> = h
        .memory
        .episode_history(run.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(ids.len(), 1);

    let mut revised = Vec::new();
    while let Ok(env) = events.try_recv() {
        if let RunEvent::EpisodeRevised {
            version, outcome, ..
        } = env.event
        {
            revised.push((version, outcome));
        }
    }
    assert_eq!(revised, vec![(2, Success), (3, Unverified), (4, Success)]);
}

#[tokio::test]
async fn contract_verified_and_failed_runs_keep_their_episodes() {
    // Verified by contract → `success` at once; nothing to confirm.
    let h = harness(Scripted::steps(send_then_finish()), RunMode::Step);
    let run = run_with(&h, "send", Some(needs_send_email())).await;
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![(1, EpisodeOutcome::Success, Provenance::Reflection)]
    );
    // Unmet contract → `incomplete` → `failure`, exactly as before.
    let h = harness(Scripted::steps(write_then_finish()), RunMode::Step);
    let run = run_with(&h, "send", Some(needs_send_email())).await;
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![(1, EpisodeOutcome::Failure, Provenance::Reflection)]
    );
}

#[tokio::test]
async fn plan_mode_unverified_run_writes_an_unverified_episode() {
    let h = harness(Scripted::plan(plan_of("write_file")), RunMode::Plan);
    let run = run_with(&h, "write", None).await;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![(1, EpisodeOutcome::Unverified, Provenance::Reflection)]
    );
}

#[tokio::test]
async fn pre_adr_0038_success_episode_is_left_as_it_is() {
    // A legacy run (pre ADR 0037) whose reflection episode was written as
    // `success` under the old rule: confirming and revoking the run governs
    // learning, but its episode neither changes nor gets a duplicate.
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    let id = uuid::Uuid::new_v4();
    let legacy: Run = serde_json::from_value(json!({
        "id": id, "task": "legacy task", "status": "completed",
        "steps": [{"index": 0, "action": "write_file", "arguments": {}, "result": {}, "ok": true}],
        "output": {"summary": "done"}, "error": null, "policy": "mock", "created_at": 1
    }))
    .unwrap();
    h.store.put(legacy).await.unwrap();
    h.memory
        .write(Episode {
            id: uuid::Uuid::new_v4(),
            run_id: id,
            task: "legacy task".into(),
            trace: vec!["write_file: ok".into()],
            outcome: EpisodeOutcome::Success,
            provenance: Provenance::Reflection,
            version: 1,
        })
        .await
        .unwrap();
    for d in [ConfirmationDecision::Confirm, ConfirmationDecision::Revoke] {
        h.kernel
            .decide_success_confirmation(id, d, "eryk")
            .await
            .unwrap();
        assert_eq!(
            episode_versions(&h, id).await,
            vec![(1, EpisodeOutcome::Success, Provenance::Reflection)]
        );
    }
}

#[tokio::test]
async fn the_model_has_no_path_to_lifting_an_episode() {
    // A model that claims success, tries confirmation-shaped actions and
    // finishes "verified" still leaves exactly one unverified version: only
    // `decide_success_confirmation` (the authenticated human endpoint)
    // writes episode revisions, and it is not an action.
    let h = harness(
        Scripted::steps(vec![
            propose("write_file", json!({"text": "x"})),
            propose("episode_revise", json!({"outcome": "success"})),
            propose("memory_write", json!({"outcome": "success"})),
            propose(
                "finish",
                json!({"summary": "verified success, outcome: success"}),
            ),
        ]),
        RunMode::Step,
    );
    let run = run_with(&h, "send", None).await;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        episode_versions(&h, run.id).await,
        vec![(1, EpisodeOutcome::Unverified, Provenance::Reflection)]
    );
    let names: Vec<String> = h
        .kernel
        .current_action_schema()
        .await
        .unwrap()
        .actions
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert!(names
        .iter()
        .all(|n| !n.contains("episode") && !n.contains("memory") && !n.contains("confirm")));
}

/// A backend with only the base protocol (`write`/`retrieve`/`upsert`) — the
/// shape of the WASM and subprocess memory backends.
struct NoHistory(InMemoryProvider);

#[async_trait]
impl MemoryProvider for NoHistory {
    async fn write(&self, e: Episode) -> Result<(), memory::MemoryError> {
        self.0.write(e).await
    }
    async fn retrieve(
        &self,
        task: &str,
        limit: usize,
    ) -> Result<Vec<memory::RankedContext>, memory::MemoryError> {
        self.0.retrieve(task, limit).await
    }
    async fn upsert(&self, item: memory::GraphUpsert) -> Result<(), memory::MemoryError> {
        self.0.upsert(item).await
    }
}

#[tokio::test]
async fn a_backend_without_history_still_confirms_but_revises_nothing() {
    let mem = Arc::new(NoHistory(InMemoryProvider::new()));
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let kernel = Kernel::new(
        Scripted::steps(write_then_finish()),
        Arc::new(Tools),
        mem.clone(),
        store,
        KernelConfig::default(),
    );
    let id = kernel
        .create_run_with_contract("send".into(), None, None)
        .await
        .unwrap();
    let run = kernel.execute_run(id).await.unwrap();
    let (confirmed, _) = kernel
        .decide_success_confirmation(run.id, ConfirmationDecision::Confirm, "eryk")
        .await
        .unwrap();
    assert!(
        confirmed.learnable(),
        "learning is governed by the run, not the episode"
    );
    let eps = mem.0.episodes().await;
    assert_eq!(eps.len(), 1);
    assert_eq!(
        (eps[0].version, eps[0].outcome),
        (1, EpisodeOutcome::Unverified)
    );
}

// ---- durability ------------------------------------------------------------

#[tokio::test]
async fn resume_from_a_terminal_entry_restores_the_verdict() {
    let h = harness(Scripted::steps(vec![]), RunMode::Step);
    let id = h
        .kernel
        .create_run_with_contract("t".into(), None, Some(needs_send_email()))
        .await
        .unwrap();
    let report = kernel_core::contract::evaluate(&needs_send_email(), &[]);
    h.store
        .append_journal(
            id,
            JournalEntry::Terminal {
                status: RunStatus::Incomplete,
                output: None,
                error: Some(report.unmet_reason()),
                verification: Some(Verification::Contract),
                contract_report: Some(report.clone()),
            },
        )
        .await
        .unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(run.verification, Some(Verification::Contract));
    assert_eq!(run.contract_report, Some(report));
}

#[test]
fn pre_adr_0037_terminal_entries_still_parse() {
    let old: JournalEntry =
        serde_json::from_value(json!({"kind": "terminal", "status": "completed"})).unwrap();
    match old {
        JournalEntry::Terminal { verification, .. } => assert_eq!(verification, None),
        _ => unreachable!(),
    }
}
