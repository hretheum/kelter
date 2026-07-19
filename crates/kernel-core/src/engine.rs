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

/// Durable context handed to an executing action source. Lets a source
/// journal critical mid-execution facts (e.g. a created remote task id) so
/// a crash-resume can reconnect instead of redoing the side effect, and
/// attribute usage to remote agents for per-agent accounting.
pub struct ExecutionContext {
    run_id: Uuid,
    step: u32,
    store: Arc<dyn RunStore>,
    notes: std::collections::HashMap<String, serde_json::Value>,
    agent_usage: std::sync::Mutex<Vec<(String, u64)>>,
}

impl ExecutionContext {
    /// Build a context for one step. The engine calls this with the step's
    /// pre-crash notes folded from the journal; tests may construct one
    /// directly (notes are journal metadata, not a trust boundary).
    pub fn new(
        run_id: Uuid,
        step: u32,
        store: Arc<dyn RunStore>,
        notes: std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            run_id,
            step,
            store,
            notes,
            agent_usage: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Durably journal a side-note for this step, immediately.
    pub async fn record_note(
        &self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), crate::store::StoreError> {
        self.store
            .append_journal(
                self.run_id,
                crate::journal::JournalEntry::Note {
                    step: self.step,
                    key: key.to_string(),
                    value,
                },
            )
            .await
    }

    /// A note journaled for this step before a crash (available on resume).
    pub fn note(&self, key: &str) -> Option<&serde_json::Value> {
        self.notes.get(key)
    }

    /// Attribute remote-agent work to this run's per-agent accounting.
    pub fn attribute_agent(&self, agent: &str, elapsed_ms: u64) {
        if let Ok(mut usage) = self.agent_usage.lock() {
            usage.push((agent.to_string(), elapsed_ms));
        }
    }
}

/// The tool substrate seam: something that can describe an action space and
/// execute validated actions (in practice: the MCP client crate and the
/// A2A delegation client).
#[async_trait]
pub trait ActionSource: Send + Sync {
    async fn action_schema(&self, latitude: Latitude) -> Result<ActionSchema, ExecutionError>;
    async fn execute(
        &self,
        action: &ValidatedAction,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError>;
}

/// Merge several action sources into one space; execution routes to the
/// source that declared the action.
pub struct CompositeActionSource {
    sources: Vec<Arc<dyn ActionSource>>,
}

impl CompositeActionSource {
    pub fn new(sources: Vec<Arc<dyn ActionSource>>) -> Self {
        Self { sources }
    }
}

#[async_trait]
impl ActionSource for CompositeActionSource {
    async fn action_schema(&self, latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        let mut actions = Vec::new();
        for source in &self.sources {
            let schema = source.action_schema(latitude).await?;
            for action in schema.actions {
                if actions.iter().any(|a: &ActionSpec| a.name == action.name) {
                    tracing::warn!(
                        "composite action source: duplicate action '{}' — keeping the first",
                        action.name
                    );
                    continue;
                }
                actions.push(action);
            }
        }
        Ok(ActionSchema { actions })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        for source in &self.sources {
            let schema = source.action_schema(Latitude::Wide).await?;
            if schema.actions.iter().any(|a| a.name == action.action()) {
                return source.execute(action, ctx).await;
            }
        }
        Err(ExecutionError::Tool(format!(
            "no source declares action '{}'",
            action.action()
        )))
    }
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
    /// Terminal: cancellation was requested (e.g. A2A `CancelTask`) and
    /// honoured before the run reached its own conclusion. A canceled run is
    /// finished — `unfinished_runs` excludes it, so a restart never resumes it.
    Canceled,
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
    /// Per-remote-agent accounting (delegation count + remote wall time).
    #[serde(default)]
    pub agent_usage: std::collections::HashMap<String, AgentUsage>,
    /// The extension trust posture in effect for this run: `(name, label)`
    /// per registered extension (ADR 0012). Makes "what ran unsandboxed"
    /// answerable via the API. Labels are honest — a subprocess is never
    /// labelled "sandbox".
    #[serde(default)]
    pub trust_posture: Vec<(String, String)>,
    /// Dollar cost of this run's model usage under the configured price table
    /// (Phase 8, item B): `Some(usd)` for a priced model, `None` (JSON
    /// `null`) for an unpriced one — never a fabricated number.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// Accounting for one remote agent within a run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AgentUsage {
    pub delegations: u64,
    pub remote_ms: u64,
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

/// How a run is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// The model picks one action at a time (Phases 1–4b).
    #[default]
    Step,
    /// The model proposes a validated DAG up front; the kernel executes it
    /// durably (ADR 0010).
    Plan,
}

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub max_steps: u32,
    pub latitude: Latitude,
    /// Gate for the self-improvement stub: when false, no reflection episode
    /// is ever written. No silent memory self-mutation.
    pub reflection_enabled: bool,
    pub mode: RunMode,
    /// The extension trust posture (ADR 0012): `(name, label)` per
    /// registered extension. Recorded on every run for provenance.
    pub trust_posture: Vec<(String, String)>,
    /// Per-model price table (Phase 8, item B) used to compute each run's
    /// dollar cost. Default empty → every run's `cost_usd` is `null` until
    /// prices are configured.
    pub pricing: crate::cost::PricingTable,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            max_steps: 12,
            latitude: Latitude::Standard,
            reflection_enabled: true,
            mode: RunMode::Step,
            trust_posture: Vec::new(),
            pricing: crate::cost::PricingTable::default(),
        }
    }
}

