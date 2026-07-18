//! The deterministic kernel: a state machine that owns control flow, retries,
//! persistence and error handling. The model is consulted at explicit
//! decision points and never executes anything itself.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::Instrument;
use uuid::Uuid;

use crate::action::{ActionSchema, ActionSpec, ValidatedAction};
use crate::journal::{fold_step, JournalEntry, Usage};
use crate::oracle::{Latitude, PolicyContext, PolicyOracle};
use crate::store::RunStore;
use memory::{Episode, EpisodeOutcome, MemoryProvider, Provenance};

/// Name of the terminal action the kernel itself adds to every action space.
pub const FINISH_ACTION: &str = "finish";

/// The tool substrate seam: something that can describe an action space and
/// execute validated actions (in practice: the MCP client crate).
#[async_trait]
pub trait ActionSource: Send + Sync {
    async fn action_schema(&self, latitude: Latitude) -> Result<ActionSchema, ExecutionError>;
    async fn execute(&self, action: &ValidatedAction) -> Result<serde_json::Value, ExecutionError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("tool error: {0}")]
    Tool(String),
    #[error("tool substrate unavailable: {0}")]
    Substrate(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

/// One executed step: which action ran, with what arguments, and how it went.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: u32,
    pub action: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub task: String,
    pub status: RunStatus,
    pub steps: Vec<StepRecord>,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    /// Which adapter drove the run (e.g. "local:...", "openrouter:...").
    pub policy: String,
    pub created_at: u64,
    /// Aggregated model-call accounting for this run.
    #[serde(default)]
    pub usage: Usage,
}

