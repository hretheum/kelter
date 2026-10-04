//! The testability thesis, demonstrated: the kernel runs end-to-end against
//! a scripted mock oracle and mock tools — no model anywhere.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
    ValidatedAction,
};
use memory::{InMemoryProvider, MemoryProvider};

struct MockTools;

#[async_trait]
impl ActionSource for MockTools {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "Echo a message".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
                    "additionalProperties": false
                }),
            }],
        })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        assert_eq!(action.action(), "echo");
        Ok(json!({"echoed": action.arguments()["message"]}))
    }
}

/// Oracle scripted with a fixed sequence of proposals.
struct ScriptedOracle {
    script: Vec<ProposedAction>,
    cursor: AtomicUsize,
}

impl ScriptedOracle {
    fn new(script: Vec<ProposedAction>) -> Self {
        Self {
            script,
            cursor: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl PolicyOracle for ScriptedOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
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

fn kernel_with(oracle: ScriptedOracle, config: KernelConfig) -> (Kernel, Arc<InMemoryProvider>) {
    let mem = Arc::new(InMemoryProvider::new());
    let kernel = Kernel::new(
        Arc::new(oracle),
        Arc::new(MockTools),
        mem.clone() as Arc<dyn MemoryProvider>,
        Arc::new(kernel_core::InMemoryRunStore::new()),
        config,
    );
    (kernel, mem)
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

#[tokio::test]
async fn run_cost_is_null_for_unpriced_model_and_a_number_for_priced() {
    use kernel_core::{ModelPrice, PricingTable};
    use std::collections::HashMap;

    // Unpriced model (mock:scripted is not in any table) → cost is null,
    // never a fabricated number.
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hi"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(oracle, KernelConfig::default());
    let run = kernel.run_task("price me".into()).await.unwrap();
    assert_eq!(run.cost_usd, None, "unpriced model must be null");

    // Priced model → a concrete dollar cost is attached to the run.
    let mut prices = HashMap::new();
    prices.insert(
        "mock:scripted".to_string(),
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
        },
    );
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hi"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(
        oracle,
        KernelConfig {
            pricing: PricingTable::new(prices),
            ..KernelConfig::default()
        },
    );
    let run = kernel.run_task("price me".into()).await.unwrap();
    assert!(run.cost_usd.is_some(), "priced model must yield a number");
}

#[tokio::test]
async fn cancel_run_marks_pending_run_canceled_durably() {
    // A created-but-not-executed run is cancellable (A2A tasks/cancel).
    let oracle = ScriptedOracle::new(vec![propose("finish", json!({"summary": "x"}))]);
    let (kernel, _) = kernel_with(oracle, KernelConfig::default());
    let run_id = kernel.create_run("cancel me".into()).await.unwrap();

    let run = kernel.cancel_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Canceled);
    // Durable: the store reflects it and it is excluded from resume.
    let stored = kernel.store().get(run_id).await.unwrap();
    assert_eq!(stored.status, RunStatus::Canceled);
    assert!(kernel.store().unfinished_runs().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancel_run_is_a_noop_on_a_terminal_run() {
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hi"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(oracle, KernelConfig::default());
    let run = kernel.run_task("finish me".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    // Cancelling a finished task must not rewrite its outcome.
    let after = kernel.cancel_run(run.id).await.unwrap();
    assert_eq!(after.status, RunStatus::Completed);
}

#[tokio::test]
async fn happy_path_completes_and_reflects() {
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hello"})),
        propose("finish", json!({"summary": "echoed hello"})),
    ]);
    let (kernel, mem) = kernel_with(oracle, KernelConfig::default());

    let run = kernel.run_task("say hello".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.steps.len(), 1);
    assert_eq!(run.steps[0].action, "echo");
    assert!(run.steps[0].ok);
    assert_eq!(run.output.unwrap()["summary"], "echoed hello");
    // Gated reflection wrote exactly one provenance-tagged episode.
    assert_eq!(mem.episode_count().await, 1);
    assert_eq!(mem.episodes().await[0].trace, vec!["echo: ok"]);
}

#[tokio::test]
async fn invalid_proposal_is_rejected_then_retried() {
    let oracle = ScriptedOracle::new(vec![
        // Unknown action → kernel rejects, retries with feedback.
        propose("rm_rf", json!({})),
        // Bad arguments → kernel rejects again.
        propose("echo", json!({"message": 42})),
        // Valid at last.
        propose("echo", json!({"message": "ok"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(oracle, KernelConfig::default());

    let run = kernel.run_task("test retry".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.steps.len(), 1);
    assert_eq!(run.steps[0].arguments["message"], "ok");
}

#[tokio::test]
async fn retry_exhaustion_fails_the_run() {
    let garbage: Vec<ProposedAction> = (0..10).map(|_| propose("nope", json!({}))).collect();
    let (kernel, mem) = kernel_with(ScriptedOracle::new(garbage), KernelConfig::default());

    let run = kernel.run_task("impossible".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.unwrap().contains("no valid action"));
    // Failures are also reflected as episodes (outcome: failure).
    assert_eq!(mem.episode_count().await, 1);
}

#[tokio::test]
async fn tool_failure_is_fed_back_not_fatal() {
    struct FailingTools;
    #[async_trait]
    impl ActionSource for FailingTools {
        async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
            MockTools.action_schema(_l).await
        }
        async fn execute(
            &self,
            _action: &ValidatedAction,
            _ctx: &ExecutionContext,
        ) -> Result<serde_json::Value, ExecutionError> {
            Err(ExecutionError::Tool("disk full".into()))
        }
    }

    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "x"})),
        propose("finish", json!({"summary": "gave up gracefully"})),
    ]);
    let kernel = Kernel::new(
        Arc::new(oracle),
        Arc::new(FailingTools),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    );

    let run = kernel.run_task("failing tool".into()).await.unwrap();

    // The failed tool call is recorded as a step, run still reaches finish —
    // but with no successful action behind it the kernel withholds success
    // (ADR 0033): `finish` after only failures is Incomplete, not Completed.
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(
        run.output.as_ref().unwrap()["summary"],
        "gave up gracefully"
    );
    assert_eq!(run.steps.len(), 1);
    assert!(!run.steps[0].ok);
    assert_eq!(run.steps[0].result["error"], "tool error: disk full");
}

#[tokio::test]
async fn constrained_latitude_narrows_action_space_after_success() {
    // Oracle that always proposes "echo": under Constrained latitude the
    // kernel removes echo from the space after its first success, so the
    // second proposal must be rejected as UnknownAction, and after retries
    // the run fails — it can never stall executing echo forever.
    let garbage: Vec<ProposedAction> = (0..10)
        .map(|_| propose("echo", json!({"message": "again"})))
        .collect();
    let (kernel, _) = kernel_with(
        ScriptedOracle::new(garbage),
        KernelConfig {
            latitude: Latitude::Constrained,
            max_steps: 4,
            ..KernelConfig::default()
        },
    );
    let run = kernel.run_task("loop bait".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    // Exactly one echo executed; the narrowed space blocked the rest.
    assert_eq!(run.steps.len(), 1);
}

#[tokio::test]
async fn duplicate_of_successful_step_is_rejected_then_alternative_accepted() {
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hi"})),
        // Exact duplicate — must be rejected with feedback, not re-run.
        propose("echo", json!({"message": "hi"})),
        // Same action, different arguments — allowed under Standard.
        propose("echo", json!({"message": "hi2"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(
        oracle,
        KernelConfig {
            latitude: Latitude::Standard,
            ..KernelConfig::default()
        },
    );
    let run = kernel.run_task("dup test".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.steps.len(), 2);
    assert_eq!(run.steps[1].arguments["message"], "hi2");
}

#[tokio::test]
async fn reflection_gate_off_writes_nothing() {
    let oracle = ScriptedOracle::new(vec![propose("finish", json!({"summary": "noop"}))]);
    let (kernel, mem) = kernel_with(
        oracle,
        KernelConfig {
            reflection_enabled: false,
            ..KernelConfig::default()
        },
    );
    kernel.run_task("noop".into()).await.unwrap();
    assert_eq!(mem.episode_count().await, 0);
}

#[tokio::test]
async fn events_stream_the_full_loop() {
    let oracle = ScriptedOracle::new(vec![
        propose("echo", json!({"message": "hi"})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let (kernel, _) = kernel_with(oracle, KernelConfig::default());
    let mut rx = kernel.subscribe();

    kernel.run_task("stream me".into()).await.unwrap();

    let mut kinds = Vec::new();
    while let Ok(env) = rx.try_recv() {
        kinds.push(
            serde_json::to_value(&env.event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(
        kinds,
        vec![
            "created",
            "proposed",
            "executed",
            "proposed",
            "reflection_written",
            "completed"
        ]
    );
}
