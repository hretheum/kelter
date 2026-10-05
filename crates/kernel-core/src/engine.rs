//! The deterministic kernel: a state machine that owns control flow, retries,
//! persistence and error handling. The model is consulted at explicit
//! decision points and never executes anything itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::Instrument;
use uuid::Uuid;

use crate::action::{ActionSchema, ActionSpec, ValidatedAction};
use crate::delegation_gate::{AgentDelegationDecision, DelegationDecisionOutcome, DelegationGate};
use crate::journal::{
    bounded_rejected_arguments, bounded_rejection_reason, execution_session_id, fold_step,
    JournalEntry, RejectionSource, RejectionSubject, ResumeCause, Usage,
};
use crate::oracle::{Latitude, PolicyContext, PolicyOracle};
use crate::outcome;
use crate::skill_gate::{ActivationAuthorization, SkillActivationDecision};
use crate::store::RunStore;
use memory::{Episode, EpisodeOutcome, MemoryProvider, Provenance};

/// Name of the terminal action the kernel itself adds to every action space.
pub const FINISH_ACTION: &str = "finish";

/// Kernel-owned action (ADR 0020): the model self-activates an advisory
/// skill's instructions for the rest of this run. Handled inline in the
/// engine loop exactly like `finish` — never routed through `ActionSource`,
/// so it grants zero capability. Offered only when a `SkillGate` is attached
/// and reports something activatable.
pub const ACTIVATE_SKILL_ACTION: &str = "activate_skill";
/// Kernel-owned counterpart: turn a run-scoped advisory skill back off.
/// Needs no gate — removing advisory context is always safe.
pub const DEACTIVATE_SKILL_ACTION: &str = "deactivate_skill";

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
    /// Live progress channel (ADR 0027), attached by the engine only for
    /// delegation dispatch sites. `None` for tests, the WASM host, and any
    /// non-delegation execution — a source that emits progress into a
    /// context without one still journals it durably (see
    /// [`Self::record_progress`]); only the live event is skipped.
    progress: Option<ProgressSink>,
    /// Progress-call ordinal (ADR 0027) AND per-step cap counter (audit
    /// finding 9-b hardening) for [`Self::record_progress`]. Lives on the
    /// context itself, not on [`ProgressSink`], so the cap applies whether or
    /// not a live sink is attached — a call path with no sink must not be
    /// able to re-flood the journal under a fixed note key indefinitely
    /// either.
    progress_ordinal: AtomicU64,
}

/// The live half of streamed delegation progress (ADR 0027): where an
/// [`ExecutionContext::record_progress`] call publishes its advisory
/// `AgentDelegationProgress` event.
#[derive(Clone)]
struct ProgressSink {
    events: broadcast::Sender<RunEventEnvelope>,
    run_id: Uuid,
    step: u32,
    branch: Option<String>,
}

/// Hard cap on advisory progress notes journaled/broadcast per step (audit
/// finding 9-b): every non-final SSE frame from a streaming delegation
/// (ADR 0027) calls `record_progress`, with no prior limit on how many times
/// per step. A fast or hostile streaming peer could flood journal rows for a
/// single step within the poll window — unlike step-mode's own journal
/// growth, which `max_steps` bounds, this had no bound at all. Generous
/// enough that no legitimate task's progress trail is ever truncated in
/// practice, while still bounding worst-case per-step journal growth. `pub`
/// so callers (and tests) can assert against it directly rather than
/// duplicating the number.
pub const MAX_PROGRESS_NOTES_PER_STEP: u64 = 200;

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
            progress: None,
            progress_ordinal: AtomicU64::new(0),
        }
    }

    /// Attach a live progress channel (ADR 0027) so [`Self::record_progress`]
    /// also publishes an advisory `AgentDelegationProgress` event, not just a
    /// journal note. The engine calls this only for delegation dispatch; a
    /// context without it still journals progress durably.
    fn with_progress(
        mut self,
        events: broadcast::Sender<RunEventEnvelope>,
        branch: Option<String>,
    ) -> Self {
        self.progress = Some(ProgressSink {
            events,
            run_id: self.run_id,
            step: self.step,
            branch,
        });
        self
    }

    /// Record one intermediate progress observation from a streaming remote
    /// delegation (ADR 0027). ALWAYS journals a durable side-note (so the
    /// progress survives in the audit trail); ADDITIONALLY publishes a live
    /// `AgentDelegationProgress` event when a progress sink is attached.
    ///
    /// Advisory by construction: this never validates, authorizes, or alters
    /// the run's control flow — a hostile remote streaming garbage can only
    /// add notes/timeline lines, never influence what the kernel executes.
    /// Best-effort: a journal failure is logged and swallowed, never
    /// propagated, so a flaky store cannot fail an otherwise-fine delegation
    /// over an advisory note.
    pub async fn record_progress(&self, agent: &str, task_id: &str, state: &str, note: &str) {
        let ordinal = self.progress_ordinal.fetch_add(1, Ordering::Relaxed);
        if ordinal >= MAX_PROGRESS_NOTES_PER_STEP {
            if ordinal == MAX_PROGRESS_NOTES_PER_STEP {
                tracing::warn!(
                    %agent, %task_id, step = self.step, cap = MAX_PROGRESS_NOTES_PER_STEP,
                    "a2a: streamed progress exceeded the per-step cap — further updates for \
                     this step are dropped (neither journaled nor broadcast)"
                );
            }
            return;
        }
        if let Err(e) = self
            .record_note(
                &format!("a2a.progress.{ordinal}"),
                serde_json::json!({
                    "agent": agent,
                    "task_id": task_id,
                    "state": state,
                    "note": note,
                }),
            )
            .await
        {
            tracing::warn!(%agent, %task_id, "a2a: could not journal progress note: {e}");
        }
        if let Some(p) = &self.progress {
            // seq 0: progress is OFF the ordered stream (advisory, best-effort
            // live) — clients order it by `ordinal`, not `seq`.
            let _ = p.events.send(RunEventEnvelope {
                run_id: p.run_id,
                seq: 0,
                event: RunEvent::AgentDelegationProgress {
                    step: p.step,
                    agent: agent.to_string(),
                    task_id: task_id.to_string(),
                    state: state.to_string(),
                    note: note.to_string(),
                    ordinal,
                    branch: p.branch.clone(),
                },
            });
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

    /// The pinned origin `action` would dial, if it is an A2A delegation
    /// this source declares (ADR 0022) — `None` for a non-A2A source
    /// (the default) or an action this source does not recognize. Lets the
    /// kernel enforce depth/cycle governance without knowing anything
    /// about the A2A protocol itself.
    fn a2a_origin(&self, _action: &str) -> Option<String> {
        None
    }

    /// Daemon-level, cross-run circuit-breaker state for `action`'s target
    /// (ADR 0023) — `Closed` (the default) for a non-A2A source or a target
    /// with no recorded failures. This is deliberately NOT part of the
    /// journaled/governed `authorize_delegation` check: a circuit is
    /// observed operational health of a remote AGENT, not a run-scoped
    /// governance decision, so it lives here as a third small probe
    /// alongside `a2a_origin` rather than inside the kernel's own state.
    /// The engine consults this before every dial (fresh, retry, or
    /// fallback) and never dials a target whose circuit is open.
    fn circuit_state(&self, _action: &str) -> CircuitCheck {
        CircuitCheck::Closed
    }
}

/// The result of probing [`ActionSource::circuit_state`] for one action's
/// target (ADR 0023).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitCheck {
    /// No known reason not to dial.
    Closed,
    /// Too many consecutive failures recently — refuse without dialing.
    /// `retry_after` is how much cooldown remains.
    Open { retry_after: std::time::Duration },
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

    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.sources.iter().find_map(|s| s.a2a_origin(action))
    }

    fn circuit_state(&self, action: &str) -> CircuitCheck {
        self.sources
            .iter()
            .find_map(|s| match s.circuit_state(action) {
                CircuitCheck::Closed => None,
                open @ CircuitCheck::Open { .. } => Some(open),
            })
            .unwrap_or(CircuitCheck::Closed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("tool error: {0}")]
    Tool(String),
    #[error("tool substrate unavailable: {0}")]
    Substrate(String),
    /// A delegation dialed a remote agent that entered `input-required`: the
    /// remote needs more input before it can finish (ADR 0026, interactive
    /// multi-turn delegation). This is NOT a failure — the engine catches it
    /// and pauses the run durably (`AwaitingDelegationInput`) to collect the
    /// human's continuation, then re-dispatches the SAME delegation, which
    /// reconnects to the existing remote task and sends the continuation.
    #[error("delegation to '{agent}' needs input: {prompt}")]
    DelegationInputRequired {
        agent: String,
        task_id: String,
        prompt: String,
    },
}

/// Journal-note key under which a human's continuation input for a paused
/// interactive delegation is recorded (ADR 0026). Defined here (not in the
/// a2a crate) because the kernel's `provide_delegation_input` writes it and
/// the a2a `ActionSource` reads it back on resume — one shared key, no magic
/// string drift.
pub const A2A_CONTINUATION_INPUT_NOTE: &str = "a2a.continuation_input";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    /// Terminal (ADR 0033): the model proposed `finish` (or a plan ran to
    /// the end) but the run executed no *effective* action — nothing the
    /// kernel dispatched to a tool/agent came back ok. The kernel withholds
    /// success: the model's summary is kept as `output`, unverified, and
    /// `error` says why. Like every terminal status, `unfinished_runs`
    /// excludes it and retention may prune it.
    Incomplete,
    /// Terminal: cancellation was requested (e.g. A2A `CancelTask`) and
    /// honoured before the run reached its own conclusion. A canceled run is
    /// finished — `unfinished_runs` excludes it, so a restart never resumes it.
    Canceled,
    /// Non-terminal, PAUSED (ADR 0021): the model self-activated an untrusted
    /// skill under an `Ask` policy and the run is waiting for a human's
    /// deny/allow-once/allow-always decision. `unfinished_runs` INCLUDES this
    /// status — a restart must re-surface, never orphan, a paused run.
    AwaitingSkillDecision,
    /// Non-terminal, PAUSED (ADR 0023): a `delegate_to_*` proposal was gated
    /// `AskHuman` by the `DelegationGate` under an `Ask` policy, and the run
    /// is waiting for a human's deny/allow-once/allow-always decision on
    /// whether to authorize dialing that agent. `unfinished_runs` INCLUDES
    /// this status for the same reason as `AwaitingSkillDecision` — a
    /// restart must re-surface, never orphan, a paused delegation.
    AwaitingDelegationDecision,
    /// Non-terminal, PAUSED (ADR 0026): an AUTHORIZED delegation dialed a
    /// remote agent that entered `input-required` — the remote needs more
    /// input to continue. The run waits for a human's continuation text, then
    /// re-dispatches the same delegation (reconnecting to the existing remote
    /// task). Distinct from `AwaitingDelegationDecision`: authorization
    /// already happened (P2+P3 passed, the dial was made); this is the remote
    /// asking a follow-up. `unfinished_runs` INCLUDES it — a restart
    /// re-surfaces the input request, never orphans it.
    AwaitingDelegationInput,
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
    /// Advisory skills the model self-activated this run (ADR 0020) —
    /// run-scoped and ephemeral: it is never written to the operator's
    /// persistent skill registry, and a fresh run always starts empty.
    #[serde(default)]
    pub active_skills: Vec<String>,
    /// The pending `activate_skill` ask-request this run is paused on, when
    /// `status == AwaitingSkillDecision` (ADR 0021). `None` otherwise.
    #[serde(default)]
    pub pending_skill_activation: Option<PendingSkillActivation>,
    /// The pending `delegate_to_*` ask-request this run is paused on, when
    /// `status == AwaitingDelegationDecision` (ADR 0023). `None` otherwise.
    #[serde(default)]
    pub pending_delegation: Option<PendingDelegation>,
    /// The pending interactive-delegation input request this run is paused on,
    /// when `status == AwaitingDelegationInput` (ADR 0026). `None` otherwise.
    #[serde(default)]
    pub pending_delegation_input: Option<PendingDelegationInput>,
    /// Per-run step-budget override (B4): `Some(n)` bounds THIS run at `n`
    /// steps instead of the daemon-wide `KernelConfig::max_steps`, so one
    /// daemon serves both short and long tasks. Fixed at run creation;
    /// `None` (every pre-existing run) = the global default.
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Chain depth reached by THIS run via inbound A2A delegation (ADR
    /// 0022): 0 for a run started directly (CLI/API/local), N for a run
    /// that arrived as the Nth hop of an A2A delegation chain. Fixed at run
    /// creation, never mutated afterward — any delegation THIS run attempts
    /// computes its own depth as `a2a_depth + 1`.
    #[serde(default)]
    pub a2a_depth: u32,
    /// Origins (`scheme://host:port`, NOT display names — ADR 0022) of
    /// every Kelter instance in this run's inbound delegation chain, oldest
    /// first. Fixed at run creation. A fresh delegation this run attempts
    /// is refused as a cycle if its target origin already appears here.
    #[serde(default)]
    pub a2a_ancestry: Vec<String>,
}

/// A durable snapshot of one pending human skill-activation decision (ADR
/// 0021), carried on the run record so the resume path and any client can
/// re-render the prompt without re-reading the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSkillActivation {
    pub step: u32,
    pub skill: String,
    pub trust: String,
    pub description: String,
    pub provenance_source_kind: String,
    pub provenance_source: String,
    /// Unix seconds when the request was made — the timeout sweeper compares
    /// against this, no journal read needed.
    pub requested_at: u64,
}

/// A durable snapshot of one pending human delegation-authorization decision
/// (ADR 0023), carried on the run record so the resume path and any client
/// can re-render the prompt without re-reading the journal. Mirrors
/// [`PendingSkillActivation`] field-for-field (`agent` instead of `skill`,
/// plus `target_origin`, which a delegation has and a skill activation does
/// not).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDelegation {
    pub step: u32,
    /// The `delegate_to_*` ACTION NAME, not a human-readable agent name —
    /// kernel-core knows agents only by the action they surface (mirrors how
    /// `PendingSkillActivation::skill` is the skill's own identifying name).
    /// Any display-name mapping lives in the a2a crate.
    pub agent: String,
    pub target_origin: String,
    pub trust: String,
    pub description: String,
    pub provenance_source_kind: String,
    pub provenance_source: String,
    pub requested_at: u64,
}

/// A durable snapshot of one pending interactive-delegation input request (ADR
/// 0026): an authorized delegation's remote task entered `input-required` and
/// the run is paused collecting the human's continuation. Carried on the run
/// record (and journaled) so a restart re-renders the prompt without re-reading
/// the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDelegationInput {
    pub step: u32,
    /// The `delegate_to_*` action whose remote task is asking for input.
    pub agent: String,
    /// The remote A2A task id awaiting continuation — reconnected to (never
    /// re-created) once the human responds.
    pub task_id: String,
    /// The remote's follow-up question / what it needs, surfaced to the human.
    pub prompt: String,
    pub requested_at: u64,
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
        /// The plan node id this event belongs to (ADR 0022), so a client
        /// can bucket a fan-out's events into lanes. `None` for ordinary
        /// step-mode events and the plan-level proposal event.
        branch: Option<String>,
    },
    Rejected {
        step: u32,
        reason: String,
        attempt: u32,
        /// See [`RunEvent::Proposed::branch`].
        branch: Option<String>,
        /// The `seq` of the `Proposed` event this rejection refers to, within
        /// the same execution's event stream (ADR 0036, additive): the
        /// refused proposal itself, the step's accepted proposal for a
        /// refused delegation retry/fallback, or the plan-level proposal for a
        /// refused plan node. `None` when no `Proposed` event exists for it
        /// (a failed model call, or a revised plan rejected before it was
        /// announced). Replaces pairing by `seq + 1`.
        proposal_seq: Option<u64>,
    },
    Executed {
        step: u32,
        action: String,
        ok: bool,
        result: serde_json::Value,
        /// See [`RunEvent::Proposed::branch`].
        branch: Option<String>,
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
    /// Terminal (ADR 0033): the run finished without a single effective
    /// action, so the kernel did not grant success. `output` is the model's
    /// own (unverified) summary, `reason` the kernel's verdict.
    Incomplete {
        output: Option<serde_json::Value>,
        reason: String,
    },
    /// A fresh `activate_skill` proposal was gated `AskHuman` (ADR 0021) —
    /// the run has paused. Carries everything a client needs to render the
    /// prompt without a follow-up read.
    SkillActivationRequested {
        step: u32,
        skill: String,
        description: String,
        trust: String,
        provenance_source_kind: String,
        provenance_source: String,
    },
    /// A pending activation request was resolved. Emitted by
    /// `decide_skill_activation` itself (not by the resumed execution), so a
    /// listening client sees the decision land instantly regardless of how
    /// long the resumed run's next model call takes.
    SkillActivationDecided {
        step: u32,
        skill: String,
        decision: SkillActivationDecision,
        principal: String,
    },
    /// A fresh `delegate_to_*` proposal was gated `AskHuman` (ADR 0023) — the
    /// run has paused. Carries everything a client needs to render the
    /// authorization prompt without a follow-up read. Field-for-field mirror
    /// of `SkillActivationRequested` (`agent` instead of `skill`) plus
    /// `target_origin`. `branch` reuses the SAME plan-node dimension as
    /// `Proposed`/`Executed`/`Rejected` (ADR 0022) — always `None` today,
    /// since this pause only ever fires from step mode (plan mode does not
    /// implement the interactive pause this round, see ADR 0023); the field
    /// exists now so the wire shape does not need to change if that changes
    /// later.
    AgentDelegationRequested {
        step: u32,
        agent: String,
        target_origin: String,
        description: String,
        trust: String,
        provenance_source_kind: String,
        provenance_source: String,
        branch: Option<String>,
    },
    /// A pending delegation-authorization request was resolved. Emitted by
    /// `decide_delegation` itself (not by the resumed execution), so a
    /// listening client sees the decision land instantly.
    AgentDelegationDecided {
        step: u32,
        agent: String,
        decision: AgentDelegationDecision,
        principal: String,
        branch: Option<String>,
    },
    /// An authorized delegation's remote task entered `input-required`; the
    /// run is paused collecting the human's continuation (ADR 0026).
    AgentDelegationInputRequired {
        step: u32,
        agent: String,
        task_id: String,
        prompt: String,
        branch: Option<String>,
    },
    /// A human provided the continuation input; the run resumes and
    /// re-dispatches the delegation, reconnecting to the remote task (ADR 0026).
    AgentDelegationInputProvided {
        step: u32,
        agent: String,
        principal: String,
        branch: Option<String>,
    },
    /// Intermediate progress streamed from a running remote delegation (ADR
    /// 0027): a status transition or partial artifact the remote agent
    /// emitted over SSE (`SendStreamingMessage`) while still WORKING. Purely
    /// ADVISORY observation — it is journaled as a side-note and shown on the
    /// timeline, but it is NEVER a validated action and NEVER authorizes
    /// anything; the delegation's real result still flows through the same
    /// terminal-state handling and validation floor. `ordinal` is monotonic
    /// within one `(run, step)` so a client can order/dedupe progress lines
    /// independently of the ordered event `seq` (progress rides seq 0, off
    /// the ordered stream, since it is advisory and best-effort-live).
    AgentDelegationProgress {
        step: u32,
        agent: String,
        task_id: String,
        state: String,
        note: String,
        ordinal: u64,
        branch: Option<String>,
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
    /// Governed A2A delegation limits (ADR 0022).
    pub delegation: DelegationLimits,
    /// Governed resilience: retry + sanctioned fallback (ADR 0023).
    pub resilience: ResilienceConfig,
    /// Plan-mode resilience: bounded node retry + bounded adaptive
    /// re-planning (ADR 0030). Defaults to off (behavior-preserving).
    pub plan_resilience: PlanResilience,
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
            delegation: DelegationLimits::default(),
            resilience: ResilienceConfig::default(),
            plan_resilience: PlanResilience::default(),
        }
    }
}

