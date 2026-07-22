//! The A2 contract (duplicate-rule refinement), on the live kernel. This
//! file BEGAN as the step-zero reproduction of the pre-A2 failure (both
//! deaths were confirmed deterministically and live — see the oculink
//! evidence pack); after the human gate approved A2, the assertions flipped
//! to pin the refined rule:
//!
//! - a MUTATING step (an action declaring `idempotency_key` — the kernel's
//!   side-effect convention) starts a new state epoch;
//! - an exact repeat WITHIN the epoch (no state change since) is still
//!   rejected — the anti-stall rule is refined, not removed (DoD #1);
//! - verification AFTER a mutation executes normally — the loop lives;
//! - Constrained's action-space narrowing is epoch-scoped the same way
//!   (DoD #2) — a mutation restores dropped tools; within an epoch the
//!   pre-A2 removal still applies;
//! - durable resume replays completed steps without re-executing them and
//!   post-resume verification is legal (DoD #3 — the exact live death
//!   observed before A2).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, JournalEntry, Kernel,
    KernelConfig, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction,
    RunStatus, RunStore, ValidatedAction,
};
use memory::InMemoryProvider;

/// Scripted "stubborn" oracle: pops proposals from a list; when exhausted it
/// repeats the LAST proposal — modeling a model that keeps insisting.
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

/// Code-loop-shaped tools mirroring the real code-server surface:
/// `czytaj` (read-only, NO idempotency_key), `edytuj` and `uruchom_testy`
/// (both keyed → mutating under the A2 convention, like popraw_kod /
/// uruchom_komende).
struct CodeLoopTools {
    executions: Mutex<Vec<String>>,
}

impl CodeLoopTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            executions: Mutex::new(Vec::new()),
        })
    }
    async fn count(&self, action: &str) -> usize {
        self.executions
            .lock()
            .await
            .iter()
            .filter(|a| *a == action)
            .count()
    }
}

#[async_trait]
impl ActionSource for CodeLoopTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![
                ActionSpec {
                    name: "czytaj".into(),
                    description: "Read a file (read-only)".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"plik": {"type": "string"}},
                        "required": ["plik"],
                        "additionalProperties": false
                    }),
                },
                ActionSpec {
                    name: "edytuj".into(),
                    description: "Edit a file (mutating)".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "plik": {"type": "string"},
                            "zmiana": {"type": "string"},
                            "idempotency_key": {"type": "string"}
                        },
                        "required": ["plik", "zmiana"],
                        "additionalProperties": false
                    }),
                },
                ActionSpec {
                    name: "uruchom_testy".into(),
                    description: "Run the test suite (keyed command)".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"idempotency_key": {"type": "string"}},
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
            "czytaj" => Ok(json!({"tresc": "    1\tfn mediana() {}"})),
            "edytuj" => Ok(json!({"edited": action.arguments()["plik"]})),
            "uruchom_testy" => Ok(json!({"exit": 1, "failed": 1, "passed": 2})),
            other => Err(ExecutionError::Tool(format!("unknown tool {other}"))),
        }
    }
}

