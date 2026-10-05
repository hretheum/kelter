//! Test support shared by the memory backends' crates (feature `testkit`,
//! enabled only from dev-dependencies — never part of a shipped build).
//!
//! [`episode_context_probe`] drives the REAL kernel against a given memory
//! backend and reports what the model was shown, so every backend can prove
//! the ADR 0038 rule end to end: an `unverified` episode never reaches the
//! model's context, and the same episode does once a human confirms it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use memory::MemoryProvider;
use serde_json::json;
use uuid::Uuid;

use crate::{
    ActionSchema, ActionSource, ActionSpec, ConfirmationDecision, ExecutionContext, ExecutionError,
    InMemoryRunStore, Kernel, KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle,
    Proposal, ProposedAction, RunStatus, ValidatedAction, Verification,
};

/// What the model saw in the probe.
#[derive(Debug)]
pub struct ContextProbe {
    /// The run whose episode is probed (ended `completed`, unverified).
    pub run_id: Uuid,
    /// Memory context handed to the model while that episode was
    /// `unverified`.
    pub before_confirmation: Vec<String>,
    /// Memory context handed to the model after a human confirmed it.
    pub after_confirmation: Vec<String>,
}

struct NoteTool;

#[async_trait]
impl ActionSource for NoteTool {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "note".into(),
                description: "Write a note".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            }],
        })
    }
    async fn execute(
        &self,
        _a: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        Ok(json!({"ok": true}))
    }
}

/// Every run: one effective `note`, then `finish` — so it ends `completed`
/// with `verification: none` (no contract). Records the memory context of
/// each run's first decision.
#[derive(Default)]
struct Recorder {
    first_contexts: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl PolicyOracle for Recorder {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let (action, arguments) = if context.steps.is_empty() {
            self.first_contexts
                .lock()
                .unwrap()
                .push(context.memory.clone());
            ("note", json!({"text": "done"}))
        } else {
            ("finish", json!({"summary": "done"}))
        };
        Ok(ProposedAction {
            action: action.into(),
            arguments,
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:context-recorder".into()
    }
}

/// Run `task` three times on one kernel over `memory`:
///
/// 1. the probed run (no contract → `completed`, `verification: none`, its
///    episode `unverified`);
/// 2. a second run — its context is `before_confirmation`;
/// 3. a human confirms run 1 (`decide_success_confirmation`), then a third
///    run — its context is `after_confirmation`.
///
/// Give `task` a token unique to the test so a shared backend (Postgres)
/// cannot crowd the probe out with older rows.
pub async fn episode_context_probe(memory: Arc<dyn MemoryProvider>, task: &str) -> ContextProbe {
    let oracle = Arc::new(Recorder::default());
    let kernel = Kernel::new(
        oracle.clone(),
        Arc::new(NoteTool),
        memory,
        Arc::new(InMemoryRunStore::new()),
        KernelConfig::default(),
    );
    let probed = run_once(&kernel, task).await;
    assert_eq!(probed.status, RunStatus::Completed);
    assert_eq!(probed.verification, Some(Verification::None));
    run_once(&kernel, task).await;
    kernel
        .decide_success_confirmation(probed.id, ConfirmationDecision::Confirm, "tester")
        .await
        .unwrap();
    run_once(&kernel, task).await;
    let seen = oracle.first_contexts.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "one first decision per run");
    ContextProbe {
        run_id: probed.id,
        before_confirmation: seen[1].clone(),
        after_confirmation: seen[2].clone(),
    }
}

async fn run_once(kernel: &Kernel, task: &str) -> crate::Run {
    let id = kernel
        .create_run_with_contract(task.to_string(), None, None)
        .await
        .unwrap();
    kernel.execute_run(id).await.unwrap()
}