/// Governed resilience for A2A delegation (ADR 0023): bounded retry against
/// the same target, plus an operator-declared, sanctioned-only fallback list
/// per action — never auto-discovered. EVERY retry and fallback attempt
/// re-enters `Kernel::authorize_delegation` (P2: budget/depth/cycle) and,
/// when configured, the `DelegationGate` (P3: trust) exactly like a fresh
/// proposal — this is the one non-negotiable of this whole mechanism (see
/// ADR 0023). Defaults are reasonable-but-arbitrary, not derived from any
/// measurement — same honest framing as `DelegationLimits::default()`.
#[derive(Debug, Clone)]
pub struct ResilienceConfig {
    /// Additional dial attempts against the SAME target after the first one
    /// fails, before falling through to the sanctioned fallback list (if
    /// any). `0` disables same-target retry (fallback, if configured, still
    /// applies).
    pub max_retries_per_delegation: u32,
    /// Sanctioned fallback targets, keyed by the PRIMARY action's name
    /// (`delegate_to_*`), values also action names, in try-order. An action
    /// with no entry here has no fallback — exhausting retries against it
    /// (or finding its circuit open) ends the resilience loop with the last
    /// failure as the outcome.
    pub fallback: std::collections::HashMap<String, Vec<String>>,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            max_retries_per_delegation: 2,
            fallback: std::collections::HashMap::new(),
        }
    }
}

/// Governed limits on A2A delegation (ADR 0022): a per-run attempt budget,
/// a chain-depth cap, and (via `Run::a2a_ancestry`) cycle detection. The
/// kernel enforces these the same way it enforces the action schema — as a
/// hard, journaled/emitted refusal, never a silent skip and never a
/// runaway. Defaults are reasonable-but-arbitrary, not derived from any
/// measurement — see ADR 0022 for the honest framing and the sign-off note.
#[derive(Debug, Clone, Copy)]
pub struct DelegationLimits {
    /// Max `delegate_to_*` attempts (admitted or refused) a single run may
    /// make.
    pub max_delegations_per_run: u32,
    /// Max A2A chain depth a run may extend to (a direct delegation reaches
    /// depth 1).
    pub max_depth: u32,
}

impl Default for DelegationLimits {
    fn default() -> Self {
        Self {
            max_delegations_per_run: 10,
            max_depth: 3,
        }
    }
}

/// Plan-mode resilience (ADR 0030): bounded per-node retry for transient
/// failures, and bounded adaptive re-planning — on a node failure the model
/// is asked for a REVISED plan (through the same `propose_plan` validation,
/// never a bypass) instead of failing the run outright. Both default to `0`,
/// which preserves the pre-ADR-0030 behavior exactly (a failed node fails
/// the plan); resilience is an operator opt-in, like everything governed.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanResilience {
    /// Additional execution attempts for a FAILED tool/a2a plan node before
    /// the failure counts (total attempts = 1 + this). Review nodes never
    /// retry — a rejection is a verdict, not a transient fault. Retries
    /// reuse the node's deterministic idempotency key, so a keyed tool can
    /// never double-execute its effect.
    pub max_node_retries: u32,
    /// Max re-plan PROPOSALS (valid or rejected) after the original plan.
    /// `0` disables adaptive re-planning. Accepted re-plans are journaled,
    /// so the budget holds across crash-resume.
    pub max_replans: u32,
}

/// Journal-step namespace per plan generation (ADR 0030): generation g's
/// node at plan-index i journals under step `g * STRIDE + i`. Strictly
/// larger than `plan::MAX_NODES` (32), so receipts/intents/notes from
/// different generations can never collide — and generation 0's keys equal
/// the plain node index, so every pre-ADR-0030 journal resumes unchanged.
pub const PLAN_GENERATION_STRIDE: u32 = 100;

/// Identity of a plan node's WORK across generations (ADR 0030): a revised
/// plan re-including a node with the same id, action and arguments refers to
/// the same side effect — its journaled result carries forward instead of
/// re-executing. `serde_json::Value::to_string` is deterministic here (maps
/// are ordered), so the key is stable across sessions.
fn plan_carry_key(node: &crate::plan::PlanNode) -> (String, String, String) {
    (
        node.id.clone(),
        node.action.clone().unwrap_or_default(),
        node.arguments.to_string(),
    )
}

/// Char-boundary-safe truncation for bounded model-facing context (ADR 0030:
/// the failure context handed to a re-plan proposal must not grow without
/// bound).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
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
    /// The requested operation does not apply to the run's current state
    /// (ADR 0021) — e.g. deciding a skill activation for a run that has no
    /// pending request, or one for a different skill than is pending.
    #[error("invalid state: {0}")]
    InvalidState(String),
}