fn kernel_with(
    oracle: Arc<StubbornOracle>,
    tools: Arc<CodeLoopTools>,
    store: Arc<dyn RunStore>,
    latitude: Latitude,
) -> Kernel {
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

fn kernel(oracle: Arc<StubbornOracle>, tools: Arc<CodeLoopTools>, latitude: Latitude) -> Kernel {
    kernel_with(
        oracle,
        tools,
        Arc::new(kernel_core::InMemoryRunStore::new()),
        latitude,
    )
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

/// THE A2 money shot: the trajectory that deterministically died before A2
/// (edit → test → edit' → test with identical arguments) now completes —
/// the post-edit re-run is verification after a mutation and executes.
#[tokio::test]
async fn test_rerun_after_edit_now_executes_and_the_loop_completes() {
    let oracle = StubbornOracle::new(vec![
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix A"})),
        propose("uruchom_testy", json!({})),
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix B"})),
        propose("uruchom_testy", json!({})), // identical to step 1 — pre-A2 death
        propose("finish", json!({"summary": "done"})),
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Standard);

    let run = k.run_task("fix the failing test".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    assert_eq!(
        tools.count("uruchom_testy").await,
        2,
        "the post-edit re-run must EXECUTE now"
    );
    assert_eq!(tools.count("edytuj").await, 2);
}

/// DoD #1 — the rule is refined, NOT removed: an exact repeat with no state
/// change in between is still rejected and still kills a stubborn run.
/// Both flavors: a keyed verification command repeated back-to-back, and a
/// read-only action repeated back-to-back.
#[tokio::test]
async fn pointless_repeat_without_state_change_is_still_rejected() {
    // Keyed command (uruchom_testy → uruchom_testy, no edit in between).
    let oracle = StubbornOracle::new(vec![
        propose("uruchom_testy", json!({})),
        propose("uruchom_testy", json!({})), // stubborn tail
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Standard);
    let run = k.run_task("run the tests twice".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let err = run.error.as_deref().unwrap();
    assert!(err.contains("no valid action"), "err: {err}");
    assert!(err.contains("already succeeded"), "err: {err}");
    assert_eq!(
        tools.count("uruchom_testy").await,
        1,
        "the stateless repeat must NOT execute"
    );

    // Read-only action (czytaj → czytaj, nothing mutating anywhere).
    let oracle = StubbornOracle::new(vec![
        propose("czytaj", json!({"plik": "lib.rs"})),
        propose("czytaj", json!({"plik": "lib.rs"})), // stubborn tail
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Standard);
    let run = k.run_task("read the file twice".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.error.as_deref().unwrap().contains("already succeeded"),
        "err: {:?}",
        run.error
    );
    assert_eq!(tools.count("czytaj").await, 1);
}

/// A repeat of the MUTATION ITSELF with identical arguments (edit X → edit
/// X, nothing else in between) is still a stall — the duplicate window
/// includes the epoch's anchor.
#[tokio::test]
async fn identical_re_edit_without_other_changes_is_still_rejected() {
    let oracle = StubbornOracle::new(vec![
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix A"})),
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix A"})), // stubborn tail
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Standard);
    let run = k.run_task("edit twice, identically".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        tools.count("edytuj").await,
        1,
        "identical re-edit must not run"
    );
}

/// DoD #2 — Constrained narrowing is epoch-scoped: a tool that succeeded
/// and was dropped from the space comes BACK after a mutation, so the
/// edit→verify loop is possible at Constrained too.
#[tokio::test]
async fn constrained_mutation_restores_a_dropped_tool() {
    let oracle = StubbornOracle::new(vec![
        propose("uruchom_testy", json!({})),
        propose("edytuj", json!({"plik": "lib.rs", "zmiana": "fix"})),
        propose("uruchom_testy", json!({})), // pre-A2: "not in the action space"
        propose("finish", json!({"summary": "done"})),
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Constrained);
    let run = k.run_task("test, fix, re-test".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    assert_eq!(
        tools.count("uruchom_testy").await,
        2,
        "the mutation must restore the dropped tool"
    );
}

/// DoD #2 (other half) — WITHIN an epoch the pre-A2 narrowing still
/// applies: a read-only tool that succeeded (no mutation since) is removed
/// from the Constrained space; proposing it again is an unknown action.
#[tokio::test]
async fn constrained_within_epoch_removal_still_applies() {
    let oracle = StubbornOracle::new(vec![
        propose("czytaj", json!({"plik": "lib.rs"})),
        propose("czytaj", json!({"plik": "other.rs"})), // same TOOL, different args
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel(oracle.clone(), tools.clone(), Latitude::Constrained);
    let run = k.run_task("read repeatedly".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("not in the action space"),
        "err: {:?}",
        run.error
    );
    assert_eq!(tools.count("czytaj").await, 1);
}

/// DoD #3 — durability/exactly-once re-proven ON TOP of A2, reproducing the
/// exact live death observed before it: czytaj + popraw (keyed, journaled)
/// completed, crash, resume — the completed steps replay WITHOUT
/// re-executing (no double side effect, model not re-queried about
/// journaled decisions), and the post-resume re-read (identical arguments —
/// legitimate re-orientation after a mutation) now EXECUTES instead of
/// killing the run.
#[tokio::test]
async fn resume_replays_without_reexecution_and_allows_post_resume_reread() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = StubbornOracle::new(vec![
        propose("czytaj", json!({"plik": "lib.rs"})), // identical to journaled step 0
        propose("uruchom_testy", json!({})),
        propose("finish", json!({"summary": "done"})),
    ]);
    let tools = CodeLoopTools::new();
    let k = kernel_with(
        oracle.clone(),
        tools.clone(),
        store.clone(),
        Latitude::Standard,
    );

    let run_id = k.create_run("resume the coding loop".into()).await.unwrap();
    // Journal a completed pre-crash session: czytaj (step 0), edytuj (step 1,
    // keyed → the mutation that opens a new epoch).
    for (step, action, args, result) in [
        (
            0u32,
            "czytaj",
            json!({"plik": "lib.rs"}),
            json!({"tresc": "    1\tfn mediana() {}"}),
        ),
        (
            1u32,
            "edytuj",
            json!({"plik": "lib.rs", "zmiana": "fix"}),
            json!({"edited": "lib.rs"}),
        ),
    ] {
        store
            .append_journal(
                run_id,
                JournalEntry::Decision {
                    step,
                    action: action.into(),
                    arguments: args.clone(),
                    usage: None,
                },
            )
            .await
            .unwrap();
        store
            .append_journal(
                run_id,
                JournalEntry::Receipt {
                    step,
                    ok: true,
                    result,
                },
            )
            .await
            .unwrap();
    }

    let run = k.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
    // Exactly-once: the journaled steps did NOT re-execute — this session
    // ran only the post-resume czytaj + uruchom_testy.
    let ex = tools.executions.lock().await.clone();
    assert_eq!(
        ex,
        vec!["czytaj".to_string(), "uruchom_testy".to_string()],
        "journaled steps must not re-execute"
    );
    // The model was queried only for the fresh steps (3 calls: czytaj,
    // uruchom_testy, finish) — never about the journaled decisions.
    assert_eq!(oracle.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        run.steps.len(),
        4,
        "2 replayed + 2 fresh + finish is terminal"
    );
}
