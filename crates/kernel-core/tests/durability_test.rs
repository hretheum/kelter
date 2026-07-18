//! Durable-execution correctness, proven at the unit level with mocks:
//! journaled model decisions replay without querying the model; receipts
//! skip re-execution; the intent→receipt crash window resolves via
//! idempotency keys or fails safely — never by silently duplicating a side
//! effect.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunStatus, RunStore, Usage, ValidatedAction, IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;

/// Oracle that counts calls and pops proposals from a script.
struct CountingOracle {
    script: Mutex<Vec<ProposedAction>>,
    calls: AtomicUsize,
    usage_per_call: Option<Usage>,
}

impl CountingOracle {
    fn new(script: Vec<ProposedAction>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            usage_per_call: None,
        })
    }
    fn with_usage(script: Vec<ProposedAction>, usage: Usage) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            usage_per_call: Some(usage),
        })
    }
}

#[async_trait]
impl PolicyOracle for CountingOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().await;
        if script.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(Proposal {
            action: script.remove(0),
            usage: self.usage_per_call,
        })
    }
    fn describe(&self) -> String {
        "mock:counting".into()
    }
}

/// Tools that count executions and record the arguments they received.
struct RecordingTools {
    keyed: bool,
    executions: Mutex<Vec<serde_json::Value>>,
}

