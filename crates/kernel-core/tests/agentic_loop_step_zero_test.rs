//! Step zero of the agentic-coding work (oculink §5): reproduce, on the LIVE
//! kernel, the predicted failure mode of a code-fix loop — a model that wants
//! to re-run the test suite after editing a file.
//!
//! These tests DOCUMENT today's contract (they are the "before" picture):
//! - Standard latitude: `uruchom_testy` re-proposed after an intervening edit
//!   is rejected as an exact duplicate (engine's anti-stall rule compares
//!   against the WHOLE run history — no notion of "state changed since"),
//!   and the run dies once the per-step attempts are exhausted.
//! - Constrained latitude (the `local:` default): it is WORSE than the
//!   reconstruction predicted — an action that succeeded once is REMOVED
//!   from the action space entirely (`narrow_action_space`), so the re-run
//!   is rejected as an unknown action before the duplicate rule is even
//!   consulted.
//!
//! If A2 (repeatable-verification) is ever implemented, the assertions here
//! are the ones that must flip.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
    RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Scripted "stubborn" oracle: pops proposals from a list; when the list is
/// exhausted it keeps repeating the LAST proposal — modeling the measured
/// behavior of a local model that keeps insisting on re-running the tests.
struct StubbornOracle {
    script: Mutex<Vec<ProposedAction>>,
    calls: AtomicUsize,
}

impl StubbornOracle {
    fn new(script: Vec<ProposedAction>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl PolicyOracle for StubbornOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().await;
        let action = if script.len() > 1 {
            script.remove(0)
        } else {
            script[0].clone()
        };
        Ok(action.into())
    }
    fn describe(&self) -> String {
        "mock:stubborn-coder".into()
    }
}

/// Two code-loop-shaped tools: `edytuj` (a mutating edit) and
/// `uruchom_testy` (runs the suite; the TOOL succeeds, the tests inside
/// fail — exactly the shape a real test-runner tool returns).
struct CodeLoopTools {
    executions: Mutex<Vec<String>>,
}

impl CodeLoopTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            executions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ActionSource for CodeLoopTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "edytuj".into(),
                    description: "Edit a file".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"plik": {"type": "string"}, "zmiana": {"type": "string"}},
                        "required": ["plik", "zmiana"],
                        "additionalProperties": false
                    }),
                },
                ActionSpec {
                    name: "uruchom_testy".into(),
                    description: "Run the test suite".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                },
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
            .push(action.action().to_string());
        match action.action() {
            "edytuj" => Ok(json!({"edited": action.arguments()["plik"]})),
            "uruchom_testy" => Ok(json!({"exit": 1, "failed": 1, "passed": 2})),
            other => Err(ExecutionError::Tool(format!("unknown tool {other}"))),
        }
    }
}

fn kernel(oracle: Arc<StubbornOracle>, tools: Arc<CodeLoopTools>, latitude: Latitude) -> Kernel {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    Kernel::new(
        oracle,
        tools,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            latitude,
            ..KernelConfig::default()
        },
    )
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

/// The reconstruction's exact prediction, on the live kernel: edit → test →
/// edit (different args) → test again (identical args) → the re-run is
/// rejected as a duplicate and the run DIES, even though the state changed
/// in between. The second `uruchom_testy` never executes.
#[tokio::test]
async fn standard_latitude_rejects_test_rerun_after_edit_and_the_run_dies() {
    let oracle = StubbornOracle::new(vec![
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix A"})),
        propose("uruchom_testy", json!({})),
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix B"})),
        propose("uruchom_testy", json!({})), // stubborn tail: repeats forever
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Standard);

    let run = k.run_task("fix the failing test".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed, "the loop must die today");
    let err = run.error.as_deref().unwrap();
    assert!(
        err.contains("no valid action"),
        "dies by attempt exhaustion: {err}"
    );
    assert!(
        err.contains("already succeeded"),
        "the killer is the duplicate rule, state-blind: {err}"
    );
    // The verification re-run NEVER executed — the loop is structurally
    // impossible under today's rule.
    let ex = tools.executions.lock().await;
    assert_eq!(
        ex.iter().filter(|a| *a == "uruchom_testy").count(),
        1,
        "the post-edit re-run must have been blocked: {ex:?}"
    );
}

/// Beyond the reconstruction: at Constrained latitude (the default for
/// `local:` engines) the re-run dies EARLIER and differently — a tool that
/// succeeded once is REMOVED from the action space, so proposing it again is
/// an unknown-action rejection, not a duplicate rejection.
#[tokio::test]
async fn constrained_latitude_removes_a_succeeded_tool_from_the_space_entirely() {
    let oracle = StubbornOracle::new(vec![
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix A"})),
        propose("uruchom_testy", json!({})),
        propose("uruchom_testy", json!({})), // stubborn tail
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Constrained);

    let run = k.run_task("fix the failing test".into()).await.unwrap();

    assert_eq!(run.status, RunStatus::Failed);
    let err = run.error.as_deref().unwrap();
    assert!(
        err.contains("not in the action space"),
        "at Constrained the tool has VANISHED, a harsher failure than the \
         duplicate rule: {err}"
    );
    let ex = tools.executions.lock().await;
    assert_eq!(ex.iter().filter(|a| *a == "uruchom_testy").count(), 1);
}