/// Events streamed to clients over WS/SSE while a run progresses.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    Created {
        task: String,
        policy: String,
    },
    Proposed {
        step: u32,
        action: String,
        arguments: serde_json::Value,
        rationale: Option<String>,
    },
    Rejected {
        step: u32,
        reason: String,
        attempt: u32,
    },
    Executed {
        step: u32,
        action: String,
        ok: bool,
        result: serde_json::Value,
    },
    ReflectionWritten {
        episode_id: Uuid,
    },
    Completed {
        output: serde_json::Value,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct RunEventEnvelope {
    pub run_id: Uuid,
    pub seq: u64,
    pub event: RunEvent,
}

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub max_steps: u32,
    pub latitude: Latitude,
    /// Gate for the self-improvement stub: when false, no reflection episode
    /// is ever written. No silent memory self-mutation.
    pub reflection_enabled: bool,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            max_steps: 12,
            latitude: Latitude::Standard,
            reflection_enabled: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

pub struct Kernel {
    oracle: Arc<dyn PolicyOracle>,
    tools: Arc<dyn ActionSource>,
    memory: Arc<dyn MemoryProvider>,
    store: Arc<dyn RunStore>,
    events: broadcast::Sender<RunEventEnvelope>,
    config: KernelConfig,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Kernel {
    pub fn new(
        oracle: Arc<dyn PolicyOracle>,
        tools: Arc<dyn ActionSource>,
        memory: Arc<dyn MemoryProvider>,
        store: Arc<dyn RunStore>,
        config: KernelConfig,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            oracle,
            tools,
            memory,
            store,
            events,
            config,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RunEventEnvelope> {
        self.events.subscribe()
    }

    pub fn store(&self) -> Arc<dyn RunStore> {
        self.store.clone()
    }

    pub fn policy_description(&self) -> String {
        self.oracle.describe()
    }

    /// The action space currently offered at a fresh decision point
    /// (tools at the configured latitude + the kernel-owned finish action).
    pub async fn current_action_schema(&self) -> Result<ActionSchema, ExecutionError> {
        let mut schema = self.tools.action_schema(self.config.latitude).await?;
        schema.actions.push(finish_spec());
        Ok(schema)
    }

    /// Create a run and execute it to completion. Returns the final run.
    /// Callers that want fire-and-forget wrap this in `tokio::spawn`.
    pub async fn run_task(&self, task: String) -> Result<Run, KernelError> {
        let run_id = self.create_run(task).await?;
        self.execute_run(run_id).await
    }

    /// Create a pending run without executing it yet.
    pub async fn create_run(&self, task: String) -> Result<Uuid, KernelError> {
        let run = Run {
            id: Uuid::new_v4(),
            task: task.clone(),
            status: RunStatus::Pending,
            steps: Vec::new(),
            output: None,
            error: None,
            policy: self.oracle.describe(),
            created_at: now_secs(),
            usage: Usage::default(),
        };
        let id = run.id;
        self.store.put(run).await?;
        self.emit(
            id,
            0,
            RunEvent::Created {
                task,
                policy: self.oracle.describe(),
            },
        );
        Ok(id)
    }

    fn emit(&self, run_id: Uuid, seq: u64, event: RunEvent) {
        // A send error only means no subscriber is listening right now.
        let _ = self.events.send(RunEventEnvelope { run_id, seq, event });
    }

    /// The state machine. Deterministic control flow; the model only picks
    /// the next action from the offered space.
    ///
    /// Durable execution: decisions, intents and receipts are journaled as
    /// they happen. Calling this again for an interrupted run *resumes* it —
    /// completed steps are not re-executed and journaled model decisions are
    /// replayed without querying the model again.
    pub async fn execute_run(&self, run_id: Uuid) -> Result<Run, KernelError> {
        let span =
            tracing::info_span!("kelter.run", run_id = %run_id, policy = %self.oracle.describe());
        self.execute_run_inner(run_id).instrument(span).await
    }

    async fn execute_run_inner(&self, run_id: Uuid) -> Result<Run, KernelError> {
        let mut run = self.store.get(run_id).await?;
        let journal = self.store.journal(run_id).await?;

        // Crash happened after the terminal journal entry but before the run
        // record was finalized: finish the bookkeeping from the journal.
        if let Some(JournalEntry::Terminal {
            status,
            output,
            error,
        }) = journal
            .iter()
            .rev()
            .find(|e| matches!(e, JournalEntry::Terminal { .. }))
        {
            tracing::info!(%run_id, "resume: run already terminal in journal — finalizing record");
            run.status = *status;
            run.output = output.clone();
            run.error = error.clone();
            self.store.put(run.clone()).await?;
            return Ok(run);
        }

        let resumed = !run.steps.is_empty() || !journal.is_empty();
        if resumed {
            tracing::info!(
                %run_id,
                completed_steps = run.steps.len(),
                journal_entries = journal.len(),
                "resume: continuing run from durable journal"
            );
        }

        run.status = RunStatus::Running;
        self.store.put(run.clone()).await?;

        let latitude = self.config.latitude;
        let mut seq: u64 = 1;

        let memory_context: Vec<String> = self
            .memory
            .retrieve(&run.task, 3)
            .await
            .map(|hits| hits.into_iter().map(|h| h.summary).collect())
            .unwrap_or_default();

        let mut last_error: Option<String> = None;
        let mut terminal: Option<(RunStatus, Option<serde_json::Value>, Option<String>)> = None;

        let first_step = run.steps.len() as u32;
        for step_index in first_step..self.config.max_steps {
            // Journal state for this step (only ever non-empty for the first
            // iteration after a resume).
            let folded = fold_step(&journal, step_index);
            let step_span = tracing::info_span!("kelter.step", step = step_index);

            // 1. Build the action space: tools + the kernel-owned finish action.
            let mut schema = self.tools.action_schema(latitude).await?;
            // Constrained latitude = harder decomposition: drop tools that
            // already succeeded so a weak engine cannot stall on one tool.
            if latitude.narrow_action_space() {
                let done: std::collections::HashSet<&str> = run
                    .steps
                    .iter()
                    .filter(|s| s.ok)
                    .map(|s| s.action.as_str())
                    .collect();
                schema.actions.retain(|a| !done.contains(a.name.as_str()));
            }
            schema.actions.push(finish_spec());

            // 2. Obtain the validated action: replay the journaled decision,
            //    or consult the policy oracle (and journal the outcome).
            let validated = if let Some((action, arguments)) = folded.decision.clone() {
                // THE replay law: the model made this decision once; it is
                // never asked again.
                tracing::info!(
                    %run_id,
                    step = step_index,
                    action = %action,
                    "resume: replaying journaled decision — model NOT queried"
                );
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Proposed {
                        step: step_index,
                        action: action.clone(),
                        arguments: arguments.clone(),
                        rationale: Some("replayed from journal".into()),
                    },
                );
                seq += 1;
                match schema.validate(crate::action::ProposedAction {
                    action,
                    arguments,
                    rationale: None,
                }) {
                    Ok(v) => v,
                    Err(e) => {
                        terminal = Some((
                            RunStatus::Failed,
                            None,
                            Some(format!(
                                "journaled decision for step {step_index} no longer validates \
                                 ({e}) — tool configuration changed between run and resume"
                            )),
                        ));
                        break;
                    }
                }
            } else {
                let mut chosen: Option<ValidatedAction> = None;
                for attempt in 0..latitude.max_retries() {
                    let context = PolicyContext {
                        task: run.task.clone(),
                        steps: run.steps.clone(),
                        memory: memory_context.clone(),
                        last_error: last_error.clone(),
                        latitude,
                        guidance: latitude.inject_guidance().then(|| {
                            "Work in small steps: one tool per step. Re-read the task and prior \
                             steps, pick exactly one NEXT action — never one that already \
                             succeeded — and finish only when the task's output has been produced."
                                .to_string()
                        }),
                    };
                    let model_span = tracing::info_span!(
                        parent: &step_span,
                        "kelter.model_call",
                        step = step_index,
                        attempt
                    );
                    let proposal = match self
                        .oracle
                        .propose(&context, &schema)
                        .instrument(model_span)
                        .await
                    {
                        Ok(p) => p,
                        Err(e) => {
                            let reason = format!("policy error: {e}");
                            tracing::warn!(%run_id, step = step_index, attempt, "{reason}");
                            self.emit(
                                run_id,
                                seq,
                                RunEvent::Rejected {
                                    step: step_index,
                                    reason: reason.clone(),
                                    attempt,
                                },
                            );
                            seq += 1;
                            last_error = Some(reason);
                            continue;
                        }
                    };
                    let usage = proposal.usage;
                    let proposed = proposal.action;

                    self.emit(
                        run_id,
                        seq,
                        RunEvent::Proposed {
                            step: step_index,
                            action: proposed.action.clone(),
                            arguments: proposed.arguments.clone(),
                            rationale: proposed.rationale.clone(),
                        },
                    );
                    seq += 1;

                    // Kernel-side validation: the trust boundary.
                    let validation = tracing::info_span!(
                        parent: &step_span,
                        "kelter.validate",
                        step = step_index
                    )
                    .in_scope(|| schema.validate(proposed));
                    match validation {
                        Ok(v) => {
                            // Deterministic anti-stall rule: outside Wide
                            // latitude, an exact repeat of any earlier step is
                            // rejected with feedback, not re-executed.
                            let duplicate_of = latitude
                                .reject_duplicates()
                                .then(|| {
                                    run.steps.iter().find(|s| {
                                        s.action == v.action() && &s.arguments == v.arguments()
                                    })
                                })
                                .flatten();
                            if let Some(prior) = duplicate_of {
                                let reason = if prior.ok {
                                    format!(
                                        "action '{}' with these exact arguments already \
                                         succeeded in an earlier step — choose a different \
                                         action",
                                        v.action()
                                    )
                                } else {
                                    format!(
                                        "action '{}' with these exact arguments already FAILED \
                                         earlier ({}) — change the arguments or pick a \
                                         different action",
                                        v.action(),
                                        prior.result
                                    )
                                };
                                self.emit(
                                    run_id,
                                    seq,
                                    RunEvent::Rejected {
                                        step: step_index,
                                        reason: reason.clone(),
                                        attempt,
                                    },
                                );
                                seq += 1;
                                last_error = Some(reason);
                                continue;
                            }

                            // Memoization point: the accepted decision (and
                            // its cost) is journaled BEFORE anything acts on
                            // it, so replay never needs the model.
                            if let Some(u) = usage {
                                run.usage.add(&u);
                            }
                            self.store
                                .append_journal(
                                    run_id,
                                    JournalEntry::Decision {
                                        step: step_index,
                                        action: v.action().to_string(),
                                        arguments: v.arguments().clone(),
                                        usage,
                                    },
                                )
                                .await?;
                            last_error = None;
                            chosen = Some(v);
                            break;
                        }
                        Err(e) => {
                            let reason = e.to_string();
                            self.emit(
                                run_id,
                                seq,
                                RunEvent::Rejected {
                                    step: step_index,
                                    reason: reason.clone(),
                                    attempt,
                                },
                            );
                            seq += 1;
                            last_error = Some(format!(
                                "your previous proposal was rejected: {reason}. Propose again."
                            ));
                        }
                    }
                }

                match chosen {
                    Some(v) => v,
                    None => {
                        terminal = Some((
                            RunStatus::Failed,
                            None,
                            Some(format!(
                                "no valid action after {} attempts at step {}: {}",
                                latitude.max_retries(),
                                step_index,
                                last_error.as_deref().unwrap_or("unknown")
                            )),
                        ));
                        break;
                    }
                }
            };

            // 3. Terminal action is handled by the kernel, not a tool.
            if validated.action() == FINISH_ACTION {
                terminal = Some((
                    RunStatus::Completed,
                    Some(validated.arguments().clone()),
                    None,
                ));
                break;
            }

            // 4. Execute through the tool substrate, inside the durable
            //    intent → receipt boundary.
            let (result, ok) = if let Some((ok, result)) = folded.receipt.clone() {
                // The tool already ran to completion before the crash; the
                // receipt is authoritative. No re-execution.
                tracing::info!(
                    %run_id,
                    step = step_index,
                    action = %validated.action(),
                    "resume: receipt found — skipping tool execution"
                );
                (result, ok)
            } else {
                let keyed = schema
                    .spec(validated.action())
                    .map(|s| s.supports_idempotency_key())
                    .unwrap_or(false);

                let idempotency_key = match folded.intent.clone() {
                    Some((_, _, journaled_key)) => {
                        // Dangling intent: the tool may or may not have run.
                        if !keyed {
                            terminal = Some((
                                RunStatus::Failed,
                                None,
                                Some(format!(
                                    "cannot safely resume step {step_index}: tool '{}' was \
                                     dispatched before the crash but wrote no receipt, and it \
                                     declares no idempotency_key support — a re-run could \
                                     duplicate its side effect. Manual check required.",
                                    validated.action()
                                )),
                            ));
                            break;
                        }
                        tracing::info!(
                            %run_id,
                            step = step_index,
                            action = %validated.action(),
                            "resume: dangling intent — re-dispatching with the journaled \
                             idempotency key (tool-side dedupe guarantees exactly-once)"
                        );
                        journaled_key
                    }
                    None => {
                        let key = format!("{run_id}:{step_index}");
                        self.store
                            .append_journal(
                                run_id,
                                JournalEntry::Intent {
                                    step: step_index,
                                    action: validated.action().to_string(),
                                    arguments: validated.arguments().clone(),
                                    idempotency_key: key.clone(),
                                },
                            )
                            .await?;
                        key
                    }
                };

                crash_point("after_intent", validated.action());

                let mut exec_action = validated.clone();
                if keyed {
                    exec_action.inject_idempotency_key(&idempotency_key);
                }

                let tool_span = tracing::info_span!(
                    parent: &step_span,
                    "kelter.tool_call",
                    step = step_index,
                    tool = %validated.action()
                );
                let (result, ok) =
                    match self.tools.execute(&exec_action).instrument(tool_span).await {
                        Ok(v) => (v, true),
                        Err(e) => {
                            let msg = e.to_string();
                            last_error = Some(format!(
                                "tool '{}' failed: {msg}. Adjust and continue.",
                                validated.action()
                            ));
                            (serde_json::json!({ "error": msg }), false)
                        }
                    };

                crash_point("after_execute", validated.action());

                self.store
                    .append_journal(
                        run_id,
                        JournalEntry::Receipt {
                            step: step_index,
                            ok,
                            result: result.clone(),
                        },
                    )
                    .await?;
                (result, ok)
            };

            let record = StepRecord {
                index: step_index,
                action: validated.action().to_string(),
                arguments: validated.arguments().clone(),
                result: result.clone(),
                ok,
            };
            run.steps.push(record);
            self.store.put(run.clone()).await?;
            self.emit(
                run_id,
                seq,
                RunEvent::Executed {
                    step: step_index,
                    action: validated.action().to_string(),
                    ok,
                    result,
                },
            );
            seq += 1;
        }

        let (status, output, error) = terminal.unwrap_or((
            RunStatus::Failed,
            None,
            Some(format!(
                "step budget exhausted ({} steps) without finish",
                self.config.max_steps
            )),
        ));

        // Terminal state is journaled first, then applied to the run record.
        self.store
            .append_journal(
                run_id,
                JournalEntry::Terminal {
                    status,
                    output: output.clone(),
                    error: error.clone(),
                },
            )
            .await?;

        run.status = status;
        run.output = output.clone();
        run.error = error.clone();
        self.store.put(run.clone()).await?;

        // 5. Gated, provenance-tagged reflection stub: write an episode.
        if self.config.reflection_enabled {
            let episode = Episode {
                id: Uuid::new_v4(),
                run_id: run.id,
                task: run.task.clone(),
                trace: run
                    .steps
                    .iter()
                    .map(|s| format!("{}: {}", s.action, if s.ok { "ok" } else { "failed" }))
                    .collect(),
                outcome: if run.status == RunStatus::Completed {
                    EpisodeOutcome::Success
                } else {
                    EpisodeOutcome::Failure
                },
                provenance: Provenance::Reflection,
                version: 1,
            };
            let episode_id = episode.id;
            if self.memory.write(episode).await.is_ok() {
                self.emit(run_id, seq, RunEvent::ReflectionWritten { episode_id });
                seq += 1;
            }
        }

        match (&run.status, output, error) {
            (RunStatus::Completed, Some(out), _) => {
                self.emit(run_id, seq, RunEvent::Completed { output: out });
            }
            (_, _, err) => {
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Failed {
                        error: err.unwrap_or_else(|| "unknown".into()),
                    },
                );
            }
        }

        Ok(run)
    }
}

/// Test/ops hook for crash-injection smokes: when `KELTER_CRASH_POINT` is
/// `"<point>:<action>"` (e.g. `after_intent:append_ledger`), the process
/// aborts hard at that boundary — simulating a crash exactly inside the
/// durability windows. Never set in normal operation.
fn crash_point(point: &str, action: &str) {
    if let Ok(v) = std::env::var("KELTER_CRASH_POINT") {
        if v == format!("{point}:{action}") {
            tracing::error!("KELTER_CRASH_POINT hit ({v}) — aborting process (test hook)");
            std::process::exit(137);
        }
    }
}

/// The kernel-owned terminal action, appended to every action space.
pub fn finish_spec() -> ActionSpec {
    ActionSpec {
        name: FINISH_ACTION.into(),
        description: "Finish the task and report the final result. Use only after the task's \
                      output has actually been produced."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "One or two sentences describing what was done and where the output is."
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        }),
    }
}
