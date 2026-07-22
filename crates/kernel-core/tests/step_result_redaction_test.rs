//! E3 — secret redaction covers `StepRecord.result` and journal `Receipt`s:
//! a tool that reads a secret (a `.env` value, a Bearer header) must not
//! persist it into the durable run record, the journal, or — transitively —
//! any later prompt built from those steps.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

const SECRET: &str = "sk-or-v1-THIS-IS-A-REAL-LOOKING-KEY-42";

struct EnvReadingTools;

#[async_trait]
impl ActionSource for EnvReadingTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "read_env".into(),
                description: "read a config file".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            }],
        })
    }
    async fn execute(
        &self,
        _action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        // The tool legitimately read a secret-bearing file.
        Ok(json!({
            "content": format!("OPENROUTER_API_KEY={SECRET}\nAuthorization: Bearer {SECRET}"),
            "nested": [{"line": format!("token: {SECRET}")}],
        }))
    }
}

struct OneShotOracle;

#[async_trait]
impl PolicyOracle for OneShotOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        // The prompt-facing steps must already be scrubbed by the time they
        // reach the oracle.
        for s in &context.steps {
            assert!(
                !serde_json::to_string(&s.result).unwrap().contains(SECRET),
                "secret leaked into the policy context"
            );
        }
        let action = if context.steps.is_empty() {
            ProposedAction {
                action: "read_env".into(),
                arguments: json!({}),
                rationale: None,
            }
        } else {
            ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "done"}),
                rationale: None,
            }
        };
        Ok(action.into())
    }
    fn describe(&self) -> String {
        "mock:env".into()
    }
}

#[tokio::test]
async fn step_results_and_receipts_never_persist_a_secret() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let k = Kernel::new(
        Arc::new(OneShotOracle),
        Arc::new(EnvReadingTools),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig::default(),
    );

    let run = k.run_task("read the env file".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    // The durable StepRecord carries the masked value, never the secret.
    let step_json = serde_json::to_string(&run.steps).unwrap();
    assert!(
        !step_json.contains(SECRET),
        "StepRecord.result leaked the secret: {step_json}"
    );
    assert!(
        step_json.contains("<redacted>"),
        "mask missing: {step_json}"
    );

    // The journal receipts carry the masked value too.
    let journal = store.journal(run.id).await.unwrap();
    for entry in &journal {
        if let JournalEntry::Receipt { result, .. } = entry {
            let s = serde_json::to_string(result).unwrap();
            assert!(!s.contains(SECRET), "Receipt leaked the secret: {s}");
        }
    }
}