pub struct Kernel {
    oracle: Arc<dyn PolicyOracle>,
    tools: Arc<dyn ActionSource>,
    memory: Arc<dyn MemoryProvider>,
    store: Arc<dyn RunStore>,
    /// Distinct, reversible learned-fact layer (Phase 10, ADR 0016). Optional:
    /// absent → no self-learning, retrieval is episodic memory only.
    learned: Option<Arc<dyn crate::learned::LearnedStore>>,
    /// The advisory-skill self-activation gate (Phase 12, ADR 0020).
    /// Optional: absent → `activate_skill`/`deactivate_skill` are never
    /// offered, so the model can never propose them. kernel-core stays
    /// skills-agnostic — this is only a seam.
    skills: Option<Arc<dyn crate::skill_gate::SkillGate>>,
    /// The agent-delegation authorization gate (ADR 0023). Optional: absent
    /// → every P2-admitted delegation is authorized silently (no trust
    /// gating) — kernel-core stays a2a-agnostic, this is only a seam.
    delegation_gate: Option<Arc<dyn DelegationGate>>,
    events: broadcast::Sender<RunEventEnvelope>,
    config: KernelConfig,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One refusal, recorded twice (ADR 0036): durably as an informational
/// `JournalEntry::Rejected` and live as `RunEvent::Rejected`.
struct Rejection {
    step: u32,
    attempt: u32,
    source: RejectionSource,
    subject: RejectionSubject,
    reason: String,
    /// The refused proposal's action (`Proposal`), the agent actually
    /// attempted (`Decision`), or the node's action (`PlanNode`).
    action: Option<String>,
    /// Only for `Proposal`: the refused proposal's arguments, journaled
    /// nowhere else.
    arguments: Option<serde_json::Value>,
    branch: Option<String>,
    proposal_seq: Option<u64>,
}

/// Why this execution resumes (ADR 0036): the last non-informational journal
/// entry decides. A resolution of a human pause there means the run was
/// waiting and was released; anything else means the previous execution
/// simply stopped.
fn resume_cause(journal: &[JournalEntry]) -> ResumeCause {
    let last = journal.iter().rev().find(|e| !e.is_informational());
    match last {
        Some(JournalEntry::SkillActivationDecided { .. })
        | Some(JournalEntry::AgentDelegationDecided { .. }) => ResumeCause::PauseResolved,
        Some(JournalEntry::Note { key, .. }) if key == A2A_CONTINUATION_INPUT_NOTE => {
            ResumeCause::PauseResolved
        }
        _ => ResumeCause::Interrupted,
    }
}

/// Whether an earlier execution of this run left durable state (ADR 0036).
/// Every execution writes a non-`Note` entry before (or together with) any
/// `Note` of its own — action-source notes follow an `Intent`, a
/// continuation note follows its `AgentDelegationInputRequested` — while the
/// only notes written BEFORE a first execution are the API's caller
/// attributions. So "any non-`Note` entry, or any recorded step" separates a
/// resume from a first execution without naming note keys. (The pre-ADR-0036
/// `!journal.is_empty()` test fired on every API-created run because of
/// that attribution note.)
fn has_prior_execution(run_steps: usize, journal: &[JournalEntry]) -> bool {
    run_steps > 0
        || journal
            .iter()
            .any(|e| !matches!(e, JournalEntry::Note { .. }))
}

/// The most recent `SkillActivationRequested` in the journal with no later
/// matching `SkillActivationDecided` (ADR 0021) — i.e. a pause the run has
/// not yet been resolved from. Scans the whole journal (like the Terminal
/// check) rather than a single step, since the pause can predate any resume.
fn still_awaiting_skill_decision(journal: &[JournalEntry]) -> Option<PendingSkillActivation> {
    let mut pending: Option<PendingSkillActivation> = None;
    for entry in journal {
        match entry {
            JournalEntry::SkillActivationRequested {
                step,
                skill,
                trust,
                description,
                provenance_source_kind,
                provenance_source,
                requested_at,
                ..
            } => {
                pending = Some(PendingSkillActivation {
                    step: *step,
                    skill: skill.clone(),
                    trust: trust.clone(),
                    description: description.clone(),
                    provenance_source_kind: provenance_source_kind.clone(),
                    provenance_source: provenance_source.clone(),
                    requested_at: *requested_at,
                });
            }
            JournalEntry::SkillActivationDecided { skill, .. }
                if pending.as_ref().is_some_and(|p| &p.skill == skill) =>
            {
                pending = None;
            }
            _ => {}
        }
    }
    pending
}

/// The most recent `AgentDelegationRequested` in the journal with no later
/// matching `AgentDelegationDecided` — i.e. a delegation-authorization pause
/// the run has not yet been resolved from. Mirrors
/// `still_awaiting_skill_decision` exactly.
fn still_awaiting_delegation_decision(journal: &[JournalEntry]) -> Option<PendingDelegation> {
    let mut pending: Option<PendingDelegation> = None;
    for entry in journal {
        match entry {
            JournalEntry::AgentDelegationRequested {
                step,
                agent,
                target_origin,
                trust,
                description,
                provenance_source_kind,
                provenance_source,
                requested_at,
                ..
            } => {
                pending = Some(PendingDelegation {
                    step: *step,
                    agent: agent.clone(),
                    target_origin: target_origin.clone(),
                    trust: trust.clone(),
                    description: description.clone(),
                    provenance_source_kind: provenance_source_kind.clone(),
                    provenance_source: provenance_source.clone(),
                    requested_at: *requested_at,
                });
            }
            JournalEntry::AgentDelegationDecided { agent, .. }
                if pending.as_ref().is_some_and(|p| &p.agent == agent) =>
            {
                pending = None;
            }
            _ => {}
        }
    }
    pending
}

/// The most recent `AgentDelegationInputRequested` in the journal that the
/// human has not yet answered — resolution is a `Note` keyed
/// `A2A_CONTINUATION_INPUT_NOTE` at the SAME step (ADR 0026). Mirrors
/// `still_awaiting_delegation_decision`, but the resolving entry is the
/// continuation Note rather than a dedicated "decided" entry.
fn still_awaiting_delegation_input(journal: &[JournalEntry]) -> Option<PendingDelegationInput> {
    let mut pending: Option<PendingDelegationInput> = None;
    for entry in journal {
        match entry {
            JournalEntry::AgentDelegationInputRequested {
                step,
                agent,
                task_id,
                prompt,
                requested_at,
            } => {
                pending = Some(PendingDelegationInput {
                    step: *step,
                    agent: agent.clone(),
                    task_id: task_id.clone(),
                    prompt: prompt.clone(),
                    requested_at: *requested_at,
                });
            }
            JournalEntry::Note { step, key, .. }
                if key == A2A_CONTINUATION_INPUT_NOTE
                    && pending.as_ref().is_some_and(|p| p.step == *step) =>
            {
                pending = None;
            }
            _ => {}
        }
    }
    pending
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
            skills: None,
            delegation_gate: None,
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

    /// Attach a skill-activation gate, enabling model self-activation of
    /// advisory skills (Phase 12, ADR 0020). Builder-style so `Kernel::new`
    /// stays unchanged for every other caller; absent → the feature is
    /// entirely inert (no new actions offered).
    pub fn with_skills(mut self, skills: Arc<dyn crate::skill_gate::SkillGate>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Attach an agent-delegation authorization gate (ADR 0023). Builder-
    /// style so `Kernel::new` stays unchanged for every other caller;
    /// absent → every P2-admitted delegation is authorized silently (no
    /// trust gating on top of budget/depth/cycle).
    pub fn with_delegation_gate(mut self, gate: Arc<dyn DelegationGate>) -> Self {
        self.delegation_gate = Some(gate);
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
        // Don't re-distil a run we already learned from — either artifact kind.
        let already: std::collections::HashSet<Uuid> = learned
            .list()
            .await?
            .into_iter()
            .flat_map(|f| f.provenance.source_run_ids)
            .chain(
                learned
                    .list_plans()
                    .await?
                    .into_iter()
                    .flat_map(|p| p.provenance.source_run_ids),
            )
            .collect();

        let batch_id = Uuid::new_v4();
        let now = now_secs();
        let mut source_run_ids = Vec::new();
        let mut facts_created = 0usize;
        let mut plans_created = 0usize;

        // Newest runs first, cap at `limit` new artifacts.
        let mut runs = self.store.list().await?;
        runs.reverse();
        for run in runs {
            if facts_created + plans_created >= limit {
                break;
            }
            if run.status != RunStatus::Completed || already.contains(&run.id) {
                continue;
            }
            let provenance = LearnedProvenance {
                source_run_ids: vec![run.id],
                distilled_at: now,
                model: run.policy.clone(),
                level: crate::learned::LEARNED_LEVEL.to_string(),
                human_validated: false,
                batch_id,
            };

            // A plan-mode run distils to its PLAN, not to prose — structure a
            // later run can re-validate rather than only read. The LAST
            // journaled generation is the source: it is the one that carried
            // the run to completion, while earlier ones failed and triggered a
            // re-plan (ADR 0030), so learning from those would be learning
            // failure. One run yields one artifact, never both.
            let final_plan = self
                .store
                .journal(run.id)
                .await?
                .into_iter()
                .filter_map(|e| match e {
                    JournalEntry::PlanProposed { plan } => Some(plan),
                    _ => None,
                })
                .next_back();

            if let Some(plan) = final_plan {
                learned
                    .put_plan(crate::learned::LearnedPlan {
                        id: Uuid::new_v4(),
                        task_context: run.task.clone(),
                        plan,
                        provenance,
                    })
                    .await?;
                source_run_ids.push(run.id);
                plans_created += 1;
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
                    provenance,
                })
                .await?;
            source_run_ids.push(run.id);
            facts_created += 1;
        }

        tracing::info!(
            %batch_id,
            facts_created,
            plans_created,
            "distill: extracted reusable facts and plans into the learned layer (ai_extracted, human_validated=false)"
        );
        Ok(crate::learned::DistillReport {
            batch_id,
            facts_created,
            plans_created,
            source_run_ids,
        })
    }

    /// All learned facts, for inspection (what the system learned + why).
    pub async fn list_learned(&self) -> Result<Vec<crate::learned::LearnedFact>, KernelError> {
        Ok(self.learned_store()?.list().await?)
    }

    /// All learned plans, same inspection contract as the facts above — and the
    /// only way to obtain a plan's id, which `forget_learned` needs.
    pub async fn list_learned_plans(
        &self,
    ) -> Result<Vec<crate::learned::LearnedPlan>, KernelError> {
        Ok(self.learned_store()?.list_plans().await?)
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
        if self.skills.as_ref().is_some_and(|g| g.any_activatable()) {
            schema.actions.push(activate_skill_spec());
            schema.actions.push(deactivate_skill_spec());
        }
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
        self.create_run_full(task, 0, Vec::new(), None).await
    }

    /// [`Self::create_run`] with a per-run step-budget override (B4).
    /// `None` = the daemon-wide `KernelConfig::max_steps`.
    pub async fn create_run_bounded(
        &self,
        task: String,
        max_steps: Option<u32>,
    ) -> Result<Uuid, KernelError> {
        self.create_run_full(task, 0, Vec::new(), max_steps).await
    }

    /// Create a pending run seeded with A2A delegation lineage (ADR 0022):
    /// used by the inbound A2A handler when an incoming `SendMessage`
    /// carries lineage metadata from an upstream Kelter hop. `a2a_depth`/
    /// `a2a_ancestry` default to `0`/`[]` (the start of a fresh chain) when
    /// no lineage metadata is present — [`Self::create_run`] is exactly
    /// that case. Otherwise identical to [`Self::create_run`].
    pub async fn create_run_with_lineage(
        &self,
        task: String,
        a2a_depth: u32,
        a2a_ancestry: Vec<String>,
    ) -> Result<Uuid, KernelError> {
        self.create_run_full(task, a2a_depth, a2a_ancestry, None)
            .await
    }

    async fn create_run_full(
        &self,
        task: String,
        a2a_depth: u32,
        a2a_ancestry: Vec<String>,
        max_steps: Option<u32>,
    ) -> Result<Uuid, KernelError> {
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
            active_skills: Vec::new(),
            pending_skill_activation: None,
            pending_delegation: None,
            pending_delegation_input: None,
            a2a_depth,
            a2a_ancestry,
            max_steps,
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
        self.cancel_run_with_reason(run_id, "canceled via A2A tasks/cancel")
            .await
    }

    /// Cancel with an honest, caller-supplied reason label (e.g. the web UI's
    /// Stop vs A2A `tasks/cancel`). Identical semantics to [`Self::cancel_run`]
    /// — a terminal run is returned unchanged; otherwise the durable canceled
    /// state is written. Only the recorded label differs.
    pub async fn cancel_run_with_reason(
        &self,
        run_id: Uuid,
        reason: &str,
    ) -> Result<Run, KernelError> {
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
                    error: Some(reason.to_string()),
                },
            )
            .await?;
        run.status = RunStatus::Canceled;
        run.error = Some(reason.to_string());
        self.store.put(run.clone()).await?;
        tracing::info!(%run_id, %reason, "run canceled");
        Ok(run)
    }

    /// Resolve a pending `activate_skill` ask-request (ADR 0021) with a
    /// human's decision. Journal + state write only — deliberately does NOT
    /// call `execute_run` itself (keeps this fast/pure); the caller (the API
    /// handler, or the timeout sweeper) is responsible for re-invoking
    /// `execute_run` to actually resume, mirroring how `create_run` (returns
    /// a run_id) is always paired with a separate `execute_run` spawn.
    pub async fn decide_skill_activation(
        &self,
        run_id: Uuid,
        skill: &str,
        decision: SkillActivationDecision,
        principal: &str,
    ) -> Result<Run, KernelError> {
        let mut run = self.store.get(run_id).await?;
        let pending = run
            .pending_skill_activation
            .as_ref()
            .filter(|p| p.skill == skill)
            .ok_or_else(|| {
                KernelError::InvalidState(format!(
                    "run {run_id} has no pending activation request for '{skill}'"
                ))
            })?;
        let step = pending.step;
        self.store
            .append_journal(
                run_id,
                JournalEntry::SkillActivationDecided {
                    step,
                    skill: skill.to_string(),
                    decision,
                    principal: principal.to_string(),
                },
            )
            .await?;
        run.pending_skill_activation = None;
        run.status = RunStatus::Running;
        self.store.put(run.clone()).await?;
        self.emit(
            run_id,
            0,
            RunEvent::SkillActivationDecided {
                step,
                skill: skill.to_string(),
                decision,
                principal: principal.to_string(),
            },
        );
        tracing::info!(%run_id, %skill, ?decision, %principal, "skill activation decided");
        Ok(run)
    }

    /// Resolve a pending `delegate_to_*` ask-request (ADR 0023) with a
    /// human's decision. Journal + state write only — mirrors
    /// `decide_skill_activation` exactly, including NOT calling
    /// `execute_run` itself; the caller (the API handler, or the timeout
    /// sweeper) is responsible for re-invoking `execute_run` to resume.
    pub async fn decide_delegation(
        &self,
        run_id: Uuid,
        agent: &str,
        decision: AgentDelegationDecision,
        principal: &str,
    ) -> Result<Run, KernelError> {
        let mut run = self.store.get(run_id).await?;
        let pending = run
            .pending_delegation
            .as_ref()
            .filter(|p| p.agent == agent)
            .ok_or_else(|| {
                KernelError::InvalidState(format!(
                    "run {run_id} has no pending delegation request for '{agent}'"
                ))
            })?;
        let step = pending.step;
        self.store
            .append_journal(
                run_id,
                JournalEntry::AgentDelegationDecided {
                    step,
                    agent: agent.to_string(),
                    decision,
                    principal: principal.to_string(),
                },
            )
            .await?;
        run.pending_delegation = None;
        run.status = RunStatus::Running;
        self.store.put(run.clone()).await?;
        self.emit(
            run_id,
            0,
            RunEvent::AgentDelegationDecided {
                step,
                agent: agent.to_string(),
                decision,
                principal: principal.to_string(),
                branch: None,
            },
        );
        tracing::info!(%run_id, %agent, ?decision, %principal, "agent delegation decided");
        Ok(run)
    }

    /// Provide a human's continuation input for a run paused on an interactive
    /// delegation (ADR 0026). Journals the input as a `Note` (the resolving
    /// entry `still_awaiting_delegation_input` looks for), clears the pause,
    /// and flips the run back to `Running`. The CALLER then calls
    /// `execute_run` to resume — the delegation is re-dispatched and
    /// reconnects to the remote task, sending this input as the continuation.
    pub async fn provide_delegation_input(
        &self,
        run_id: Uuid,
        input: &str,
        principal: &str,
    ) -> Result<Run, KernelError> {
        let mut run = self.store.get(run_id).await?;
        let pending = run.pending_delegation_input.as_ref().ok_or_else(|| {
            KernelError::InvalidState(format!(
                "run {run_id} has no pending delegation-input request"
            ))
        })?;
        let step = pending.step;
        let agent = pending.agent.clone();
        self.store
            .append_journal(
                run_id,
                JournalEntry::Note {
                    step,
                    key: A2A_CONTINUATION_INPUT_NOTE.to_string(),
                    value: serde_json::json!(input),
                },
            )
            .await?;
        run.pending_delegation_input = None;
        run.status = RunStatus::Running;
        self.store.put(run.clone()).await?;
        self.emit(
            run_id,
            0,
            RunEvent::AgentDelegationInputProvided {
                step,
                agent,
                principal: principal.to_string(),
                branch: None,
            },
        );
        tracing::info!(%run_id, %principal, "delegation input provided");
        Ok(run)
    }

    fn emit(&self, run_id: Uuid, seq: u64, event: RunEvent) {
        // A send error only means no subscriber is listening right now.
        let _ = self.events.send(RunEventEnvelope { run_id, seq, event });
    }

    /// Record a refusal (ADR 0036): journal it (best-effort — the entry is
    /// informational, so a store hiccup here is logged and never changes
    /// what the run does next), then emit the live event and advance `seq`.
    async fn reject(&self, run_id: Uuid, seq: &mut u64, r: Rejection) {
        let entry = JournalEntry::Rejected {
            step: r.step,
            attempt: r.attempt,
            source: r.source,
            subject: r.subject,
            reason: bounded_rejection_reason(&r.reason),
            action: r.action,
            arguments: r.arguments.as_ref().map(bounded_rejected_arguments),
            branch: r.branch.clone(),
            at_ms: now_ms(),
        };
        if let Err(e) = self.store.append_journal(run_id, entry).await {
            tracing::warn!(
                %run_id, step = r.step,
                "journal: could not record a rejection (informational; the run continues): {e}"
            );
        }
        self.emit(
            run_id,
            *seq,
            RunEvent::Rejected {
                step: r.step,
                reason: r.reason,
                attempt: r.attempt,
                branch: r.branch,
                proposal_seq: r.proposal_seq,
            },
        );
        *seq += 1;
    }

    /// Record that this execution resumes durable state (ADR 0036). Best-
    /// effort for the same reason as [`Self::reject`].
    async fn record_resumed(
        &self,
        run_id: Uuid,
        journal: &[JournalEntry],
        at_step: u32,
        replayed_steps: Vec<u32>,
    ) {
        let entry = JournalEntry::Resumed {
            at_step,
            session: execution_session_id().to_string(),
            cause: resume_cause(journal),
            replayed_steps,
            at_ms: now_ms(),
        };
        if let Err(e) = self.store.append_journal(run_id, entry).await {
            tracing::warn!(
                %run_id,
                "journal: could not record the resume (informational; the run continues): {e}"
            );
        }
    }

    /// Governed check before a run may attempt one more A2A delegation (ADR
    /// 0022): budget, chain depth, and cycle detection — the kernel's
    /// admission-control point, mirroring the ADR 0020/0021 skill-gate Deny
    /// pattern. `attempted` is the caller-computed count of delegation
    /// attempts so far THIS CALL (not re-derived from `run.steps` here):
    /// step mode has exactly one action in flight at a time, so counting
    /// straight from `run.steps` is correct there; the concurrent plan
    /// executor (ADR 0022 Part 2) admits several nodes in one pass before
    /// any of them completes and appends a `StepRecord`, so it threads an
    /// explicit running counter instead — recomputing from `run.steps`
    /// there would under-count in-flight siblings admitted earlier in the
    /// same pass and let more than the budget through.
    ///
    /// Depth and cycle checks need no such counter: `run.a2a_depth` and
    /// `run.a2a_ancestry` are fixed for the whole run (set once at
    /// creation), so they can never be under- or over-counted by
    /// concurrent admission — only the attempt budget can.
    fn authorize_delegation(
        &self,
        run: &Run,
        attempted: u32,
        target_origin: &str,
    ) -> Result<(), String> {
        if attempted >= self.config.delegation.max_delegations_per_run {
            return Err(format!(
                "delegation budget exhausted ({attempted}/{} this run)",
                self.config.delegation.max_delegations_per_run
            ));
        }
        let next_depth = run.a2a_depth + 1;
        if next_depth > self.config.delegation.max_depth {
            return Err(format!(
                "delegation depth limit exceeded (would reach {next_depth}, max {})",
                self.config.delegation.max_depth
            ));
        }
        if run.a2a_ancestry.iter().any(|o| o == target_origin) {
            return Err(format!(
                "delegation cycle detected: '{target_origin}' already appears in this run's \
                 A2A ancestry {:?}",
                run.a2a_ancestry
            ));
        }
        Ok(())
    }

    /// Governed resilience (ADR 0023, step-mode only — see the ADR for the
    /// disclosed scope decision on why plan-mode nodes do not retry/fall
    /// back in this round). Called only after the FIRST dial for
    /// `primary_action` already failed (or was skipped because its circuit
    /// was already open). Two phases:
    ///
    /// 1. Up to `resilience.max_retries_per_delegation` additional dials
    ///    against the SAME target — skipped entirely if the target's circuit
    ///    is open (retrying a known-broken agent is pointless; go straight
    ///    to fallback).
    /// 2. The sanctioned fallback list for `primary_action` (if any),
    ///    in config order — an unconfigured alternative is NEVER tried.
    ///
    /// EVERY attempt in both phases re-enters `authorize_delegation` (P2:
    /// budget/depth/cycle) and, when a `DelegationGate` is attached, its
    /// `authorize_delegation_to` (P3: trust) — exactly like a fresh
    /// proposal. This is the one non-negotiable of the whole mechanism (see
    /// ADR 0023's cited test). A P2 or P3 refusal ABORTS the loop entirely
    /// (it is a RUN-scoped or delegation-scoped refusal, not agent-scoped —
    /// trying yet another candidate would not fix it). A circuit already
    /// open on a SPECIFIC candidate only skips THAT candidate — an
    /// agent-scoped operational fact, not a governance refusal — this is the
    /// deliberate circuit-vs-budget loop-abort distinction ADR 0023 documents
    /// explicitly.
    ///
    /// Returns the final `(result, ok)` plus any additional per-agent
    /// attributions accumulated across attempts (the caller merges them into
    /// `run.agent_usage`).
    #[allow(clippy::too_many_arguments)]
    async fn run_delegation_resilience(
        &self,
        run: &Run,
        run_id: Uuid,
        step_index: u32,
        primary_action: &str,
        task_arguments: &serde_json::Value,
        schema: &ActionSchema,
        seq: &mut u64,
        first_failure: serde_json::Value,
        decision_seq: Option<u64>,
    ) -> Result<(serde_json::Value, bool, Vec<(String, u64)>), KernelError> {
        // Every refusal below refers to this step's accepted, journaled
        // `Decision` (ADR 0036) — never to an adjacent event.
        let refusal =
            |attempt: u32, source: RejectionSource, agent: &str, reason: String| Rejection {
                step: step_index,
                attempt,
                source,
                subject: RejectionSubject::Decision,
                reason,
                action: Some(agent.to_string()),
                arguments: None,
                branch: None,
                proposal_seq: decision_seq,
            };
        // Preserve the caller's own failure reason (e.g. "circuit open...")
        // as the outcome if NEITHER phase below ever dials anything — never
        // silently discard it behind a placeholder. This matters whenever
        // retries are disabled (`max_retries_per_delegation == 0`) and no
        // fallback is configured: both phases are then no-ops, and the
        // original failure must still be the one that gets journaled.
        let mut result = first_failure;
        let mut ok = false;
        let mut attributions = Vec::new();
        // The FIRST attempt already consumed one budget slot in the caller
        // (either via the P2 admission check before Decision was journaled,
        // or — for the circuit-open-skip case — it never dialed but still
        // counts as this run's Nth a2a step once recorded). Every retry/
        // fallback attempt here consumes one more, checked fresh each time.
        // A step count bounded by `max_steps` (config, a dozen by default) —
        // never remotely near u32::MAX.
        #[allow(clippy::cast_possible_truncation)]
        let mut attempted = run
            .steps
            .iter()
            .filter(|s| is_a2a_action(&s.action))
            .count() as u32
            + 1;
        let mut aborted = false;

        // ---- phase 1: retries against the primary target ------------------
        let primary_open = matches!(
            self.tools.circuit_state(primary_action),
            CircuitCheck::Open { .. }
        );
        if !primary_open {
            for attempt_n in 1..=self.config.resilience.max_retries_per_delegation {
                let Some(origin) = self.tools.a2a_origin(primary_action) else {
                    break;
                };
                if let Err(reason) = self.authorize_delegation(run, attempted, &origin) {
                    self.reject(
                        run_id,
                        seq,
                        refusal(
                            attempt_n,
                            RejectionSource::DelegationLimit,
                            primary_action,
                            format!("retry refused: {reason}"),
                        ),
                    )
                    .await;
                    aborted = true;
                    break;
                }
                attempted += 1;
                if let Some(gate_refusal) = self.delegation_gate_refusal(primary_action, &origin) {
                    self.reject(
                        run_id,
                        seq,
                        refusal(
                            attempt_n,
                            RejectionSource::DelegationGate,
                            primary_action,
                            gate_refusal,
                        ),
                    )
                    .await;
                    aborted = true;
                    break;
                }
                let (r, o, attrs) = self
                    .dispatch_delegation_attempt(
                        run_id,
                        step_index,
                        primary_action,
                        task_arguments,
                        &format!("retry{attempt_n}"),
                        schema,
                        run.a2a_depth,
                        &run.a2a_ancestry,
                        &origin,
                    )
                    .await;
                attributions.extend(attrs);
                result = r;
                ok = o;
                if ok {
                    break;
                }
                self.reject(
                    run_id,
                    seq,
                    refusal(
                        attempt_n,
                        RejectionSource::DelegationRetry,
                        primary_action,
                        format!(
                            "delegation attempt to '{primary_action}' failed: {} — retrying \
                             ({attempt_n}/{})",
                            result
                                .get("error")
                                .and_then(|e| e.as_str())
                                .unwrap_or("unknown error"),
                            self.config.resilience.max_retries_per_delegation
                        ),
                    ),
                )
                .await;
            }
        }

        // ---- phase 2: sanctioned fallback, in config order -----------------
        if !ok && !aborted {
            if let Some(fallback_list) = self.config.resilience.fallback.get(primary_action) {
                // `i` is bounded by the config-declared fallback list length
                // (a handful of entries) — the `(i + 1) as u32` casts below
                // never truncate in practice.
                #[allow(clippy::cast_possible_truncation)]
                for (i, candidate) in fallback_list.iter().enumerate() {
                    if matches!(
                        self.tools.circuit_state(candidate),
                        CircuitCheck::Open { .. }
                    ) {
                        self.reject(
                            run_id,
                            seq,
                            refusal(
                                (i + 1) as u32,
                                RejectionSource::CircuitOpen,
                                candidate,
                                format!(
                                    "sanctioned fallback '{candidate}' skipped — its circuit is \
                                     open"
                                ),
                            ),
                        )
                        .await;
                        continue;
                    }
                    let Some(origin) = self.tools.a2a_origin(candidate) else {
                        continue;
                    };
                    if let Err(reason) = self.authorize_delegation(run, attempted, &origin) {
                        self.reject(
                            run_id,
                            seq,
                            refusal(
                                (i + 1) as u32,
                                RejectionSource::DelegationLimit,
                                candidate,
                                format!("fallback refused: {reason}"),
                            ),
                        )
                        .await;
                        break;
                    }
                    attempted += 1;
                    if let Some(gate_refusal) = self.delegation_gate_refusal(candidate, &origin) {
                        self.reject(
                            run_id,
                            seq,
                            refusal(
                                (i + 1) as u32,
                                RejectionSource::DelegationGate,
                                candidate,
                                gate_refusal,
                            ),
                        )
                        .await;
                        break;
                    }
                    let (r, o, attrs) = self
                        .dispatch_delegation_attempt(
                            run_id,
                            step_index,
                            candidate,
                            task_arguments,
                            &format!("fallback{}", i + 1),
                            schema,
                            run.a2a_depth,
                            &run.a2a_ancestry,
                            &origin,
                        )
                        .await;
                    attributions.extend(attrs);
                    result = r;
                    ok = o;
                    if ok {
                        break;
                    }
                }
            }
        }

        Ok((result, ok, attributions))
    }

    /// A `DelegationGate` refusal (Deny or AskHuman) for one automated
    /// retry/fallback candidate, as a human-readable reason — or `None` if
    /// no gate is attached or it allows. An `AskHuman` outcome mid-loop is
    /// treated as a refusal (not a pause): a headless automated retry/
    /// fallback attempt has no durable story for pausing partway through an
    /// in-progress resilience loop — only the FIRST, model-proposed attempt
    /// can pause (see the call site in `execute_run_inner`).
    fn delegation_gate_refusal(&self, action: &str, target_origin: &str) -> Option<String> {
        let gate = self.delegation_gate.as_ref()?;
        match gate.authorize_delegation_to(action, target_origin) {
            DelegationDecisionOutcome::Allow(_) => None,
            DelegationDecisionOutcome::Deny(reason) => Some(reason),
            DelegationDecisionOutcome::AskHuman(meta) => Some(format!(
                "agent '{action}' requires human authorization (trust={}) — automatic retry/\
                 fallback does not pause for a decision",
                meta.trust
            )),
        }
    }

    /// One dial attempt (retry or fallback) against `candidate_action`,
    /// reusing the original delegation's `task` argument verbatim. Journals
    /// its own `Intent` (a fresh durability boundary per attempt, keyed by a
    /// distinct `attempt_tag` so a crash mid-retry resumes safely — see ADR
    /// 0023); the CALLER journals the single final `Receipt` once the whole
    /// resilience loop concludes. Builds its `ExecutionContext` with an
    /// EMPTY notes map (deliberately NOT `folded.notes`): a retry must dial
    /// fresh, never reconnect to a prior FAILED attempt's remote task.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_delegation_attempt(
        &self,
        run_id: Uuid,
        step_index: u32,
        candidate_action: &str,
        task_arguments: &serde_json::Value,
        attempt_tag: &str,
        schema: &ActionSchema,
        run_a2a_depth: u32,
        run_a2a_ancestry: &[String],
        target_origin: &str,
    ) -> (serde_json::Value, bool, Vec<(String, u64)>) {
        let proposed = crate::action::ProposedAction {
            action: candidate_action.to_string(),
            arguments: task_arguments.clone(),
            rationale: Some(format!("kernel-initiated delegation {attempt_tag}")),
        };
        let validated = match schema.validate(proposed) {
            Ok(v) => v,
            Err(e) => {
                return (
                    serde_json::json!({
                        "error": format!("resilience candidate '{candidate_action}' invalid: {e}")
                    }),
                    false,
                    Vec::new(),
                )
            }
        };
        let keyed = schema
            .spec(candidate_action)
            .map(|s| s.supports_idempotency_key())
            .unwrap_or(false);
        let idempotency_key = format!("{run_id}:{step_index}:{attempt_tag}");
        if let Err(e) = self
            .store
            .append_journal(
                run_id,
                JournalEntry::Intent {
                    step: step_index,
                    action: candidate_action.to_string(),
                    arguments: validated.arguments().clone(),
                    idempotency_key: idempotency_key.clone(),
                },
            )
            .await
        {
            return (
                serde_json::json!({ "error": format!("cannot journal retry intent: {e}") }),
                false,
                Vec::new(),
            );
        }
        let mut exec_action = validated.clone();
        if keyed {
            exec_action.inject_idempotency_key(&idempotency_key);
        }
        let mut ancestry = run_a2a_ancestry.to_vec();
        ancestry.push(target_origin.to_string());
        exec_action.inject_a2a_lineage(run_a2a_depth + 1, &ancestry);

        let exec_ctx = ExecutionContext::new(
            run_id,
            step_index,
            self.store.clone(),
            std::collections::HashMap::new(),
        )
        // Streamed progress (ADR 0027) for kernel-initiated retries/fallbacks
        // too — a retry dial streams just like the first one (step mode: no
        // plan branch).
        .with_progress(self.events.clone(), None);
        let outcome = self.tools.execute(&exec_action, &exec_ctx).await;
        let attributions = exec_ctx.agent_usage.into_inner().unwrap_or_default();
        // E3: mask secret-shaped runs BEFORE the result is recorded — a tool
        // that read a `.env`/`Bearer` value must not persist it into the
        // durable StepRecord/Receipt (SQLite) and every later prompt.
        match outcome {
            Ok(v) => (crate::redact::scrub_json(&v), true, attributions),
            Err(e) => (
                serde_json::json!({ "error": crate::redact::scrub_secrets(&e.to_string()) }),
                false,
                attributions,
            ),
        }
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

        // Still awaiting a human skill-activation decision (ADR 0021)? Re-
        // surface, do not advance. Handles BOTH "daemon restarted while
        // pending" (the startup sweep calls execute_run again) and "someone
        // calls execute_run again before deciding" (idempotent — no side
        // effects, no model call). Mirrors the Terminal check above: sits
        // before the Running flip so there is no pointless status flicker.
        if let Some(pending) = still_awaiting_skill_decision(&journal) {
            tracing::info!(
                %run_id,
                step = pending.step,
                skill = %pending.skill,
                "resume: still awaiting a human skill-activation decision — re-surfacing"
            );
            self.emit(
                run_id,
                0,
                RunEvent::SkillActivationRequested {
                    step: pending.step,
                    skill: pending.skill.clone(),
                    description: pending.description.clone(),
                    trust: pending.trust.clone(),
                    provenance_source_kind: pending.provenance_source_kind.clone(),
                    provenance_source: pending.provenance_source.clone(),
                },
            );
            run.status = RunStatus::AwaitingSkillDecision;
            run.pending_skill_activation = Some(pending);
            self.store.put(run.clone()).await?;
            return Ok(run);
        }

        // Still awaiting a human delegation-authorization decision (ADR
        // 0023)? Re-surface, do not advance. Mirrors the skill-activation
        // check immediately above exactly.
        if let Some(pending) = still_awaiting_delegation_decision(&journal) {
            tracing::info!(
                %run_id,
                step = pending.step,
                agent = %pending.agent,
                "resume: still awaiting a human delegation-authorization decision — re-surfacing"
            );
            self.emit(
                run_id,
                0,
                RunEvent::AgentDelegationRequested {
                    step: pending.step,
                    agent: pending.agent.clone(),
                    target_origin: pending.target_origin.clone(),
                    description: pending.description.clone(),
                    trust: pending.trust.clone(),
                    provenance_source_kind: pending.provenance_source_kind.clone(),
                    provenance_source: pending.provenance_source.clone(),
                    branch: None,
                },
            );
            run.status = RunStatus::AwaitingDelegationDecision;
            run.pending_delegation = Some(pending);
            self.store.put(run.clone()).await?;
            return Ok(run);
        }

        // Still awaiting a human continuation for an interactive delegation
        // (ADR 0026)? Re-surface, do not advance — mirrors the two checks
        // above. Only reached once the continuation Note is ABSENT (a resolved
        // input is not "still awaiting"), so a resumed run whose human already
        // answered falls through and re-dispatches the delegation.
        if let Some(pending) = still_awaiting_delegation_input(&journal) {
            tracing::info!(
                %run_id,
                step = pending.step,
                agent = %pending.agent,
                "resume: still awaiting a human delegation-input continuation — re-surfacing"
            );
            self.emit(
                run_id,
                0,
                RunEvent::AgentDelegationInputRequired {
                    step: pending.step,
                    agent: pending.agent.clone(),
                    task_id: pending.task_id.clone(),
                    prompt: pending.prompt.clone(),
                    branch: None,
                },
            );
            run.status = RunStatus::AwaitingDelegationInput;
            run.pending_delegation_input = Some(pending);
            self.store.put(run.clone()).await?;
            return Ok(run);
        }

        let resumed = has_prior_execution(run.steps.len(), &journal);
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

        // Make the resume visible in the run's own history (ADR 0036). The
        // entry is computed from the journal snapshot loaded above — the same
        // snapshot every replay decision below reads — and is appended AFTER
        // that snapshot was taken, so this execution cannot see it; a later
        // one ignores it (informational, see `fold_step`).
        if resumed {
            let (at_step, replayed_steps) = if self.config.mode == RunMode::Plan {
                // The active generation is the last journaled plan; it is
                // replayed, never re-queried.
                #[allow(clippy::cast_possible_truncation)]
                let generations = journal
                    .iter()
                    .filter(|e| matches!(e, JournalEntry::PlanProposed { .. }))
                    .count() as u32;
                match generations.checked_sub(1) {
                    Some(g) => (g * PLAN_GENERATION_STRIDE, vec![g * PLAN_GENERATION_STRIDE]),
                    None => (0, Vec::new()),
                }
            } else {
                // Only the first step after a resume can be replayed: earlier
                // ones are already recorded, and the snapshot holds nothing
                // later. Replayed = a journaled Decision, or a resolved Allow
                // on a human ask (whose decision is the model's journaled
                // original proposal).
                #[allow(clippy::cast_possible_truncation)]
                let at_step = run.steps.len() as u32;
                let f = fold_step(&journal, at_step);
                let allowed_skill = f.skill_activation.as_ref().is_some_and(|p| {
                    matches!(
                        p.decision,
                        Some(
                            SkillActivationDecision::AllowOnce
                                | SkillActivationDecision::AllowAlways
                        )
                    )
                });
                let allowed_delegation = f.delegation.as_ref().is_some_and(|p| {
                    matches!(
                        p.decision,
                        Some(
                            AgentDelegationDecision::AllowOnce
                                | AgentDelegationDecision::AllowAlways
                        )
                    )
                });
                let replayed = f.decision.is_some() || allowed_skill || allowed_delegation;
                (at_step, if replayed { vec![at_step] } else { Vec::new() })
            };
            self.record_resumed(run_id, &journal, at_step, replayed_steps)
                .await;
        }

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

        // Bounded by `max_steps` — the per-run override (B4) when the run
        // carries one, else the daemon-wide config; a dozen by default —
        // never remotely near u32::MAX.
        let step_budget = run.max_steps.unwrap_or(self.config.max_steps);
        #[allow(clippy::cast_possible_truncation)]
        let first_step = run.steps.len() as u32;
        for step_index in first_step..step_budget {
            // Journal state for this step (only ever non-empty for the first
            // iteration after a resume).
            let mut folded = fold_step(&journal, step_index);
            let step_span = tracing::info_span!("kelter.step", step = step_index);
            // `seq` of this step's accepted (or replayed) `Proposed` event —
            // what a refused delegation retry/fallback refers to (ADR 0036).
            let mut decision_seq: Option<u64> = None;

            // A decided pending skill-activation request (ADR 0021): only
            // DECIDED ones reach here (the top-of-function early check above
            // already returned on an undecided one). Deny seeds `last_error`
            // and falls into the existing fresh-proposal branch below (a
            // fresh model call is required — the model must be told and must
            // propose something else). Allow* synthesizes exactly what the
            // existing replay branch already expects, so the ~150-line
            // attempt loop is never duplicated.
            if let Some(pending) = folded.skill_activation.clone() {
                match pending.decision {
                    None => { /* defensive no-op: the top-of-fn check already handles this */ }
                    Some(SkillActivationDecision::Deny) => {
                        last_error = Some(format!(
                            "the operator declined to activate '{}' for this run — pick a \
                             different approach.",
                            pending.skill
                        ));
                    }
                    Some(SkillActivationDecision::AllowAlways) => {
                        if let Some(gate) = &self.skills {
                            if let Err(e) = gate.elevate_trust(&pending.skill) {
                                tracing::warn!(
                                    %run_id, skill = %pending.skill,
                                    "trust elevation failed (activation still applies for \
                                     this run): {e}"
                                );
                            }
                        }
                        self.store
                            .append_journal(
                                run_id,
                                JournalEntry::Decision {
                                    step: step_index,
                                    action: ACTIVATE_SKILL_ACTION.into(),
                                    arguments: pending.arguments.clone(),
                                    usage: None,
                                },
                            )
                            .await?;
                        folded.decision =
                            Some((ACTIVATE_SKILL_ACTION.into(), pending.arguments.clone()));
                    }
                    Some(SkillActivationDecision::AllowOnce) => {
                        self.store
                            .append_journal(
                                run_id,
                                JournalEntry::Decision {
                                    step: step_index,
                                    action: ACTIVATE_SKILL_ACTION.into(),
                                    arguments: pending.arguments.clone(),
                                    usage: None,
                                },
                            )
                            .await?;
                        folded.decision =
                            Some((ACTIVATE_SKILL_ACTION.into(), pending.arguments.clone()));
                    }
                }
            }

            // A decided pending delegation-authorization request (ADR 0023):
            // mirrors the skill-activation replay block immediately above,
            // field-for-field, for `delegate_to_*` instead of
            // `activate_skill`. Deny seeds `last_error`; Allow* synthesizes
            // a Decision for the SAME action (the agent this run originally
            // proposed), so the existing replay branch below dispatches it —
            // no new execution path.
            if let Some(pending) = folded.delegation.clone() {
                match pending.decision {
                    None => { /* defensive no-op: the top-of-fn check already handles this */ }
                    Some(AgentDelegationDecision::Deny) => {
                        last_error = Some(format!(
                            "the operator declined to authorize delegating to '{}' for this \
                             run — pick a different approach.",
                            pending.agent
                        ));
                    }
                    Some(AgentDelegationDecision::AllowAlways) => {
                        if let Some(gate) = &self.delegation_gate {
                            if let Err(e) = gate.elevate_trust(&pending.agent) {
                                tracing::warn!(
                                    %run_id, agent = %pending.agent,
                                    "trust elevation failed (delegation still applies for \
                                     this run): {e}"
                                );
                            }
                        }
                        self.store
                            .append_journal(
                                run_id,
                                JournalEntry::Decision {
                                    step: step_index,
                                    action: pending.agent.clone(),
                                    arguments: pending.arguments.clone(),
                                    usage: None,
                                },
                            )
                            .await?;
                        folded.decision = Some((pending.agent.clone(), pending.arguments.clone()));
                    }
                    Some(AgentDelegationDecision::AllowOnce) => {
                        self.store
                            .append_journal(
                                run_id,
                                JournalEntry::Decision {
                                    step: step_index,
                                    action: pending.agent.clone(),
                                    arguments: pending.arguments.clone(),
                                    usage: None,
                                },
                            )
                            .await?;
                        folded.decision = Some((pending.agent.clone(), pending.arguments.clone()));
                    }
                }
            }

            // 1. Build the action space: tools + the kernel-owned finish action.
            let mut schema = self.tools.action_schema(latitude).await?;
            // A2 (duplicate-rule refinement): a MUTATING action — one whose
            // spec declares `idempotency_key` support, the kernel's own
            // side-effect convention — starts a new STATE EPOCH. Repeats are
            // judged within the current epoch only: verification after a
            // change is legitimate; a repeat with no state change in between
            // is a stall. Classified from the FULL schema, BEFORE the
            // narrowing below removes anything. An unkeyed side-effect tool
            // is conservatively treated as non-mutating — its repeats stay
            // rejected, exactly the pre-A2 behavior.
            let mutating_actions: std::collections::HashSet<String> = schema
                .actions
                .iter()
                .filter(|a| a.supports_idempotency_key())
                .map(|a| a.name.clone())
                .collect();
            let epoch_start = run
                .steps
                .iter()
                .rposition(|s| s.ok && mutating_actions.contains(&s.action));
            // Constrained latitude = harder decomposition: drop tools that
            // already succeeded — but only IN THE CURRENT EPOCH (A2): a
            // mutation brings dropped tools back into the space, so an
            // edit→verify loop stays possible. The name-level window
            // EXCLUDES the mutation itself (or the mutating tool could
            // never run twice); its exact-argument repeat is still caught
            // by the duplicate rule below, whose window INCLUDES it.
            if latitude.narrow_action_space() {
                let window = match epoch_start {
                    Some(i) => &run.steps[i + 1..],
                    None => &run.steps[..],
                };
                let done: std::collections::HashSet<&str> = window
                    .iter()
                    .filter(|s| s.ok)
                    .map(|s| s.action.as_str())
                    .collect();
                schema.actions.retain(|a| !done.contains(a.name.as_str()));
            }
            schema.actions.push(finish_spec());
            // Kernel-owned self-activation (ADR 0020) — pushed AFTER the
            // narrow_action_space retain so it is never dropped by
            // constrained latitude (a model may want to (de)activate
            // repeatedly).
            if self.skills.as_ref().is_some_and(|g| g.any_activatable()) {
                schema.actions.push(activate_skill_spec());
                schema.actions.push(deactivate_skill_spec());
            }

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
                decision_seq = Some(seq);
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Proposed {
                        step: step_index,
                        action: action.clone(),
                        arguments: arguments.clone(),
                        rationale: Some("replayed from journal".into()),
                        branch: None,
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
                             steps and pick exactly one NEXT action. Avoid repeating a step that \
                             already succeeded unless a state-changing action ran in between \
                             (verifying after a change is fine; an identical repeat with no \
                             change is rejected). Finish only when the task's output has been \
                             produced."
                                .to_string()
                        }),
                        active_skills: run.active_skills.clone(),
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
                            self.reject(
                                run_id,
                                &mut seq,
                                Rejection {
                                    step: step_index,
                                    attempt,
                                    source: RejectionSource::PolicyError,
                                    subject: RejectionSubject::None,
                                    reason: reason.clone(),
                                    action: None,
                                    arguments: None,
                                    branch: None,
                                    proposal_seq: None,
                                },
                            )
                            .await;
                            last_error = Some(reason);
                            continue;
                        }
                    };
                    let usage = proposal.usage;
                    let proposed = proposal.action;
                    // Kept for the journal (ADR 0036): validation consumes
                    // the proposal, and a refused one is journaled nowhere
                    // else.
                    let proposal_seq = seq;
                    let refused = |source: RejectionSource,
                                   action: &str,
                                   args: &serde_json::Value,
                                   reason: String| Rejection {
                        step: step_index,
                        attempt,
                        source,
                        subject: RejectionSubject::Proposal,
                        reason,
                        action: Some(action.to_string()),
                        arguments: Some(args.clone()),
                        branch: None,
                        proposal_seq: Some(proposal_seq),
                    };
                    let proposed_action = proposed.action.clone();
                    let proposed_arguments = proposed.arguments.clone();

                    self.emit(
                        run_id,
                        seq,
                        RunEvent::Proposed {
                            step: step_index,
                            action: proposed.action.clone(),
                            arguments: proposed.arguments.clone(),
                            rationale: proposed.rationale.clone(),
                            branch: None,
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
                            // Deterministic anti-stall rule, A2-refined:
                            // outside Wide latitude an exact repeat is
                            // rejected ONLY within the current state epoch —
                            // from the last successful mutating step
                            // (INCLUSIVE: repeating the mutation itself with
                            // identical arguments is still a stall) to now.
                            // A repeat with a state change in between is
                            // legitimate verification and executes normally.
                            let duplicate_of = latitude
                                .reject_duplicates()
                                .then(|| {
                                    let window = match epoch_start {
                                        Some(i) => &run.steps[i..],
                                        None => &run.steps[..],
                                    };
                                    window.iter().find(|s| {
                                        s.action == v.action() && &s.arguments == v.arguments()
                                    })
                                })
                                .flatten();
                            if let Some(prior) = duplicate_of {
                                tracing::info!(
                                    %run_id,
                                    step = step_index,
                                    action = %v.action(),
                                    epoch_start = ?epoch_start,
                                    prior_step = prior.index,
                                    prior_ok = prior.ok,
                                    mutating = ?mutating_actions,
                                    "duplicate rejected (A2 epoch window)"
                                );
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
                                self.reject(
                                    run_id,
                                    &mut seq,
                                    refused(
                                        RejectionSource::Duplicate,
                                        v.action(),
                                        v.arguments(),
                                        reason.clone(),
                                    ),
                                )
                                .await;
                                last_error = Some(reason);
                                continue;
                            }

                            // Governed delegation limits (ADR 0022): authorize
                            // ONE fresh A2A proposal before it is journaled as
                            // a Decision — mirrors the skill-activation gate
                            // exactly (Deny → rejected with feedback, no
                            // journaled decision, the model retries). Does NOT
                            // run on the journal-replay path (replay is law:
                            // the decision was authorized when first made) —
                            // this whole `else` arm only runs for a fresh
                            // proposal, never for a replayed one.
                            if is_a2a_action(v.action()) {
                                let target_origin = self.tools.a2a_origin(v.action());
                                // Bounded by `max_steps` — never remotely
                                // near u32::MAX.
                                #[allow(clippy::cast_possible_truncation)]
                                let attempted = run
                                    .steps
                                    .iter()
                                    .filter(|s| is_a2a_action(&s.action))
                                    .count() as u32;
                                let refusal = match &target_origin {
                                    None => Some(format!(
                                        "internal error: no origin declared for a2a action \
                                         '{}' (plan/schema offered it but the action source \
                                         cannot resolve its origin) — refusing rather than \
                                         dialing an unverified target",
                                        v.action()
                                    )),
                                    Some(origin) => {
                                        self.authorize_delegation(&run, attempted, origin).err()
                                    }
                                };
                                if let Some(reason) = refusal {
                                    self.reject(
                                        run_id,
                                        &mut seq,
                                        refused(
                                            RejectionSource::DelegationLimit,
                                            v.action(),
                                            v.arguments(),
                                            reason.clone(),
                                        ),
                                    )
                                    .await;
                                    last_error = Some(reason);
                                    continue;
                                }

                                // Agent-trust gate (ADR 0023): runs SECOND,
                                // only once P2 (budget/depth/cycle) already
                                // allowed this delegation structurally — see
                                // the module docs on `DelegationGate` for the
                                // ordering rationale. `target_origin` is
                                // guaranteed `Some` here (the `None` arm
                                // above already refused and `continue`d).
                                let origin = target_origin.expect(
                                    "target_origin is Some: the None arm above already refused",
                                );
                                if let Some(gate) = &self.delegation_gate {
                                    match gate.authorize_delegation_to(v.action(), &origin) {
                                        DelegationDecisionOutcome::Allow(_) => { /* fall through */
                                        }
                                        DelegationDecisionOutcome::Deny(reason) => {
                                            self.reject(
                                                run_id,
                                                &mut seq,
                                                refused(
                                                    RejectionSource::DelegationGate,
                                                    v.action(),
                                                    v.arguments(),
                                                    reason.clone(),
                                                ),
                                            )
                                            .await;
                                            last_error = Some(reason);
                                            continue;
                                        }
                                        DelegationDecisionOutcome::AskHuman(meta) => {
                                            let requested_at = now_secs();
                                            self.store
                                                .append_journal(
                                                    run_id,
                                                    JournalEntry::AgentDelegationRequested {
                                                        step: step_index,
                                                        agent: v.action().to_string(),
                                                        target_origin: origin.clone(),
                                                        arguments: v.arguments().clone(),
                                                        trust: meta.trust.clone(),
                                                        description: meta.description.clone(),
                                                        provenance_source_kind: meta
                                                            .provenance_source_kind
                                                            .clone(),
                                                        provenance_source: meta
                                                            .provenance_source
                                                            .clone(),
                                                        requested_at,
                                                    },
                                                )
                                                .await?;
                                            run.status = RunStatus::AwaitingDelegationDecision;
                                            run.pending_delegation = Some(PendingDelegation {
                                                step: step_index,
                                                agent: v.action().to_string(),
                                                target_origin: origin.clone(),
                                                trust: meta.trust.clone(),
                                                description: meta.description.clone(),
                                                provenance_source_kind: meta
                                                    .provenance_source_kind
                                                    .clone(),
                                                provenance_source: meta.provenance_source.clone(),
                                                requested_at,
                                            });
                                            self.store.put(run.clone()).await?;
                                            self.emit(
                                                run_id,
                                                seq,
                                                RunEvent::AgentDelegationRequested {
                                                    step: step_index,
                                                    agent: v.action().to_string(),
                                                    target_origin: origin,
                                                    description: meta.description,
                                                    trust: meta.trust,
                                                    provenance_source_kind: meta
                                                        .provenance_source_kind,
                                                    provenance_source: meta.provenance_source,
                                                    branch: None,
                                                },
                                            );
                                            return Ok(run);
                                        }
                                    }
                                }
                            }

                            // Self-activation gate (ADR 0020): authorize ONE
                            // fresh model proposal before it is journaled as
                            // a Decision. Err → rejected with feedback like a
                            // duplicate — no journaled decision, no state
                            // change, the model retries. This does NOT run on
                            // the journal-replay path (replay is law: the
                            // decision was authorized when first made).
                            // Deactivate needs no gate.
                            if v.action() == ACTIVATE_SKILL_ACTION {
                                if let Some(gate) = &self.skills {
                                    let name = v
                                        .arguments()
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("");
                                    match gate.authorize_activation(name, &run.active_skills) {
                                        ActivationAuthorization::Allow(_) => { /* fall through */ }
                                        ActivationAuthorization::Deny(reason) => {
                                            self.reject(
                                                run_id,
                                                &mut seq,
                                                refused(
                                                    RejectionSource::SkillGate,
                                                    v.action(),
                                                    v.arguments(),
                                                    reason.clone(),
                                                ),
                                            )
                                            .await;
                                            last_error = Some(reason);
                                            continue;
                                        }
                                        ActivationAuthorization::AskHuman(meta) => {
                                            let requested_at = now_secs();
                                            self.store
                                                .append_journal(
                                                    run_id,
                                                    JournalEntry::SkillActivationRequested {
                                                        step: step_index,
                                                        skill: name.to_string(),
                                                        arguments: v.arguments().clone(),
                                                        trust: meta.trust.clone(),
                                                        description: meta.description.clone(),
                                                        provenance_source_kind: meta
                                                            .provenance_source_kind
                                                            .clone(),
                                                        provenance_source: meta
                                                            .provenance_source
                                                            .clone(),
                                                        requested_at,
                                                    },
                                                )
                                                .await?;
                                            run.status = RunStatus::AwaitingSkillDecision;
                                            run.pending_skill_activation =
                                                Some(PendingSkillActivation {
                                                    step: step_index,
                                                    skill: name.to_string(),
                                                    trust: meta.trust.clone(),
                                                    description: meta.description.clone(),
                                                    provenance_source_kind: meta
                                                        .provenance_source_kind
                                                        .clone(),
                                                    provenance_source: meta
                                                        .provenance_source
                                                        .clone(),
                                                    requested_at,
                                                });
                                            self.store.put(run.clone()).await?;
                                            self.emit(
                                                run_id,
                                                seq,
                                                RunEvent::SkillActivationRequested {
                                                    step: step_index,
                                                    skill: name.to_string(),
                                                    description: meta.description,
                                                    trust: meta.trust,
                                                    provenance_source_kind: meta
                                                        .provenance_source_kind,
                                                    provenance_source: meta.provenance_source,
                                                },
                                            );
                                            return Ok(run);
                                        }
                                    }
                                }
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
                            decision_seq = Some(proposal_seq);
                            chosen = Some(v);
                            break;
                        }
                        Err(e) => {
                            let reason = e.to_string();
                            self.reject(
                                run_id,
                                &mut seq,
                                refused(
                                    RejectionSource::Schema,
                                    &proposed_action,
                                    &proposed_arguments,
                                    reason.clone(),
                                ),
                            )
                            .await;
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
            // The model proposes `finish`; the kernel decides whether the
            // run earned success (ADR 0033): with no effective step behind
            // it, the claim is kept as output but the status is Incomplete.
            if validated.action() == FINISH_ACTION {
                terminal = Some(if outcome::effective_steps(&run.steps) > 0 {
                    (
                        RunStatus::Completed,
                        Some(validated.arguments().clone()),
                        None,
                    )
                } else {
                    (
                        RunStatus::Incomplete,
                        Some(validated.arguments().clone()),
                        Some(outcome::no_effect_reason(&run.steps)),
                    )
                });
                break;
            }

            // 3.5 Kernel-owned self-activation actions (ADR 0020): the ONLY
            // effect is mutating `run.active_skills` — never routed through
            // the tool substrate, so they grant zero capability, only
            // changing which advisory skill instructions the model sees on
            // the NEXT step. No Intent/Receipt: there is no external side
            // effect, and a set insert/remove is repeatable, so re-applying
            // it under Decision replay is exactly-once by construction.
            if matches!(
                validated.action(),
                ACTIVATE_SKILL_ACTION | DEACTIVATE_SKILL_ACTION
            ) {
                let name = validated
                    .arguments()
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = if validated.action() == ACTIVATE_SKILL_ACTION {
                    // Authorization already passed for a fresh proposal; on
                    // replay, re-derive the trust label (the gate call is
                    // pure). If the gate now errs (e.g. policy changed since
                    // the decision was journaled), apply anyway — replay is
                    // law — with an honest "unknown" label rather than
                    // fabricating one.
                    let trust = self
                        .skills
                        .as_ref()
                        .and_then(
                            |g| match g.authorize_activation(&name, &run.active_skills) {
                                ActivationAuthorization::Allow(trust) => Some(trust),
                                _ => None,
                            },
                        )
                        .unwrap_or_else(|| "unknown".to_string());
                    if !run.active_skills.iter().any(|n| n == &name) {
                        run.active_skills.push(name.clone());
                    }
                    serde_json::json!({
                        "activated": name,
                        "trust": trust,
                        "run_active": run.active_skills,
                    })
                } else {
                    run.active_skills.retain(|n| n != &name);
                    serde_json::json!({
                        "deactivated": name,
                        "run_active": run.active_skills,
                    })
                };

                let record = StepRecord {
                    index: step_index,
                    action: validated.action().to_string(),
                    arguments: validated.arguments().clone(),
                    result: result.clone(),
                    ok: true,
                };
                run.steps.push(record);
                self.store.put(run.clone()).await?;
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Executed {
                        step: step_index,
                        action: validated.action().to_string(),
                        ok: true,
                        result,
                        branch: None,
                    },
                );
                seq += 1;
                continue;
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
                            // Audit finding 3-a: this is a contract on the
                            // TOOL, not a kernel-verified guarantee — a
                            // keyed tool is expected to dedupe by this key,
                            // but the kernel cannot confirm every
                            // registered ActionSource actually honors it
                            // (the A2A source, for one, achieves its own
                            // exactly-once via a separate reconnect-not-
                            // redial mechanism and never reads this key).
                            "resume: dangling intent — re-dispatching with the journaled \
                             idempotency key (the tool is expected to dedupe by it)"
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
                // Lineage injection (ADR 0022) is recomputed fresh here,
                // unconditionally, for every A2A dispatch — a pure function
                // of `run.a2a_depth`/`run.a2a_ancestry` (fixed for the
                // run's whole lifetime) and the target's origin, so it is
                // correct whether this is a brand-new dispatch or a
                // dangling-intent reconnect after a crash (which took the
                // journal-replay path above and so never re-ran the
                // authorize_delegation gate). The gate above already
                // decided whether this attempt is ALLOWED; this only
                // computes the VALUES to embed in the outbound A2A
                // metadata, which the a2a crate only reads when a fresh
                // `SendMessage` actually goes out (a reconnect never
                // resends it — see `a2a::A2aActionSource::execute`).
                if is_a2a_action(validated.action()) {
                    if let Some(origin) = self.tools.a2a_origin(validated.action()) {
                        let mut ancestry = run.a2a_ancestry.clone();
                        ancestry.push(origin);
                        exec_action.inject_a2a_lineage(run.a2a_depth + 1, &ancestry);
                    }
                }

                let exec_ctx = ExecutionContext::new(
                    run_id,
                    step_index,
                    self.store.clone(),
                    folded.notes.clone(),
                )
                // Streamed delegation progress (ADR 0027): attach the live
                // channel so an A2A source can publish intermediate progress
                // as it streams. Harmless for a non-A2A action — those never
                // call `record_progress`. Step mode has no plan branch.
                .with_progress(self.events.clone(), None);
                let tool_span = tracing::info_span!(
                    parent: &step_span,
                    "kelter.tool_call",
                    step = step_index,
                    tool = %validated.action()
                );
                // Governed resilience (ADR 0023): a circuit already open on
                // this action's target is refused WITHOUT dialing — never
                // dial a known-broken agent, not even on the first attempt.
                // The resilience loop below treats this identically to a
                // failed first dial (and, since the primary's circuit is
                // open, skips straight to any sanctioned fallback).
                let primary_circuit = is_a2a_action(validated.action())
                    .then(|| self.tools.circuit_state(validated.action()));
                let (mut result, mut ok) = match primary_circuit {
                    Some(CircuitCheck::Open { retry_after }) => {
                        let reason = format!(
                            "agent circuit open for '{}' (retry after {}s) — refusing to dial",
                            validated.action(),
                            retry_after.as_secs()
                        );
                        tracing::warn!(%run_id, step = step_index, "{reason}");
                        (serde_json::json!({ "error": reason }), false)
                    }
                    _ => match self
                        .tools
                        .execute(&exec_action, &exec_ctx)
                        .instrument(tool_span)
                        .await
                    {
                        // E3: mask secret-shaped runs before the result
                        // becomes the durable StepRecord/Receipt and, via
                        // `run.steps`, every later prompt.
                        Ok(v) => (crate::redact::scrub_json(&v), true),
                        // Interactive delegation (ADR 0026): the remote task
                        // entered `input-required`. This is NOT a failure —
                        // pause the run durably to collect the human's
                        // continuation, then return. The delegation's Intent
                        // and remote task id are already journaled (the dial
                        // happened), so a later resume re-dispatches the SAME
                        // action, which reconnects to the remote task and
                        // sends the continuation. Never re-dials from scratch.
                        Err(ExecutionError::DelegationInputRequired {
                            agent,
                            task_id,
                            prompt,
                        }) => {
                            let requested_at = now_secs();
                            self.store
                                .append_journal(
                                    run_id,
                                    JournalEntry::AgentDelegationInputRequested {
                                        step: step_index,
                                        agent: agent.clone(),
                                        task_id: task_id.clone(),
                                        prompt: prompt.clone(),
                                        requested_at,
                                    },
                                )
                                .await?;
                            run.status = RunStatus::AwaitingDelegationInput;
                            run.pending_delegation_input = Some(PendingDelegationInput {
                                step: step_index,
                                agent: agent.clone(),
                                task_id: task_id.clone(),
                                prompt: prompt.clone(),
                                requested_at,
                            });
                            self.store.put(run.clone()).await?;
                            self.emit(
                                run_id,
                                seq,
                                RunEvent::AgentDelegationInputRequired {
                                    step: step_index,
                                    agent,
                                    task_id,
                                    prompt,
                                    branch: None,
                                },
                            );
                            return Ok(run);
                        }
                        Err(e) => {
                            // E3: error text can carry a secret too (an HTTP
                            // body echoing an auth header) — scrub both the
                            // recorded result and the prompt-facing error.
                            let msg = crate::redact::scrub_secrets(&e.to_string());
                            last_error = Some(format!(
                                "tool '{}' failed: {msg}. Adjust and continue.",
                                validated.action()
                            ));
                            (serde_json::json!({ "error": msg }), false)
                        }
                    },
                };

                crash_point("after_execute", validated.action());

                // Per-agent accounting attributed during the FIRST attempt.
                if let Ok(attributions) = exec_ctx.agent_usage.into_inner() {
                    for (agent, elapsed_ms) in attributions {
                        let entry = run.agent_usage.entry(agent).or_default();
                        entry.delegations += 1;
                        entry.remote_ms += elapsed_ms;
                    }
                }

                // Governed resilience (ADR 0023): bounded retry against the
                // same target, then sanctioned fallback — ONLY for a failed
                // A2A delegation. Every attempt re-enters
                // `authorize_delegation` (P2) and, when configured, the
                // `DelegationGate` (P3) exactly like a fresh proposal; see
                // `run_delegation_resilience`'s docs for the full contract.
                if is_a2a_action(validated.action()) {
                    if ok {
                        last_error = None;
                    } else {
                        let (r, o, extra_attributions) = self
                            .run_delegation_resilience(
                                &run,
                                run_id,
                                step_index,
                                validated.action(),
                                validated.arguments(),
                                &schema,
                                &mut seq,
                                result.clone(),
                                decision_seq,
                            )
                            .await?;
                        result = r;
                        ok = o;
                        for (agent, elapsed_ms) in extra_attributions {
                            let entry = run.agent_usage.entry(agent).or_default();
                            entry.delegations += 1;
                            entry.remote_ms += elapsed_ms;
                        }
                        last_error = if ok {
                            None
                        } else {
                            Some(format!(
                                "delegation via '{}' failed after retry/fallback: {}. Adjust \
                                 and continue.",
                                validated.action(),
                                result
                                    .get("error")
                                    .and_then(|e| e.as_str())
                                    .unwrap_or("unknown error")
                            ))
                        };
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
                    branch: None,
                },
            );
            seq += 1;
        }

        let (status, output, error) = terminal.unwrap_or((
            RunStatus::Failed,
            None,
            Some(format!(
                "step budget exhausted ({step_budget} steps) without finish"
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
            (RunStatus::Incomplete, out, err) => {
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Incomplete {
                        output: out,
                        reason: err.unwrap_or_else(|| "unknown".into()),
                    },
                );
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

        // 2. Journaled plans, in proposal order (ADR 0030): index = plan
        //    GENERATION; the LAST entry is the active plan. Replay is law —
        //    a journaled plan is never re-queried. Generation g's node at
        //    plan-index i journals under step g * PLAN_GENERATION_STRIDE + i,
        //    so receipts from different generations can never collide (gen 0
        //    keys are exactly the pre-ADR-0030 keys — old journals resume
        //    unchanged).
        let mut plans: Vec<crate::plan::Plan> = journal
            .iter()
            .filter_map(|e| match e {
                JournalEntry::PlanProposed { plan } => Some(plan.clone()),
                _ => None,
            })
            .collect();

        // Carry-forward map (ADR 0030, exactly-once through re-planning): a
        // node a PRIOR generation completed successfully — identified by its
        // (id, action, arguments) — is never re-executed; a revised plan
        // re-including it verbatim gets the journaled result applied instead
        // of dispatching. Rebuilt deterministically from the journal alone,
        // so crash-resume reaches the same map without executing anything.
        // Review nodes are never carried: a verdict is re-taken over the NEW
        // plan's upstream state, not replayed across plans.
        let mut carried: std::collections::HashMap<(String, String, String), serde_json::Value> =
            std::collections::HashMap::new();
        for (g, plan) in plans.iter().enumerate() {
            for (i, node) in plan.nodes.iter().enumerate() {
                if node.node_type == crate::plan::NodeType::Review {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation)]
                let step = (g * PLAN_GENERATION_STRIDE as usize + i) as u32;
                if let Some((true, result)) = fold_step(&journal, step).receipt {
                    carried.insert(plan_carry_key(node), result);
                }
            }
        }

        // Delegation-attempt counter for THIS call (ADR 0022): a plain
        // local, incremented the moment an A2A node is considered —
        // admitted or refused — strictly BEFORE any spawn, so concurrent
        // admission within one pass can never under-count and over-admit
        // past the budget (recomputing from `run.steps` on every check, as
        // step mode does, would under-count siblings admitted earlier in
        // the same pass that haven't completed/recorded a `StepRecord`
        // yet). Seeded from every A2A node ANY journaled generation already
        // has a Receipt OR a dangling Intent for (from a prior crashed
        // session), so a resume does not reset the run-scoped budget.
        // Node counts are bounded by the plan's own size (MAX_NODES) and
        // the generation count by max_replans — never remotely near u32::MAX.
        #[allow(clippy::cast_possible_truncation)]
        let mut a2a_attempts: u32 = plans
            .iter()
            .enumerate()
            .map(|(g, plan)| {
                plan.nodes
                    .iter()
                    .enumerate()
                    .filter(|(i, n)| {
                        n.node_type == crate::plan::NodeType::A2a && {
                            let step = (g * PLAN_GENERATION_STRIDE as usize + i) as u32;
                            let f = fold_step(&journal, step);
                            f.intent.is_some() || f.receipt.is_some()
                        }
                    })
                    .count()
            })
            .sum::<usize>() as u32;

        // Re-plan budget (ADR 0030 crux #2, BOUNDED): counts re-plan
        // PROPOSALS — accepted ones (journaled, so the count survives a
        // crash-resume) and rejected ones (session-local). The generation
        // loop below asks the model at most `max_replans` times beyond the
        // original plan, then fails cleanly — no re-plan→fail→re-plan loop.
        let resilience = self.config.plan_resilience;
        #[allow(clippy::cast_possible_truncation)]
        let mut replans_used: u32 = plans.len().saturating_sub(1) as u32;
        // Bounded failure context for the NEXT re-plan proposal; `Some` also
        // means "a fresh proposal is required" (the active plan failed, or
        // the previous proposal was rejected by validation). Replaced
        // wholesale on each failure — never accumulated across generations.
        let mut replan_guidance: Option<String> = None;

        // The validation floor (ADR 0001/0010, re-affirmed by ADR 0030 crux
        // #1): every plan — original or revised — validates against the SAME
        // pristine tool schema. The kernel-owned review action is only part
        // of the EXECUTION schema below, never proposable as a plan node
        // action.
        let validation_schema = schema.clone();
        schema.actions.push(ActionSpec {
            name: crate::plan::REVIEW_ACTION.into(),
            description: "Approve or reject based on upstream node outputs.".into(),
            parameters: crate::plan::review_schema(),
        });

        // Retrieved context — which plan mode has never had. This branch
        // returns before step mode's memory block, so plan-mode runs have been
        // composing plans against an empty `PolicyContext.memory`. Same
        // sources and the same `[learned]` tagging as step mode, and the same
        // degradation: a failing backend costs context quality, never run
        // integrity.
        let mut memory_context: Vec<String> = match self.memory.retrieve(&run.task, 3).await {
            Ok(hits) => hits.into_iter().map(|h| h.summary).collect(),
            Err(e) => {
                tracing::warn!(%run_id, "memory retrieve failed: {e}");
                Vec::new()
            }
        };
        if let Some(learned) = &self.learned {
            match learned.retrieve(&run.task, 3).await {
                Ok(facts) => {
                    memory_context
                        .extend(facts.iter().map(crate::learned::LearnedFact::as_context));
                }
                Err(e) => tracing::warn!(%run_id, "learned retrieve failed: {e}"),
            }
        }

        // Candidate learned plans (ADR 0016's deferred second slice). Each is
        // re-validated against the LIVE action space here and only survivors
        // are offered, so a plan naming a tool the registry no longer carries
        // simply does not exist for the model. That is the structural analogue
        // of forgetting: no expiry field, no staleness policy — the action
        // space itself does the pruning.
        let candidates: Vec<crate::learned::LearnedPlan> = match &self.learned {
            Some(learned) => match learned.retrieve_plans(&run.task, 3).await {
                Ok(found) => found
                    .into_iter()
                    .filter(|c| {
                        crate::plan::validate(c.plan.clone(), &validation_schema, is_a2a_action)
                            .is_ok()
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(%run_id, "learned plan retrieve failed: {e}");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        'generations: loop {
            // Obtain this generation's plan: replay the journaled active plan,
            // or ask the model ONCE (offering only `propose_plan`,
            // schema-constrained). A re-plan request (`replan_guidance` set) is
            // the SAME action through the SAME validation — informed about the
            // failure, privileged in nothing.
            #[allow(clippy::cast_possible_truncation)]
            let generation: u32 = if replan_guidance.is_some() {
                plans.len() as u32
            } else {
                plans.len().saturating_sub(1) as u32
            };
            let step_base = generation * PLAN_GENERATION_STRIDE;
            let (plan, fresh_proposal) = match (&replan_guidance, plans.last()) {
                (None, Some(journaled)) => {
                    tracing::info!(
                        %run_id, generation,
                        "resume: replaying journaled plan — model NOT queried"
                    );
                    (journaled.clone(), false)
                }
                _ => {
                    // Adoption is offered on the FIRST proposal only. A plan
                    // that just failed must never return as a candidate — that
                    // is a direct route to a loop — and confining the offer to
                    // generation zero leaves the ADR 0030 re-plan budget
                    // untouched.
                    let mut offer_adoption = replan_guidance.is_none() && !candidates.is_empty();
                    loop {
                        let mut actions = vec![ActionSpec {
                            name: crate::plan::PROPOSE_PLAN_ACTION.into(),
                            description:
                                "Propose a plan: a DAG of typed nodes (tool / a2a / review) \
                                      with dependencies. Keep it minimal and correct."
                                    .into(),
                            parameters: crate::plan::plan_schema(),
                        }];
                        if offer_adoption {
                            actions.push(ActionSpec {
                                name: crate::plan::ADOPT_PLAN_ACTION.into(),
                                description: "Adopt one of the learned plans listed in the \
                                     guidance, by its id, instead of composing a new one."
                                    .into(),
                                parameters: crate::plan::adopt_schema(),
                            });
                        }
                        let propose_schema = ActionSchema { actions };
                        let base_rules = format!(
                            "Available tool/a2a actions: [{}]. Rules: each node needs a unique `id` \
                     (choose short ids like n1, n2 — NOT the action name); a node's `deps` \
                     must be [] or list OTHER nodes' ids — a node must never depend on \
                     itself or on its own action; a `tool`/`a2a` node needs `action` + \
                     `arguments`; a `review` node needs no action. The simplest valid plan \
                     is one tool node with deps [].",
                            validation_schema
                                .actions
                                .iter()
                                .map(|a| a.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        // The candidates' CONTENTS reach the prompt, not just
                        // their ids: the model decides seeing what it would
                        // adopt, rather than blind on an opaque handle.
                        let candidate_rules = if offer_adoption {
                            let listed = candidates
                                .iter()
                                .map(|c| {
                                    let nodes = c
                                        .plan
                                        .nodes
                                        .iter()
                                        .map(|n| match &n.action {
                                            Some(a) => format!("{}={a}", n.id),
                                            None => format!("{}=review", n.id),
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    format!(
                                        "- id {} [learned, lower-trust], distilled from a task \
                                         like \"{}\": [{nodes}]",
                                        c.id, c.task_context
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            format!(
                                "\nLearned plans that still fit this action space — adopt one with \
                                 `{}` if it suits the task, otherwise propose your own:\n{listed}",
                                crate::plan::ADOPT_PLAN_ACTION
                            )
                        } else {
                            String::new()
                        };
                        let guidance = match &replan_guidance {
                            Some(failure_ctx) => {
                                format!("{base_rules}{candidate_rules}\n{failure_ctx}")
                            }
                            None => format!("{base_rules}{candidate_rules}"),
                        };
                        let context = PolicyContext {
                            task: run.task.clone(),
                            steps: Vec::new(),
                            memory: memory_context.clone(),
                            last_error: replan_guidance.as_ref().map(|_| {
                                "previous plan failed — a revised plan was requested".into()
                            }),
                            latitude,
                            guidance: Some(guidance),
                            active_skills: run.active_skills.clone(),
                        };
                        let proposal = self
                            .oracle
                            .propose(&context, &propose_schema)
                            .await
                            .map_err(|e| {
                                ExecutionError::Tool(format!("plan proposal failed: {e}"))
                            })?;
                        if let Some(u) = proposal.usage {
                            run.usage.add(&u);
                        }
                        let validated = propose_schema.validate(proposal.action).map_err(|e| {
                            ExecutionError::Tool(format!("proposed plan is not schema-valid: {e}"))
                        })?;

                        if validated.action() == crate::plan::ADOPT_PLAN_ACTION {
                            let requested = validated.arguments()["plan_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            match candidates.iter().find(|c| c.id.to_string() == requested) {
                                Some(chosen) => {
                                    tracing::info!(
                                        %run_id, plan_id = %chosen.id,
                                        "plan: adopted a learned plan — validated below like any other"
                                    );
                                    break (chosen.plan.clone(), true);
                                }
                                None => {
                                    // A hallucinated id, or one forgotten
                                    // between retrieval and now. Ask once more
                                    // WITHOUT the adoption action: the retry is
                                    // bounded by the schema itself rather than
                                    // by a counter, and consumes no re-plan
                                    // budget. Failing the whole run over a bad
                                    // id would be disproportionate.
                                    tracing::warn!(
                                        %run_id, requested,
                                        "plan: adoption named an unknown plan id — re-asking for a fresh plan"
                                    );
                                    offer_adoption = false;
                                    continue;
                                }
                            }
                        }

                        let plan: crate::plan::Plan =
                            serde_json::from_value(validated.arguments().clone()).map_err(|e| {
                                ExecutionError::Tool(format!("malformed plan: {e}"))
                            })?;
                        break (plan, true);
                    }
                }
            };

            // 3. Validate the plan (structure/cycles/types/references) BEFORE
            //    any node executes — the SAME floor for the original plan and
            //    every re-plan (ADR 0030 crux #1). An invalid ORIGINAL plan
            //    fails the run — no partial side effects (this gate runs before
            //    the PlanProposed journal). An invalid RE-plan is REJECTED and
            //    consumes a re-plan slot: with budget left the model is asked
            //    again (told why the proposal was rejected); budget exhausted →
            //    the run fails cleanly.
            let validated_plan =
                match crate::plan::validate(plan.clone(), &validation_schema, is_a2a_action) {
                    Ok(v) => v,
                    Err(e) => {
                        let was_replan = replan_guidance.is_some();
                        let reason = format!("plan rejected: {e}");
                        tracing::warn!(%run_id, "{reason}");
                        if was_replan && replans_used < resilience.max_replans {
                            replans_used += 1;
                            // The refused revision is never journaled as a
                            // `PlanProposed`, so the entry carries it (ADR
                            // 0036); it was never announced as `Proposed`.
                            self.reject(
                                run_id,
                                &mut seq,
                                Rejection {
                                    step: step_base,
                                    attempt: replans_used,
                                    source: RejectionSource::PlanValidation,
                                    subject: RejectionSubject::Proposal,
                                    reason: reason.clone(),
                                    action: Some(crate::plan::PROPOSE_PLAN_ACTION.into()),
                                    arguments: Some(
                                        serde_json::to_value(&plan)
                                            .unwrap_or(serde_json::Value::Null),
                                    ),
                                    branch: None,
                                    proposal_seq: None,
                                },
                            )
                            .await;
                            replan_guidance = Some(format!(
                                "{}\nYour previous revised plan was REJECTED by validation: {}. \
                             Propose a corrected plan.",
                                replan_guidance.as_deref().unwrap_or_default(),
                                truncate_chars(&e.to_string(), 300)
                            ));
                            continue 'generations;
                        }
                        let reason = if was_replan {
                            format!(
                                "re-{reason} ({replans_used} re-plan(s) used, budget exhausted)"
                            )
                        } else {
                            reason
                        };
                        return self
                            .finalize_run(run, RunStatus::Failed, None, Some(reason), seq)
                            .await;
                    }
                };

            // Journal an ACCEPTED plan (once per generation). On resume this is
            // what gets replayed instead of re-querying the model — a crash
            // between this append and the run's terminal state resumes HERE,
            // deterministically, with the same plan.
            if fresh_proposal {
                self.store
                    .append_journal(run_id, JournalEntry::PlanProposed { plan: plan.clone() })
                    .await?;
                plans.push(plan);
            }
            // What a refused node of this generation refers to (ADR 0036).
            let plan_seq = seq;
            let node_refusal = |step: u32,
                                attempt: u32,
                                source: RejectionSource,
                                node: &crate::plan::PlanNode,
                                reason: String| {
                Rejection {
                    step,
                    attempt,
                    source,
                    subject: RejectionSubject::PlanNode,
                    reason,
                    action: node.action.clone(),
                    arguments: None,
                    branch: Some(node.id.clone()),
                    proposal_seq: Some(plan_seq),
                }
            };
            self.emit(
                run_id,
                seq,
                RunEvent::Proposed {
                    step: step_base,
                    action: crate::plan::PROPOSE_PLAN_ACTION.into(),
                    arguments: serde_json::json!({ "nodes": validated_plan.len() }),
                    rationale: Some(if generation == 0 {
                        "validated plan".into()
                    } else {
                        format!("validated re-plan (generation {generation})")
                    }),
                    branch: None,
                },
            );
            seq += 1;

            // 4. Execute nodes: a readiness-driven admission loop (ADR 0022,
            //    Part 2). Independent nodes run CONCURRENTLY once their
            //    dependencies are satisfied; this orchestrator is the single
            //    writer of `run`/`node_outputs` (no locking needed) — the
            //    durable intent→execute→receipt tail for each admitted node
            //    runs inside a spawned task (`dispatch_plan_node_execution`).
            //    Governance (delegation budget/depth/cycle, ADR 0022 Part 3)
            //    runs HERE, on the orchestrator, strictly before spawning: the
            //    admission-control point that makes a race on the budget
            //    counter structurally impossible (checked+incremented before
            //    any concurrent work starts).
            let nodes: Vec<(usize, crate::plan::PlanNode)> = validated_plan
                .nodes_in_order()
                .map(|(i, n)| (i, n.clone()))
                .collect();

            let mut node_outputs: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            let mut completed: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut dispatched: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut failure: Option<String> = None;
            let mut in_flight: FuturesUnordered<
                tokio::task::JoinHandle<Result<PlanNodeOutcome, KernelError>>,
            > = FuturesUnordered::new();

            // Bounded per-node retry state (ADR 0030): attempts already burned
            // per node index (seeded from journaled failed receipts on resume,
            // incremented live), and the set of nodes whose NEXT dispatch is a
            // fresh retry attempt — dispatched with a clean per-step journal
            // view so a definitive failed receipt isn't mistaken for a crash
            // window. The idempotency key is a pure function of (run, step), so
            // every retry reuses the SAME key: a keyed tool whose effect
            // somehow DID land before the failure dedupes instead of
            // double-executing (exactly-once, crux #3).
            let mut node_retries: std::collections::HashMap<usize, u32> =
                std::collections::HashMap::new();
            let mut retry_fresh: std::collections::HashSet<usize> =
                std::collections::HashSet::new();

            // Drain-then-pause holder (ADR 0025): set when a FRESH a2a node's
            // trust gate returns `AskHuman`. Once set, no new node is admitted
            // (the admission guard below also checks it), so already-in-flight
            // siblings drain to receipts — keeping the resume budget seed exact —
            // and then the run suspends durably as `AwaitingDelegationDecision`
            // rather than degrading the ask to a deny (the ADR 0023 plan-mode
            // gap). Only ONE node's ask is surfaced at a time: with the loop
            // frozen to new admissions, a second ask-worthy node simply waits its
            // turn on the next resume — the single-decision-at-a-time contract
            // step mode already has.
            struct PlanAsk {
                step: u32,
                branch: String,
                agent: String,
                origin: String,
                arguments: serde_json::Value,
                trust: String,
                description: String,
                provenance_source_kind: String,
                provenance_source: String,
                requested_at: u64,
            }
            let mut pending_ask: Option<PlanAsk> = None;

            loop {
                for (node_index, node) in &nodes {
                    if completed.contains(node_index) || dispatched.contains(node_index) {
                        continue;
                    }
                    if failure.is_some() || pending_ask.is_some() {
                        // No NEW node is admitted once the plan has failed
                        // (fail-fast, ADR 0010) OR a node is awaiting human
                        // delegation authorization (drain-then-pause, ADR 0025) —
                        // but already in-flight siblings are left to drain
                        // naturally below, never orphaned.
                        continue;
                    }
                    if !validated_plan
                        .deps_of(*node_index)
                        .iter()
                        .all(|d| node_outputs.contains_key(d))
                    {
                        continue;
                    }

                    // Bounded by the plan's own node count — see the comment on
                    // `a2a_attempts`'s seed above.
                    #[allow(clippy::cast_possible_truncation)]
                    let step = step_base + *node_index as u32;
                    // A node re-admitted for a RETRY dispatches with a clean
                    // per-step view: its failed receipt is a definitive outcome,
                    // not a crash window — re-reading it here would either
                    // re-complete the node as failed or take the dangling-intent
                    // path. The deterministic (run, step) idempotency key still
                    // protects the effect.
                    let folded = if retry_fresh.contains(node_index) {
                        crate::journal::StepJournal::default()
                    } else {
                        fold_step(&journal, step)
                    };

                    // Resume fast-path: already has a receipt from before a
                    // crash — apply it inline, no need to spawn/await anything.
                    // A FAILED receipt with retry budget left (ADR 0030) is NOT
                    // terminal: seed the attempt count from the journal and fall
                    // through to a fresh retry dispatch instead.
                    if let Some((ok, result)) = folded.receipt.clone() {
                        #[allow(clippy::cast_possible_truncation)]
                    let attempts_so_far = journal
                        .iter()
                        .filter(|e| matches!(e, JournalEntry::Receipt { step: s, .. } if *s == step))
                        .count() as u32;
                        if !ok
                            && node.node_type != crate::plan::NodeType::Review
                            && attempts_so_far <= resilience.max_node_retries
                        {
                            tracing::info!(
                                %run_id, node = %node.id, attempts_so_far,
                                "resume: node failed before the crash — retry budget left, \
                                 re-dispatching"
                            );
                            node_retries.insert(*node_index, attempts_so_far);
                            retry_fresh.insert(*node_index);
                            continue;
                        }
                        tracing::info!(
                            %run_id, node = %node.id,
                            "resume: node receipt found — skipping execution"
                        );
                        node_outputs.insert(node.id.clone(), result.clone());
                        self.record_node_step(&mut run, step, node, result, ok)
                            .await?;
                        completed.insert(*node_index);
                        dispatched.insert(*node_index);
                        if !ok {
                            failure =
                                Some(format!("node '{}' had failed before the crash", node.id));
                        }
                        continue;
                    }

                    // Carry-forward fast-path (ADR 0030, exactly-once through
                    // re-planning): a PRIOR generation already completed this
                    // exact node — same id, action and arguments — so its
                    // journaled result is applied instead of re-executing the
                    // side effect. Review nodes never carry (map excludes them).
                    if !retry_fresh.contains(node_index) {
                        if let Some(prior) = carried.get(&plan_carry_key(node)).cloned() {
                            tracing::info!(
                                %run_id, node = %node.id,
                                "re-plan: node completed in a prior generation — carrying its \
                                 result forward, NOT re-executing"
                            );
                            node_outputs.insert(node.id.clone(), prior.clone());
                            self.record_node_step(&mut run, step, node, prior.clone(), true)
                                .await?;
                            self.emit(
                                run_id,
                                seq,
                                RunEvent::Executed {
                                    step,
                                    action: node.action.clone().unwrap_or_default(),
                                    ok: true,
                                    result: prior,
                                    branch: Some(node.id.clone()),
                                },
                            );
                            seq += 1;
                            completed.insert(*node_index);
                            dispatched.insert(*node_index);
                            continue;
                        }
                    }

                    let is_a2a_node = node.node_type == crate::plan::NodeType::A2a;

                    // Resume path (ADR 0025): this a2a node reached an `AskHuman`
                    // gate in a prior session and the human has since decided.
                    // Consume the recorded decision — replay is law, the gate is
                    // NOT re-consulted. `Deny` fails the node (and the plan);
                    // `Allow*` counts one budget slot and falls through to a
                    // normal dispatch. An undecided ask should have been
                    // re-surfaced by `execute_run`'s `still_awaiting` check before
                    // reaching here; treat it defensively as a re-pause.
                    if is_a2a_node {
                        if let Some(fold) = folded.delegation.clone() {
                            match fold.decision {
                                Some(AgentDelegationDecision::Deny) => {
                                    let reason = format!(
                                    "the operator declined to authorize delegating to '{}' for \
                                     this run",
                                    fold.agent
                                );
                                    self.reject(
                                        run_id,
                                        &mut seq,
                                        node_refusal(
                                            step,
                                            0,
                                            RejectionSource::OperatorDenied,
                                            node,
                                            reason.clone(),
                                        ),
                                    )
                                    .await;
                                    let result = serde_json::json!({ "error": reason });
                                    self.record_node_step(
                                        &mut run,
                                        step,
                                        node,
                                        result.clone(),
                                        false,
                                    )
                                    .await?;
                                    node_outputs.insert(node.id.clone(), result);
                                    completed.insert(*node_index);
                                    dispatched.insert(*node_index);
                                    failure = Some(format!(
                                        "plan node '{}' delegation denied by the operator",
                                        node.id
                                    ));
                                    continue;
                                }
                                Some(AgentDelegationDecision::AllowAlways) => {
                                    if let Some(gate) = &self.delegation_gate {
                                        if let Err(e) = gate.elevate_trust(&fold.agent) {
                                            tracing::warn!(
                                                %run_id, agent = %fold.agent,
                                                "trust elevation failed (delegation still applies \
                                                 for this run): {e}"
                                            );
                                        }
                                    }
                                    // Audit finding 3-c: only count this node's
                                    // attempt here if the seed loop above did NOT
                                    // already count it via a dangling Intent — a
                                    // two-crash resume (decided in session A,
                                    // Intent journaled in session A, crashed
                                    // AGAIN before a Receipt) would otherwise hit
                                    // this same arm a second time and double-count
                                    // a single attempt. Direction was safe
                                    // (over-count → may over-refuse a sibling,
                                    // never over-admit past the real budget), but
                                    // could spuriously fail an otherwise-fine plan.
                                    if folded.intent.is_none() {
                                        a2a_attempts += 1;
                                    }
                                    // Fall through to dispatch (gate NOT re-checked).
                                }
                                Some(AgentDelegationDecision::AllowOnce) => {
                                    if folded.intent.is_none() {
                                        a2a_attempts += 1;
                                    }
                                    // Fall through to dispatch (gate NOT re-checked).
                                }
                                None => {
                                    // Defensive re-pause: keep the run suspended
                                    // rather than silently proceeding.
                                    pending_ask = Some(PlanAsk {
                                        step,
                                        branch: node.id.clone(),
                                        agent: fold.agent.clone(),
                                        origin: fold.target_origin.clone(),
                                        arguments: fold.arguments.clone(),
                                        trust: fold.meta.trust.clone(),
                                        description: fold.meta.description.clone(),
                                        provenance_source_kind: fold
                                            .meta
                                            .provenance_source_kind
                                            .clone(),
                                        provenance_source: fold.meta.provenance_source.clone(),
                                        requested_at: fold.requested_at,
                                    });
                                    continue;
                                }
                            }
                        }
                    }

                    // Fresh admission — governed delegation limits (ADR 0022) +
                    // trust gate (ADR 0023). Skipped for a node with a dangling
                    // Intent (already passed this gate before a crash; "replay is
                    // law") and for a node whose ask was pre-decided above
                    // (`folded.delegation` present → handled by the resume arm).
                    let has_dangling_intent = folded.intent.is_some();
                    let ask_already_recorded = folded.delegation.is_some();
                    if is_a2a_node && !has_dangling_intent && !ask_already_recorded {
                        let action_name = node.action.clone().unwrap_or_default();
                        let target_origin = self.tools.a2a_origin(&action_name);
                        // P2 (budget/depth/cycle) FIRST — a hard refusal fails the
                        // plan (a run-scoped governance fact); the counter is
                        // incremented exactly once per considered a2a node.
                        let refusal = match &target_origin {
                            None => Some(format!(
                            "internal error: no origin declared for a2a action '{action_name}' \
                             (plan validation should have caught this) — refusing rather than \
                             dialing an unverified target"
                        )),
                            Some(origin) => {
                                self.authorize_delegation(&run, a2a_attempts, origin).err()
                            }
                        }
                        .map(|reason| (RejectionSource::DelegationLimit, reason));
                        a2a_attempts += 1;
                        // Circuit-breaker (ADR 0023): an open circuit is a node
                        // failure here — plan-mode does not retry/fall back within
                        // a node this round (disclosed scope, ADR 0025).
                        let refusal = refusal.or_else(|| {
                            target_origin.as_deref()?;
                            matches!(
                                self.tools.circuit_state(&action_name),
                                CircuitCheck::Open { .. }
                            )
                            .then(|| {
                                (
                                    RejectionSource::CircuitOpen,
                                    format!(
                                        "agent circuit open for '{action_name}' — refusing to \
                                         dial (plan-mode does not retry/fall back within a node \
                                         this round)"
                                    ),
                                )
                            })
                        });
                        if let Some((source, reason)) = refusal {
                            self.reject(
                                run_id,
                                &mut seq,
                                node_refusal(step, 0, source, node, reason.clone()),
                            )
                            .await;
                            let result = serde_json::json!({ "error": reason });
                            self.record_node_step(&mut run, step, node, result.clone(), false)
                                .await?;
                            node_outputs.insert(node.id.clone(), result);
                            completed.insert(*node_index);
                            dispatched.insert(*node_index);
                            failure = Some(format!(
                                "plan node '{}' refused by delegation governance",
                                node.id
                            ));
                            continue;
                        }

                        // Trust gate (ADR 0023) — now THREE-way in plan-mode
                        // (ADR 0025): Allow → dispatch; Deny → node/plan fail;
                        // AskHuman → drain-then-pause (no degrade-to-deny). Runs
                        // SECOND, only once P2 already allowed this delegation
                        // structurally — same ordering as step mode.
                        if let Some(gate) = &self.delegation_gate {
                            let origin = target_origin.clone().expect(
                                "target_origin is Some: the None arm above already refused",
                            );
                            match gate.authorize_delegation_to(&action_name, &origin) {
                                DelegationDecisionOutcome::Allow(_) => { /* dispatch below */ }
                                DelegationDecisionOutcome::Deny(reason) => {
                                    self.reject(
                                        run_id,
                                        &mut seq,
                                        node_refusal(
                                            step,
                                            0,
                                            RejectionSource::DelegationGate,
                                            node,
                                            reason.clone(),
                                        ),
                                    )
                                    .await;
                                    let result = serde_json::json!({ "error": reason });
                                    self.record_node_step(
                                        &mut run,
                                        step,
                                        node,
                                        result.clone(),
                                        false,
                                    )
                                    .await?;
                                    node_outputs.insert(node.id.clone(), result);
                                    completed.insert(*node_index);
                                    dispatched.insert(*node_index);
                                    failure = Some(format!(
                                        "plan node '{}' refused by delegation governance",
                                        node.id
                                    ));
                                    continue;
                                }
                                DelegationDecisionOutcome::AskHuman(meta) => {
                                    pending_ask = Some(PlanAsk {
                                        step,
                                        branch: node.id.clone(),
                                        agent: action_name.clone(),
                                        origin,
                                        arguments: node.arguments.clone(),
                                        trust: meta.trust.clone(),
                                        description: meta.description.clone(),
                                        provenance_source_kind: meta.provenance_source_kind.clone(),
                                        provenance_source: meta.provenance_source.clone(),
                                        requested_at: now_secs(),
                                    });
                                    continue;
                                }
                            }
                        }
                    }

                    dispatched.insert(*node_index);
                    let deps_context: Vec<serde_json::Value> = node
                        .deps
                        .iter()
                        .filter_map(|d| node_outputs.get(d).cloned())
                        .collect();
                    let handle = tokio::spawn(dispatch_plan_node_execution(
                        run_id,
                        step,
                        node.clone(),
                        deps_context,
                        folded,
                        schema.clone(),
                        run.active_skills.clone(),
                        run.a2a_depth,
                        run.a2a_ancestry.clone(),
                        latitude,
                        self.store.clone(),
                        self.tools.clone(),
                        self.oracle.clone(),
                        self.events.clone(),
                    ));
                    in_flight.push(handle);
                }

                if in_flight.is_empty() {
                    break;
                }

                // Readiness-driven: await the NEXT completion (not all) — as
                // soon as one branch finishes, its dependents may become ready
                // this same loop iteration.
                let joined = in_flight
                    .next()
                    .await
                    .expect("in_flight is non-empty (just checked above)");
                let outcome = match joined {
                    Ok(Ok(o)) => o,
                    Ok(Err(kernel_err)) => return Err(kernel_err),
                    Err(join_err) => {
                        return Err(KernelError::Execution(ExecutionError::Tool(format!(
                            "plan node task panicked: {join_err}"
                        ))));
                    }
                };
                // `outcome.node_index` carries the generation-scoped STEP; the
                // plan-local index is recovered by subtracting this generation's
                // base.
                let node_index = (outcome.node_index - step_base) as usize;
                let node = validated_plan.node(node_index);

                if let Some(u) = outcome.usage {
                    run.usage.add(&u);
                }
                for (agent, elapsed_ms) in &outcome.agent_usage {
                    let entry = run.agent_usage.entry(agent.clone()).or_default();
                    entry.delegations += 1;
                    entry.remote_ms += elapsed_ms;
                }

                // Bounded per-node retry (ADR 0030): a FAILED tool/a2a node with
                // budget left is re-admitted instead of failing the plan — no
                // step is recorded for the burned attempt (its failed receipt is
                // journaled by the dispatch tail; `fold_step` last-wins, so a
                // crash mid-retry resumes from the latest attempt). Review nodes
                // never retry: a rejection is a verdict, not a transient fault.
                // An a2a retry re-enters the FULL admission governance (budget +
                // trust gate) exactly like a fresh dial — the ADR 0023 rule.
                if !outcome.ok && node.node_type != crate::plan::NodeType::Review {
                    let used = node_retries.entry(node_index).or_insert(0);
                    if *used < resilience.max_node_retries {
                        *used += 1;
                        let attempt = *used;
                        tracing::warn!(
                            %run_id, node = %node.id, attempt,
                            max = resilience.max_node_retries,
                            "plan node failed — retrying (bounded)"
                        );
                        self.reject(
                            run_id,
                            &mut seq,
                            node_refusal(
                                outcome.node_index,
                                attempt,
                                RejectionSource::NodeRetry,
                                node,
                                truncate_chars(
                                    &format!(
                                        "node '{}' attempt failed — retrying: {}",
                                        node.id, outcome.result
                                    ),
                                    400,
                                ),
                            ),
                        )
                        .await;
                        dispatched.remove(&node_index);
                        retry_fresh.insert(node_index);
                        continue;
                    }
                }

                node_outputs.insert(node.id.clone(), outcome.result.clone());
                self.record_node_step(
                    &mut run,
                    outcome.node_index,
                    node,
                    outcome.result.clone(),
                    outcome.ok,
                )
                .await?;
                self.emit(
                    run_id,
                    seq,
                    RunEvent::Executed {
                        step: outcome.node_index,
                        action: node
                            .action
                            .clone()
                            .unwrap_or_else(|| format!("{:?}", node.node_type).to_lowercase()),
                        ok: outcome.ok,
                        result: outcome.result,
                        branch: Some(node.id.clone()),
                    },
                );
                seq += 1;
                completed.insert(node_index);
                if !outcome.ok {
                    // Fail-on-node-error: no NEW node is admitted (the guard at
                    // the top of the admission pass), already-in-flight siblings
                    // drain naturally, and then the generation either re-plans
                    // (ADR 0030, budget permitting) or the run fails.
                    failure = Some(format!("plan node '{}' failed", node.id));
                }
            }

            // Drain-then-pause resolution (ADR 0025): a fresh a2a node's trust
            // gate asked for human authorization. The loop only exits once
            // `in_flight` is empty, so every sibling has drained to a receipt —
            // the resume budget seed is therefore exact — and the run now suspends
            // durably as `AwaitingDelegationDecision`. A concurrent sibling
            // FAILURE during the drain takes priority (the plan is doomed anyway),
            // so the pause is only taken when `failure` is still None; otherwise
            // the ask is abandoned unjournaled (its node never ran) and the plan
            // finalizes as failed below.
            if failure.is_none() {
                if let Some(ask) = pending_ask {
                    self.store
                        .append_journal(
                            run_id,
                            JournalEntry::AgentDelegationRequested {
                                step: ask.step,
                                agent: ask.agent.clone(),
                                target_origin: ask.origin.clone(),
                                arguments: ask.arguments.clone(),
                                trust: ask.trust.clone(),
                                description: ask.description.clone(),
                                provenance_source_kind: ask.provenance_source_kind.clone(),
                                provenance_source: ask.provenance_source.clone(),
                                requested_at: ask.requested_at,
                            },
                        )
                        .await?;
                    run.status = RunStatus::AwaitingDelegationDecision;
                    run.pending_delegation = Some(PendingDelegation {
                        step: ask.step,
                        agent: ask.agent.clone(),
                        target_origin: ask.origin.clone(),
                        trust: ask.trust.clone(),
                        description: ask.description.clone(),
                        provenance_source_kind: ask.provenance_source_kind.clone(),
                        provenance_source: ask.provenance_source.clone(),
                        requested_at: ask.requested_at,
                    });
                    self.store.put(run.clone()).await?;
                    self.emit(
                        run_id,
                        seq,
                        RunEvent::AgentDelegationRequested {
                            step: ask.step,
                            agent: ask.agent,
                            target_origin: ask.origin,
                            description: ask.description,
                            trust: ask.trust,
                            provenance_source_kind: ask.provenance_source_kind,
                            provenance_source: ask.provenance_source,
                            branch: Some(ask.branch),
                        },
                    );
                    return Ok(run);
                }
            }

            match failure {
                None => {
                    let summary = if generation == 0 {
                        format!("plan completed: {} node(s)", validated_plan.len())
                    } else {
                        format!(
                            "plan completed: {} node(s) (after {} re-plan(s))",
                            validated_plan.len(),
                            generation
                        )
                    };
                    let output = Some(serde_json::json!({
                        "summary": summary,
                        "nodes": node_outputs,
                    }));
                    // Same success predicate as step mode (ADR 0033): a plan
                    // whose every node is a review dispatched nothing.
                    if outcome::effective_steps(&run.steps) == 0 {
                        let reason = outcome::no_effect_reason(&run.steps);
                        return self
                            .finalize_run(run, RunStatus::Incomplete, output, Some(reason), seq)
                            .await;
                    }
                    return self
                        .finalize_run(run, RunStatus::Completed, output, None, seq)
                        .await;
                }
                Some(err) => {
                    // Adaptive re-planning (ADR 0030): with budget left, fold
                    // this generation's successes into the carry-forward map,
                    // hand the model a BOUNDED failure context, and loop for a
                    // fresh `propose_plan` — through the same validation floor.
                    // Budget exhausted (or resilience off) → the run fails
                    // cleanly, exactly as before ADR 0030.
                    if replans_used < resilience.max_replans {
                        replans_used += 1;
                        let this_gen_steps: Vec<&StepRecord> = run
                            .steps
                            .iter()
                            .filter(|s| {
                                s.index >= step_base && s.index < step_base + PLAN_GENERATION_STRIDE
                            })
                            .collect();
                        let mut completed_desc = String::new();
                        let mut failed_desc = String::new();
                        for s in &this_gen_steps {
                            let node = validated_plan.node((s.index - step_base) as usize);
                            if s.ok {
                                if node.node_type != crate::plan::NodeType::Review {
                                    carried.insert(plan_carry_key(node), s.result.clone());
                                }
                                completed_desc.push_str(&format!(
                                    "\n- id={} action={} arguments={} output={}",
                                    node.id,
                                    s.action,
                                    truncate_chars(&s.arguments.to_string(), 120),
                                    truncate_chars(&s.result.to_string(), 160),
                                ));
                            } else {
                                failed_desc.push_str(&format!(
                                    "\n- id={} action={} error={}",
                                    node.id,
                                    s.action,
                                    truncate_chars(&s.result.to_string(), 300),
                                ));
                            }
                        }
                        tracing::warn!(
                            %run_id, attempt = replans_used,
                            max = resilience.max_replans,
                            "plan failed — requesting a REVISED plan from the model (bounded)"
                        );
                        replan_guidance = Some(format!(
                            "A previous plan for this task FAILED (re-plan attempt {} of {}). \
                         Failed node(s):{}\nNodes already completed successfully — their side \
                         effects are DONE; to let later nodes depend on one, include it again \
                         with EXACTLY the same id, action and arguments (it will NOT \
                         re-execute):{}\nPropose a REVISED plan that fixes or replaces the \
                         failed work. Do not redo completed work.",
                            replans_used,
                            resilience.max_replans,
                            if failed_desc.is_empty() {
                                "\n- (none recorded)".to_string()
                            } else {
                                failed_desc
                            },
                            if completed_desc.is_empty() {
                                "\n- (none)".to_string()
                            } else {
                                completed_desc
                            },
                        ));
                        continue 'generations;
                    }
                    let err = if replans_used > 0 {
                        format!("{err} ({replans_used} re-plan(s) exhausted)")
                    } else {
                        err
                    };
                    return self
                        .finalize_run(run, RunStatus::Failed, None, Some(err), seq)
                        .await;
                }
            }
        } // 'generations
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
        // Idempotent by node index: a durable ask-pause (ADR 0025) persists
        // `run.steps` before suspending, and the resume fast-path re-applies
        // each journaled receipt — without this guard a drained sibling would
        // be recorded twice across the pause. Node indices are unique, so
        // skipping an already-present index is exactly-once, not lossy.
        if run.steps.iter().any(|s| s.index == step) {
            return Ok(());
        }
        run.steps.push(StepRecord {
            index: step,
            // A review node dispatches nothing whatever `action` it carries,
            // so it is always recorded under the kernel-owned name — a review
            // must never pass for an executed tool (ADR 0033).
            action: if node.node_type == crate::plan::NodeType::Review {
                outcome::REVIEW_STEP_ACTION.to_string()
            } else {
                node.action
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", node.node_type).to_lowercase())
            },
            arguments: node.arguments.clone(),
            result,
            ok,
        });
        self.store.put(run.clone()).await?;
        Ok(())
    }
}

/// One plan node's outcome from the durable intent→execute→receipt tail
/// (ADR 0022), returned to the single orchestrator (`Kernel::execute_plan`)
/// — the only writer of `run`/`node_outputs` — which applies it via
/// `record_node_step` in completion order, so no locking is needed there.
struct PlanNodeOutcome {
    node_index: u32,
    result: serde_json::Value,
    ok: bool,
    agent_usage: Vec<(String, u64)>,
    /// Model-call usage — only a review node's `review_verdict` call
    /// produces this; `None` for tool/a2a nodes.
    usage: Option<Usage>,
}

/// The durable intent→execute→receipt tail for one plan node (ADR 0022,
/// Part 2), split out of the orchestrator so it can run inside
/// `tokio::spawn` — concurrently with sibling nodes whose dependencies are
/// also already satisfied — while `Kernel::execute_plan` stays the sole
/// writer of `run`/`node_outputs`. Takes no `&mut Run`: owned/`Arc` clones
/// only, so the future is `'static` as `tokio::spawn` requires.
///
/// Mirrors the former `execute_plan_node`'s per-node-type logic exactly
/// (review vs tool/a2a; idempotency; A2A lineage injection) — the only
/// structural difference is that bookkeeping which needs `&mut Run`
/// (`StepRecord` append, usage/agent_usage aggregation) is returned via
/// [`PlanNodeOutcome`] instead of applied here.
#[allow(clippy::too_many_arguments)]
async fn dispatch_plan_node_execution(
    run_id: Uuid,
    step: u32,
    node: crate::plan::PlanNode,
    deps_context: Vec<serde_json::Value>,
    folded: crate::journal::StepJournal,
    schema: ActionSchema,
    active_skills: Vec<String>,
    a2a_depth: u32,
    a2a_ancestry: Vec<String>,
    latitude: Latitude,
    store: Arc<dyn RunStore>,
    tools: Arc<dyn ActionSource>,
    oracle: Arc<dyn PolicyOracle>,
    events: broadcast::Sender<RunEventEnvelope>,
) -> Result<PlanNodeOutcome, KernelError> {
    // Build the node's validated action. Every node — tool, a2a, review —
    // goes through kernel-side schema validation (governance is NOT
    // bypassed by planning).
    if node.node_type == crate::plan::NodeType::Review {
        // A review node is a kernel-owned decision. It calls the oracle
        // (memoized via the receipt) for an approve/reject verdict over its
        // upstream outputs.
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
                serde_json::to_string(&deps_context).unwrap_or_default()
            ),
            steps: Vec::new(),
            memory: Vec::new(),
            last_error: None,
            latitude,
            guidance: Some(
                "Call review_verdict with approve=true only if the results look correct.".into(),
            ),
            active_skills,
        };
        let proposal = oracle
            .propose(&context, &review_schema)
            .await
            .map_err(|e| ExecutionError::Tool(format!("review failed: {e}")))?;
        let validated = review_schema
            .validate(proposal.action)
            .map_err(|e| ExecutionError::Tool(format!("review verdict invalid: {e}")))?;
        let approved = validated.arguments()["approve"].as_bool().unwrap_or(false);
        // A review node has no external side effect: journal its verdict
        // directly as the receipt (memoized decision).
        let result = validated.arguments().clone();
        store
            .append_journal(
                run_id,
                JournalEntry::Receipt {
                    step,
                    ok: approved,
                    result: result.clone(),
                },
            )
            .await?;
        return Ok(PlanNodeOutcome {
            node_index: step,
            result,
            ok: approved,
            agent_usage: Vec::new(),
            usage: proposal.usage,
        });
    }

    // Tool / A2a node — the SAME per-action seam as step mode.
    let action = node.action.clone().unwrap_or_default();
    let proposed = crate::action::ProposedAction {
        action: action.clone(),
        arguments: node.arguments.clone(),
        rationale: Some(format!("plan node '{}'", node.id)),
    };
    let validated = schema.validate(proposed).map_err(|e| {
        // Should not happen (plan validation checked existence), but
        // arguments are validated here per-action.
        ExecutionError::Tool(format!("node '{}' action invalid: {e}", node.id))
    })?;

    // Durable intent → execute → receipt (identical contract to step mode;
    // idempotency + A2A reconnect apply).
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
                    node.id, action
                );
                store
                    .append_journal(
                        run_id,
                        JournalEntry::Receipt {
                            step,
                            ok: false,
                            result: serde_json::json!({ "error": reason }),
                        },
                    )
                    .await?;
                return Ok(PlanNodeOutcome {
                    node_index: step,
                    result: serde_json::json!({ "error": "manual check required" }),
                    ok: false,
                    agent_usage: Vec::new(),
                    usage: None,
                });
            }
            tracing::info!(
                %run_id, node = %node.id,
                "resume: dangling node intent — re-dispatching with the journaled key"
            );
            journaled_key
        }
        None => {
            let key = format!("{run_id}:plan:{step}");
            store
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
    // Lineage injection (ADR 0022): recomputed fresh here, unconditionally,
    // for every A2A dispatch — a pure function of the run's own (fixed)
    // depth/ancestry and the target's origin. Correct for both a brand-new
    // dispatch and a dangling-intent reconnect: the admission loop already
    // decided whether this attempt is ALLOWED before spawning this task;
    // this only computes the VALUES the a2a crate embeds in a fresh
    // outbound `SendMessage` — never read on a reconnect (which polls an
    // existing remote task instead of sending a new message).
    if node.node_type == crate::plan::NodeType::A2a {
        if let Some(origin) = tools.a2a_origin(validated.action()) {
            let mut ancestry = a2a_ancestry;
            ancestry.push(origin);
            exec_action.inject_a2a_lineage(a2a_depth + 1, &ancestry);
        }
    }

    // Streamed delegation progress (ADR 0027): a plan A2A node streams under
    // its own plan branch (`node.id`), so a client can lane the progress into
    // the right fan-out swimlane.
    let exec_ctx = ExecutionContext::new(run_id, step, store.clone(), folded.notes.clone())
        .with_progress(events, Some(node.id.clone()));
    // E3: mask secret-shaped runs before the result becomes a durable
    // Receipt/StepRecord — same seam as step mode.
    let (result, ok) = match tools.execute(&exec_action, &exec_ctx).await {
        Ok(v) => (crate::redact::scrub_json(&v), true),
        Err(e) => (
            serde_json::json!({ "error": crate::redact::scrub_secrets(&e.to_string()) }),
            false,
        ),
    };

    crash_point("after_execute", validated.action());

    let agent_usage = exec_ctx.agent_usage.into_inner().unwrap_or_default();

    store
        .append_journal(
            run_id,
            JournalEntry::Receipt {
                step,
                ok,
                result: result.clone(),
            },
        )
        .await?;

    Ok(PlanNodeOutcome {
        node_index: step,
        result,
        ok,
        agent_usage,
        usage: None,
    })
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

/// The kernel-owned self-activation action (ADR 0020), appended alongside
/// `finish` only when a `SkillGate` is attached and reports something
/// activatable.
fn activate_skill_spec() -> ActionSpec {
    ActionSpec {
        name: ACTIVATE_SKILL_ACTION.into(),
        description: "Load a listed skill's full instructions into context for the rest of \
                      this run. Advisory only — it never changes which actions you may take; \
                      every proposal is still validated the same way."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact skill name as it appears in the catalog."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

/// The kernel-owned counterpart to [`activate_skill_spec`]: drop a run-scoped
/// skill's instructions from context. Needs no gate — always safe.
fn deactivate_skill_spec() -> ActionSpec {
    ActionSpec {
        name: DEACTIVATE_SKILL_ACTION.into(),
        description: "Stop loading a previously self-activated skill's instructions for the \
                      rest of this run."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact skill name to deactivate."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}