impl RecordingTools {
    fn new(keyed: bool) -> Arc<Self> {
        Arc::new(Self {
            keyed,
            executions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ActionSource for RecordingTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let mut properties = serde_json::json!({
            "entry": {"type": "string"}
        });
        if self.keyed {
            properties[IDEMPOTENCY_KEY_ARG] = json!({"type": "string"});
        }
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "record".into(),
                description: "Record an entry (side effect)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": properties,
                    "required": ["entry"],
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
        self.executions
            .lock()
            .await
            .push(action.arguments().clone());
        Ok(json!({"recorded": true}))
    }
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

struct Setup {
    kernel: Kernel,
    store: Arc<kernel_core::InMemoryRunStore>,
    oracle: Arc<CountingOracle>,
    tools: Arc<RecordingTools>,
}

fn setup(oracle: Arc<CountingOracle>, tools: Arc<RecordingTools>) -> Setup {
    let store = Arc::new(kernel_core::InMemoryRunStore::new());
    let kernel = Kernel::new(
        oracle.clone(),
        tools.clone(),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig {
            latitude: Latitude::Standard,
            max_steps: 6,
            ..KernelConfig::default()
        },
    );
    Setup {
        kernel,
        store,
        oracle,
        tools,
    }
}

async fn seed_run(store: &kernel_core::InMemoryRunStore, kernel: &Kernel) -> Uuid {
    let _ = store; // created through the kernel so the record shape matches
    kernel.create_run("record something".into()).await.unwrap()
}

#[tokio::test]
async fn journaled_decision_replays_without_model_call() {
    // Script contains ONLY finish: if the kernel re-queried the model for
    // step 0, it would receive finish and never execute the tool.
    let s = setup(
        CountingOracle::new(vec![propose("finish", json!({"summary": "done"}))]),
        RecordingTools::new(false),
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    // Pre-crash journal: the model already decided step 0.
    s.store
        .append_journal(
            run_id,
            JournalEntry::Decision {
                step: 0,
                action: "record".into(),
                arguments: json!({"entry": "from-journal"}),
                usage: None,
            },
        )
        .await
        .unwrap();

    let run = s.kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.steps.len(), 1);
    assert_eq!(run.steps[0].arguments["entry"], "from-journal");
    // The model was consulted exactly once — for the finish decision only.
    assert_eq!(s.oracle.calls.load(Ordering::SeqCst), 1);
    // The tool executed (there was no receipt yet).
    assert_eq!(s.tools.executions.lock().await.len(), 1);
}

#[tokio::test]
async fn receipt_skips_tool_reexecution() {
    let s = setup(
        CountingOracle::new(vec![propose("finish", json!({"summary": "done"}))]),
        RecordingTools::new(false),
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    for entry in [
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
            idempotency_key: "k0".into(),
        },
        JournalEntry::Receipt {
            step: 0,
            ok: true,
            result: json!({"recorded": true}),
        },
    ] {
        s.store.append_journal(run_id, entry).await.unwrap();
    }

    let run = s.kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.steps.len(), 1);
    assert!(run.steps[0].ok);
    // The completed step was NOT re-executed.
    assert_eq!(s.tools.executions.lock().await.len(), 0);
}

#[tokio::test]
async fn dangling_intent_without_idempotency_fails_safely() {
    let s = setup(
        CountingOracle::new(vec![]),
        RecordingTools::new(false), // no idempotency_key in schema
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    for entry in [
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
            idempotency_key: "k0".into(),
        },
    ] {
        s.store.append_journal(run_id, entry).await.unwrap();
    }

    let run = s.kernel.execute_run(run_id).await.unwrap();

    // Honest failure: the side effect might have fired; never re-run blind.
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run
        .error
        .as_deref()
        .unwrap()
        .contains("Manual check required"));
    assert_eq!(s.tools.executions.lock().await.len(), 0);
    assert_eq!(s.oracle.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dangling_intent_with_key_redispatches_with_journaled_key() {
    let s = setup(
        CountingOracle::new(vec![propose("finish", json!({"summary": "done"}))]),
        RecordingTools::new(true), // declares idempotency_key
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    for entry in [
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
            idempotency_key: "journaled-key-42".into(),
        },
    ] {
        s.store.append_journal(run_id, entry).await.unwrap();
    }

    let run = s.kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    let executions = s.tools.executions.lock().await;
    assert_eq!(executions.len(), 1);
    // Re-dispatched with the SAME key so the tool can dedupe.
    assert_eq!(executions[0][IDEMPOTENCY_KEY_ARG], "journaled-key-42");
}

#[tokio::test]
async fn fresh_keyed_tool_gets_kernel_generated_key() {
    let s = setup(
        CountingOracle::new(vec![
            propose("record", json!({"entry": "hello"})),
            propose("finish", json!({"summary": "done"})),
        ]),
        RecordingTools::new(true),
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    let run = s.kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    let executions = s.tools.executions.lock().await;
    assert_eq!(
        executions[0][IDEMPOTENCY_KEY_ARG],
        format!("{run_id}:0"),
        "kernel generates a stable per-step key"
    );
    // And the intent journaled the same key.
    let journal = s.store.journal(run_id).await.unwrap();
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::Intent { idempotency_key, .. } if idempotency_key == &format!("{run_id}:0")
    )));
}

#[tokio::test]
async fn terminal_journal_entry_finalizes_without_work() {
    let s = setup(CountingOracle::new(vec![]), RecordingTools::new(false));
    let run_id = seed_run(&s.store, &s.kernel).await;
    s.store
        .append_journal(
            run_id,
            JournalEntry::Terminal {
                status: RunStatus::Completed,
                output: Some(json!({"summary": "was done"})),
                error: None,
            },
        )
        .await
        .unwrap();

    let run = s.kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.output.unwrap()["summary"], "was done");
    assert_eq!(s.oracle.calls.load(Ordering::SeqCst), 0);
    assert_eq!(s.tools.executions.lock().await.len(), 0);
}

#[tokio::test]
async fn usage_accumulates_per_run() {
    let s = setup(
        CountingOracle::with_usage(
            vec![
                propose("record", json!({"entry": "a"})),
                propose("finish", json!({"summary": "done"})),
            ],
            Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                calls: 1,
            },
        ),
        RecordingTools::new(false),
    );
    let run_id = seed_run(&s.store, &s.kernel).await;
    let run = s.kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.usage.prompt_tokens, 200);
    assert_eq!(run.usage.completion_tokens, 20);
    assert_eq!(run.usage.calls, 2);
}