/// Actions named `delegate_to_*` are A2A delegations; everything else is a
/// plain tool. This is the naming convention the A2A source emits, and the
/// classifier plan validation uses to check node types.
pub fn is_a2a_action(name: &str) -> bool {
    name.starts_with("delegate_to_")
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
    /// Distinct, reversible learned-fact layer (Phase 10, ADR 0016). Optional:
    /// absent → no self-learning, retrieval is episodic memory only.
    learned: Option<Arc<dyn crate::learned::LearnedStore>>,
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
            learned: None,
            events,
            config,
        }
    }

    /// Attach a learned-fact store, enabling governed self-learning (Phase 10).
    /// Builder-style so `Kernel::new` stays unchanged for every other caller.
    pub fn with_learned(mut self, learned: Arc<dyn crate::learned::LearnedStore>) -> Self {
        self.learned = Some(learned);
        self
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RunEventEnvelope> {
        self.events.subscribe()
    }

    // ---- governed self-learning (Phase 10, ADR 0016) --------------------

    fn learned_store(&self) -> Result<&Arc<dyn crate::learned::LearnedStore>, KernelError> {
        self.learned.as_ref().ok_or_else(|| {
            KernelError::Execution(ExecutionError::Tool(
                "self-learning is not enabled (no learned store configured)".into(),
            ))
        })
    }

    /// Governed distillation: read completed runs and extract reusable facts
    /// into the learned layer, each provenance-tagged (`ai_extracted`,
    /// `human_validated: false`, source run ids). Runs already distilled are
    /// skipped, so repeated calls do not duplicate. This is a kernel operation
    /// — logged and durably recorded as the learned artifacts themselves.
    pub async fn distill(
        &self,
        limit: usize,
    ) -> Result<crate::learned::DistillReport, KernelError> {
        use crate::learned::{LearnedFact, LearnedProvenance};

        let learned = self.learned_store()?;
        // Don't re-distil a run we already learned from.
        let already: std::collections::HashSet<Uuid> = learned
            .list()
            .await?
            .into_iter()
            .flat_map(|f| f.provenance.source_run_ids)
            .collect();

        let batch_id = Uuid::new_v4();
        let now = now_secs();
        let mut source_run_ids = Vec::new();
        let mut created = 0usize;

        // Newest runs first, cap at `limit` new facts.
        let mut runs = self.store.list().await?;
        runs.reverse();
        for run in runs {
            if created >= limit {
                break;
            }
            if run.status != RunStatus::Completed || already.contains(&run.id) {
                continue;
            }
            let actions: Vec<String> = run
                .steps
                .iter()
                .filter(|s| s.ok && s.action != FINISH_ACTION)
                .map(|s| s.action.clone())
                .collect();
            if actions.is_empty() {
                continue; // nothing reusable to learn
            }
            let fact = format!(
                "For a task like \"{}\", a successful approach used these actions in order: {}.",
                run.task,
                actions.join(" → ")
            );
            learned
                .put(LearnedFact {
                    id: Uuid::new_v4(),
                    fact,
                    task_context: run.task.clone(),
                    provenance: LearnedProvenance {
                        source_run_ids: vec![run.id],
                        distilled_at: now,
                        model: run.policy.clone(),
                        level: crate::learned::LEARNED_LEVEL.to_string(),
                        human_validated: false,
                        batch_id,
                    },
                })
                .await?;
            source_run_ids.push(run.id);
            created += 1;
        }

        tracing::info!(
            %batch_id,
            facts_created = created,
            "distill: extracted reusable facts into the learned layer (ai_extracted, human_validated=false)"
        );
        Ok(crate::learned::DistillReport {
            batch_id,
            facts_created: created,
            source_run_ids,
        })
    }

    /// All learned facts, for inspection (what the system learned + why).
    pub async fn list_learned(&self) -> Result<Vec<crate::learned::LearnedFact>, KernelError> {
        Ok(self.learned_store()?.list().await?)
    }

    /// Completely forget one learned fact — the next run will not see it.
    pub async fn forget_learned(&self, id: Uuid) -> Result<bool, KernelError> {
        let removed = self.learned_store()?.forget(id).await?;
        tracing::info!(%id, removed, "forget: learned fact removed (no residue)");
        Ok(removed)
    }

    /// Completely forget a whole distillation batch.
    pub async fn forget_learned_batch(&self, batch_id: Uuid) -> Result<u64, KernelError> {
        let n = self.learned_store()?.forget_batch(batch_id).await?;
        tracing::info!(%batch_id, removed = n, "forget: learned batch removed (no residue)");
        Ok(n)
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
            agent_usage: std::collections::HashMap::new(),
            trust_posture: self.config.trust_posture.clone(),
            cost_usd: None,
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

    /// Request cancellation of a run (A2A `CancelTask`). Cooperative and
    /// durable: if the run is already terminal it is returned unchanged (a
    /// finished task cannot be canceled); otherwise a `Terminal { Canceled }`
    /// entry is journaled and the run record is set to `Canceled`, so the
    /// decision survives a restart (the resume path finalises from that
    /// terminal journal entry rather than re-executing).
    ///
    /// The caller is responsible for stopping any in-flight execution of this
    /// run first (e.g. aborting the spawned task); once execution is stopped,
    /// this write is the single authoritative terminal record.
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<Run, KernelError> {
        let mut run = self.store.get(run_id).await?;
        if matches!(
            run.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Canceled
        ) {
            return Ok(run);
        }
        self.store
            .append_journal(
                run_id,
                JournalEntry::Terminal {
                    status: RunStatus::Canceled,
                    output: None,
                    error: Some("canceled via A2A tasks/cancel".into()),
                },
            )
            .await?;
        run.status = RunStatus::Canceled;
        run.error = Some("canceled via A2A tasks/cancel".into());
        self.store.put(run.clone()).await?;
        tracing::info!(%run_id, "run canceled (A2A tasks/cancel)");
        Ok(run)
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
            run.cost_usd = self.config.pricing.cost(&run.policy, &run.usage);
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

        // Journal the extension trust posture once (ADR 0012): what ran, and
        // under which trust mode — durably attributable, not re-derived.
        if !self.config.trust_posture.is_empty()
            && !journal
                .iter()
                .any(|e| matches!(e, JournalEntry::TrustPosture { .. }))
        {
            for (name, label) in &self.config.trust_posture {
                if label == "subprocess-unsandboxed" {
                    tracing::warn!(
                        %run_id, extension = %name,
                        "trust: extension runs as an operator-trusted subprocess \
                         (unsandboxed — OS-level egress; NOT contained)"
                    );
                }
            }
            self.store
                .append_journal(
                    run_id,
                    JournalEntry::TrustPosture {
                        entries: self.config.trust_posture.clone(),
                    },
                )
                .await?;
        }

        if self.config.mode == RunMode::Plan {
            return self.execute_plan(run, journal).await;
        }

        let latitude = self.config.latitude;
        let mut seq: u64 = 1;

        let mut memory_context: Vec<String> = match self.memory.retrieve(&run.task, 3).await {
            Ok(hits) => {
                if !hits.is_empty() {
                    tracing::info!(
                        %run_id,
                        hits = hits.len(),
                        top_score = hits[0].score,
                        "memory: retrieved context from the registered provider"
                    );
                }
                hits.into_iter().map(|h| h.summary).collect()
            }
            Err(e) => {
                // A failing memory backend degrades context quality, never
                // run integrity — logged, not fatal.
                tracing::warn!(%run_id, "memory retrieve failed: {e}");
                Vec::new()
            }
        };

        // Governed self-learning (Phase 10, ADR 0016): distilled facts enter
        // ONLY here, as retrieved *context* — the same seam as episodic memory,
        // tagged `[learned]` (lower-trust). The model may be informed by them,
        // but every proposed action is still validated against the schema
        // below; a learned fact is never an unvalidated authority.
        if let Some(learned) = &self.learned {
            match learned.retrieve(&run.task, 3).await {
                Ok(facts) if !facts.is_empty() => {
                    tracing::info!(
                        %run_id,
                        learned = facts.len(),
                        "learned: injected distilled facts as context (validated like any context)"
                    );
                    memory_context
                        .extend(facts.iter().map(crate::learned::LearnedFact::as_context));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(%run_id, "learned retrieve failed: {e}"),
            }
        }

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

                let exec_ctx = ExecutionContext::new(
                    run_id,
                    step_index,
                    self.store.clone(),
                    folded.notes.clone(),
                );
                let tool_span = tracing::info_span!(
                    parent: &step_span,
                    "kelter.tool_call",
                    step = step_index,
                    tool = %validated.action()
                );
                let (result, ok) = match self
                    .tools
                    .execute(&exec_action, &exec_ctx)
                    .instrument(tool_span)
                    .await
                {
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

                // Per-agent accounting attributed during execution.
                if let Ok(attributions) = exec_ctx.agent_usage.into_inner() {
                    for (agent, elapsed_ms) in attributions {
                        let entry = run.agent_usage.entry(agent).or_default();
                        entry.delegations += 1;
                        entry.remote_ms += elapsed_ms;
                    }
                }

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

        self.finalize_run(run, status, output, error, seq).await
    }

    /// Journal the terminal state, persist the run, write the gated
    /// reflection episode, and emit the completed/failed event. Shared by
    /// step mode and plan mode.
    async fn finalize_run(
        &self,
        mut run: Run,
        status: RunStatus,
        output: Option<serde_json::Value>,
        error: Option<String>,
        mut seq: u64,
    ) -> Result<Run, KernelError> {
        let run_id = run.id;

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
        // Dollar cost from the final token usage (Phase 8, item B); `None`
        // for an unpriced model.
        run.cost_usd = self.config.pricing.cost(&run.policy, &run.usage);
        self.store.put(run.clone()).await?;

        // Gated, provenance-tagged reflection stub: write an episode.
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

    /// Plan mode (ADR 0010): the model proposes a DAG, the kernel validates
    /// it before running anything, then executes nodes durably.
    ///
    /// The plan is a **memoized decision** — journaled once via
    /// `PlanProposed`; on resume it is replayed and the model is NOT
    /// re-queried for it. Each node reuses the per-action durable machinery
    /// (`Intent → execute → Receipt`, idempotency keys, A2A reconnect notes)
    /// keyed by its node index, so completed nodes are never re-run.
    async fn execute_plan(
        &self,
        mut run: Run,
        journal: Vec<JournalEntry>,
    ) -> Result<Run, KernelError> {
        let run_id = run.id;
        let mut seq: u64 = 1;
        let latitude = self.config.latitude;

        // 1. The action space this plan is validated and executed against.
        let mut schema = self.tools.action_schema(latitude).await?;

        // 2. Obtain the plan: replay the journaled one, or ask the model
        //    ONCE (offering only `propose_plan`, schema-constrained).
        let journaled_plan = journal.iter().find_map(|e| match e {
            JournalEntry::PlanProposed { plan } => Some(plan.clone()),
            _ => None,
        });

        let plan = match journaled_plan {
            Some(plan) => {
                tracing::info!(
                    %run_id,
                    "resume: replaying journaled plan — model NOT queried"
                );
                plan
            }
            None => {
                let propose_schema = ActionSchema {
                    actions: vec![ActionSpec {
                        name: crate::plan::PROPOSE_PLAN_ACTION.into(),
                        description: "Propose a plan: a DAG of typed nodes (tool / a2a / review) \
                                      with dependencies. Keep it minimal and correct."
                            .into(),
                        parameters: crate::plan::plan_schema(),
                    }],
                };
                let context = PolicyContext {
                    task: run.task.clone(),
                    steps: Vec::new(),
                    memory: Vec::new(),
                    last_error: None,
                    latitude,
                    guidance: Some(format!(
                        "Available tool/a2a actions: [{}]. Rules: each node needs a unique `id` \
                         (choose short ids like n1, n2 — NOT the action name); a node's `deps` \
                         must be [] or list OTHER nodes' ids — a node must never depend on \
                         itself or on its own action; a `tool`/`a2a` node needs `action` + \
                         `arguments`; a `review` node needs no action. The simplest valid plan \
                         is one tool node with deps [].",
                        schema
                            .actions
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                };
                let proposal = self
                    .oracle
                    .propose(&context, &propose_schema)
                    .await
                    .map_err(|e| ExecutionError::Tool(format!("plan proposal failed: {e}")))?;
                if let Some(u) = proposal.usage {
                    run.usage.add(&u);
                }
                let validated = propose_schema.validate(proposal.action).map_err(|e| {
                    ExecutionError::Tool(format!("proposed plan is not schema-valid: {e}"))
                })?;
                let plan: crate::plan::Plan = serde_json::from_value(validated.arguments().clone())
                    .map_err(|e| ExecutionError::Tool(format!("malformed plan: {e}")))?;
                plan
            }
        };

        // 3. Validate the plan (structure/cycles/types/references) BEFORE
        //    any node executes. An invalid plan fails the run — no partial
        //    side effects (this gate runs before the PlanProposed journal).
        let validated_plan = match crate::plan::validate(plan.clone(), &schema, is_a2a_action) {
            Ok(v) => v,
            Err(e) => {
                let reason = format!("plan rejected: {e}");
                tracing::warn!(%run_id, "{reason}");
                return self
                    .finalize_run(run, RunStatus::Failed, None, Some(reason), seq)
                    .await;
            }
        };

        // Journal the accepted plan (once). On resume this is what gets
        // replayed instead of re-querying the model.
        if journal
            .iter()
            .all(|e| !matches!(e, JournalEntry::PlanProposed { .. }))
        {
            self.store
                .append_journal(run_id, JournalEntry::PlanProposed { plan })
                .await?;
        }
        self.emit(
            run_id,
            seq,
            RunEvent::Proposed {
                step: 0,
                action: crate::plan::PROPOSE_PLAN_ACTION.into(),
                arguments: serde_json::json!({ "nodes": validated_plan.len() }),
                rationale: Some("validated plan".into()),
            },
        );
        seq += 1;

        // Ensure the review action is available for review nodes.
        schema.actions.push(ActionSpec {
            name: crate::plan::REVIEW_ACTION.into(),
            description: "Approve or reject based on upstream node outputs.".into(),
            parameters: crate::plan::review_schema(),
        });

        // 4. Execute nodes in topological order, each durably, keyed by its
        //    node index so a resume skips completed nodes.
        let mut node_outputs: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        let mut failure: Option<String> = None;

        for (node_index, node) in validated_plan
            .nodes_in_order()
            .map(|(i, n)| (i, n.clone()))
            .collect::<Vec<_>>()
        {
            let step = node_index as u32;
            let folded = fold_step(&journal, step);
            let node_span = tracing::info_span!("kelter.plan_node", node = %node.id, step);

            // Completed already (resume)? Use the journaled receipt.
            if let Some((ok, result)) = folded.receipt.clone() {
                tracing::info!(
                    %run_id, node = %node.id,
                    "resume: node receipt found — skipping execution"
                );
                node_outputs.insert(node.id.clone(), result.clone());
                self.record_node_step(&mut run, step, &node, result, ok)
                    .await?;
                if !ok {
                    failure = Some(format!("node '{}' had failed before the crash", node.id));
                    break;
                }
                continue;
            }

            let deps_context: Vec<serde_json::Value> = node
                .deps
                .iter()
                .filter_map(|d| node_outputs.get(d).cloned())
                .collect();

            let (result, ok) = self
                .execute_plan_node(&mut run, step, &node, &deps_context, &folded, &schema)
                .instrument(node_span)
                .await?;

            node_outputs.insert(node.id.clone(), result.clone());
            self.emit(
                run_id,
                seq,
                RunEvent::Executed {
                    step,
                    action: node
                        .action
                        .clone()
                        .unwrap_or_else(|| format!("{:?}", node.node_type).to_lowercase()),
                    ok,
                    result,
                },
            );
            seq += 1;

            if !ok {
                // Fail-on-node-error: adaptive re-planning is deliberately
                // deferred (ADR 0010) — a node failure fails the plan
                // explicitly rather than silently guessing a new plan.
                failure = Some(format!("plan node '{}' failed", node.id));
                break;
            }
        }

        let (status, output, error) = match failure {
            None => (
                RunStatus::Completed,
                Some(serde_json::json!({
                    "summary": format!("plan completed: {} node(s)", validated_plan.len()),
                    "nodes": node_outputs,
                })),
                None,
            ),
            Some(err) => (RunStatus::Failed, None, Some(err)),
        };
        self.finalize_run(run, status, output, error, seq).await
    }

    /// Record a plan node as a run step (mirrors StepRecord for observability
    /// and reflection).
    async fn record_node_step(
        &self,
        run: &mut Run,
        step: u32,
        node: &crate::plan::PlanNode,
        result: serde_json::Value,
        ok: bool,
    ) -> Result<(), KernelError> {
        run.steps.push(StepRecord {
            index: step,
            action: node
                .action
                .clone()
                .unwrap_or_else(|| format!("{:?}", node.node_type).to_lowercase()),
            arguments: node.arguments.clone(),
            result,
            ok,
        });
        self.store.put(run.clone()).await?;
        Ok(())
    }

    /// Execute one plan node through the appropriate existing seam, inside
    /// the durable intent→receipt boundary keyed by the node's index.
    async fn execute_plan_node(
        &self,
        run: &mut Run,
        step: u32,
        node: &crate::plan::PlanNode,
        deps_context: &[serde_json::Value],
        folded: &crate::journal::StepJournal,
        schema: &ActionSchema,
    ) -> Result<(serde_json::Value, bool), KernelError> {
        let run_id = run.id;

        // Build the node's validated action. Every node — tool, a2a, review
        // — goes through kernel-side schema validation (governance is NOT
        // bypassed by planning).
        let (validated, action_label) = match node.node_type {
            crate::plan::NodeType::Review => {
                // A review node is a kernel-owned decision. It calls the
                // oracle (memoized via the receipt) for an approve/reject
                // verdict over its upstream outputs.
                let review_schema = ActionSchema {
                    actions: vec![schema
                        .spec(crate::plan::REVIEW_ACTION)
                        .expect("review action present")
                        .clone()],
                };
                let context = PolicyContext {
                    task: format!(
                        "Review the following upstream results and decide whether the work is \
                         acceptable:\n{}",
                        serde_json::to_string(deps_context).unwrap_or_default()
                    ),
                    steps: Vec::new(),
                    memory: Vec::new(),
                    last_error: None,
                    latitude: self.config.latitude,
                    guidance: Some(
                        "Call review_verdict with approve=true only if the results look correct."
                            .into(),
                    ),
                };
                let proposal = self
                    .oracle
                    .propose(&context, &review_schema)
                    .await
                    .map_err(|e| ExecutionError::Tool(format!("review failed: {e}")))?;
                if let Some(u) = proposal.usage {
                    run.usage.add(&u);
                }
                let validated = review_schema
                    .validate(proposal.action)
                    .map_err(|e| ExecutionError::Tool(format!("review verdict invalid: {e}")))?;
                let approved = validated.arguments()["approve"].as_bool().unwrap_or(false);
                // A review node has no external side effect: journal its
                // verdict directly as the receipt (memoized decision).
                let result = validated.arguments().clone();
                self.store
                    .append_journal(
                        run_id,
                        JournalEntry::Receipt {
                            step,
                            ok: approved,
                            result: result.clone(),
                        },
                    )
                    .await?;
                self.record_node_step(run, step, node, result.clone(), approved)
                    .await?;
                return Ok((result, approved));
            }
            crate::plan::NodeType::Tool | crate::plan::NodeType::A2a => {
                let action = node.action.clone().unwrap_or_default();
                let proposed = crate::action::ProposedAction {
                    action: action.clone(),
                    arguments: node.arguments.clone(),
                    rationale: Some(format!("plan node '{}'", node.id)),
                };
                let validated = schema.validate(proposed).map_err(|e| {
                    // Should not happen (plan validation checked existence),
                    // but arguments are validated here per-action.
                    ExecutionError::Tool(format!("node '{}' action invalid: {e}", node.id))
                })?;
                (validated, action)
            }
        };

        // Durable intent → execute → receipt (identical contract to step
        // mode; idempotency + A2A reconnect apply).
        let keyed = schema
            .spec(validated.action())
            .map(|s| s.supports_idempotency_key())
            .unwrap_or(false);

        let idempotency_key = match folded.intent.clone() {
            Some((_, _, journaled_key)) => {
                if !keyed {
                    let reason = format!(
                        "cannot safely resume plan node '{}': action '{}' was dispatched before \
                         the crash but wrote no receipt and declares no idempotency_key — manual \
                         check required.",
                        node.id, action_label
                    );
                    self.store
                        .append_journal(
                            run_id,
                            JournalEntry::Receipt {
                                step,
                                ok: false,
                                result: serde_json::json!({ "error": reason }),
                            },
                        )
                        .await?;
                    self.record_node_step(
                        run,
                        step,
                        node,
                        serde_json::json!({"error": reason}),
                        false,
                    )
                    .await?;
                    return Ok((
                        serde_json::json!({ "error": "manual check required" }),
                        false,
                    ));
                }
                tracing::info!(
                    %run_id, node = %node.id,
                    "resume: dangling node intent — re-dispatching with the journaled key"
                );
                journaled_key
            }
            None => {
                let key = format!("{run_id}:plan:{step}");
                self.store
                    .append_journal(
                        run_id,
                        JournalEntry::Intent {
                            step,
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
        let exec_ctx =
            ExecutionContext::new(run_id, step, self.store.clone(), folded.notes.clone());
        let (result, ok) = match self.tools.execute(&exec_action, &exec_ctx).await {
            Ok(v) => (v, true),
            Err(e) => (serde_json::json!({ "error": e.to_string() }), false),
        };

        crash_point("after_execute", validated.action());

        if let Ok(attributions) = exec_ctx.agent_usage.into_inner() {
            for (agent, elapsed_ms) in attributions {
                let entry = run.agent_usage.entry(agent).or_default();
                entry.delegations += 1;
                entry.remote_ms += elapsed_ms;
            }
        }

        self.store
            .append_journal(
                run_id,
                JournalEntry::Receipt {
                    step,
                    ok,
                    result: result.clone(),
                },
            )
            .await?;
        self.record_node_step(run, step, node, result.clone(), ok)
            .await?;
        Ok((result, ok))
    }
}

/// Test/ops hook for crash-injection smokes: when `KELTER_CRASH_POINT` is
/// `"<point>:<action>"` (e.g. `after_intent:append_ledger`), the process
/// aborts hard at that boundary — simulating a crash exactly inside the
/// durability windows. Never set in normal operation.
pub fn crash_point(point: &str, action: &str) {
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
