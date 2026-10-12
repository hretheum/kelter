/* The execution-axis engine (design/00 §3.2, 06 §4 — the structural piece the
 * old single-<pre> UI lacked).
 *
 * The kernel's WS is a GLOBAL broadcast of every run's events. This module
 * buckets envelopes by run_id, orders by seq, and folds each run's events into
 * a step-keyed AxisState the timeline renders. The same reducer is fed from
 * the journal replay (D-1, GET /runs/{id}/events), so live watching, reconnect
 * gap-fill, and historical viewing share ONE code path — merging is idempotent
 * because everything is keyed by step index.
 *
 * Honest scope (design/07 D-1): the journal holds decisions/receipts/terminal
 * and, since ADR 0036, refusals — so a replayed axis has proposals, results
 * and retry detail. Rationale stays live-WS-only. */

import type {
  AgentView,
  JournalEntry,
  PendingActionApprovalUi,
  PendingDelegationInputUi,
  PendingDelegationUi,
  PendingSkillActivationUi,
  RunEvent,
  RunEventEnvelope,
} from "./api";

export interface AxisStep {
  step: number;
  action: string;
  arguments: unknown;
  rationale: string | null;
  rejections: { reason: string; attempt: number }[];
  executed: { ok: boolean; result: unknown } | null;
  /* Streamed remote-delegation progress (ADR 0027): advisory intermediate
   * updates from a running A2A step, ordered by `ordinal`. Live-only — the
   * journal stores these as notes the replay path does not render, so a
   * reconnect may show fewer lines (disclosed best-effort, design/08). */
  progress: { ordinal: number; state: string; note: string; agent: string }[];
  /* Plan node id for a parallel-plan-fanout node (P1, ADR 0022); `null` for
   * an ordinary step-mode step — today's single-lane behavior, unchanged. */
  branch: string | null;
  /* Client wall-clock time this step was first observed (design/03 R-3
   * parallel variant "elapsed time" for a still-running lane — remote_ms is
   * only known once a receipt lands, so a running lane's elapsed reading is
   * honestly sourced from local time, same technique as the run header's
   * own elapsed timer). */
  receivedAt: number;
}

export interface AxisState {
  runId: string;
  created: { task: string; policy: string } | null;
  steps: Map<number, AxisStep>;
  terminal:
    | { kind: "completed"; output: unknown }
    | { kind: "failed"; error: string }
    | { kind: "incomplete"; output: unknown; reason: string }
    | { kind: "canceled"; error: string | null }
    | null;
  reflections: number;
  /* True when any part came from journal replay (retry/rationale detail may
   * be missing — the UI shows the honesty note when replayOnly). */
  fromReplay: boolean;
  /* Highest live seq applied — replayed journal entries carry no seq. */
  lastSeq: number;
  /* Set by skill_activation_requested, cleared by the paired
   * skill_activation_decided (ADR 0021) — not step-shaped, so it lives
   * top-level rather than on an AxisStep. */
  pendingSkillActivation: PendingSkillActivationUi | null;
  /* Set by agent_delegation_requested, cleared by the paired
   * agent_delegation_decided (ADR 0023) — mirrors pendingSkillActivation's
   * shape exactly; a paused delegation is a real authorization gate, not
   * advice, but the pause mechanics are identical. */
  pendingDelegation: PendingDelegationUi | null;
  /* Set by agent_delegation_input_required, cleared by the paired
   * agent_delegation_input_provided (ADR 0026) — an AUTHORIZED delegation
   * whose remote asked for more input. Separate from pendingDelegation: the
   * dial already happened, this is the remote's follow-up. */
  pendingDelegationInput: PendingDelegationInputUi | null;
  /* Set by action_approval_requested, cleared by the paired
   * action_approval_decided (ADR 0038) — a mutating action waiting on a
   * human; nothing has executed. */
  pendingActionApproval: PendingActionApprovalUi | null;
}

function newAxis(runId: string): AxisState {
  return {
    runId,
    created: null,
    steps: new Map(),
    terminal: null,
    reflections: 0,
    fromReplay: false,
    lastSeq: -1,
    pendingSkillActivation: null,
    pendingDelegation: null,
    pendingDelegationInput: null,
    pendingActionApproval: null,
  };
}

function stepOf(axis: AxisState, step: number): AxisStep {
  let s = axis.steps.get(step);
  if (!s) {
    s = {
      step,
      action: "",
      arguments: null,
      rationale: null,
      rejections: [],
      executed: null,
      progress: [],
      branch: null,
      receivedAt: Date.now(),
    };
    axis.steps.set(step, s);
  }
  return s;
}

export function applyEvent(axis: AxisState, ev: RunEvent): void {
  switch (ev.type) {
    case "created":
      axis.created = { task: ev.task, policy: ev.policy };
      break;
    case "proposed": {
      const s = stepOf(axis, ev.step);
      s.action = ev.action;
      s.arguments = ev.arguments;
      if (ev.rationale) s.rationale = ev.rationale;
      s.branch = ev.branch;
      break;
    }
    case "rejected": {
      const s = stepOf(axis, ev.step);
      // Keyed dedupe: replay + live can both deliver the identical event.
      // `attempt` alone is NOT unique per step (ADR 0023 P4): the governed
      // resilience loop's retry phase and fallback phase each number their
      // own attempts from 1, so `attempt: 1` legitimately appears twice for
      // one step (once per phase) — deduping on attempt alone would silently
      // drop the second phase's real, distinct rejection. `reason` differs
      // by phase (verified against crates/kernel-core/src/engine.rs's
      // actual copy), so attempt+reason is the correct dedupe key.
      const dup = s.rejections.some((r) => r.attempt === ev.attempt && r.reason === ev.reason);
      if (!dup) {
        s.rejections.push({ reason: ev.reason, attempt: ev.attempt });
      }
      s.branch = ev.branch;
      break;
    }
    case "executed": {
      const s = stepOf(axis, ev.step);
      if (!s.action) s.action = ev.action;
      s.executed = { ok: ev.ok, result: ev.result };
      s.branch = ev.branch;
      break;
    }
    case "reflection_written":
      axis.reflections += 1;
      break;
    case "episode_revised":
      // ADR 0040: memory state only; the run's axis is unchanged.
      break;
    case "completed":
      axis.terminal = { kind: "completed", output: ev.output };
      break;
    case "failed":
      axis.terminal = { kind: "failed", error: ev.error };
      break;
    case "incomplete":
      axis.terminal = { kind: "incomplete", output: ev.output, reason: ev.reason };
      break;
    case "skill_activation_requested":
      axis.pendingSkillActivation = {
        step: ev.step,
        skill: ev.skill,
        description: ev.description,
        trust: ev.trust,
        provenance_source_kind: ev.provenance_source_kind,
        provenance_source: ev.provenance_source,
      };
      break;
    case "skill_activation_decided":
      if (axis.pendingSkillActivation?.skill === ev.skill) {
        axis.pendingSkillActivation = null;
      }
      break;
    case "agent_delegation_requested":
      axis.pendingDelegation = {
        step: ev.step,
        agent: ev.agent,
        description: ev.description,
        trust: ev.trust,
        provenance_source_kind: ev.provenance_source_kind,
        provenance_source: ev.provenance_source,
        branch: ev.branch,
      };
      break;
    case "agent_delegation_decided":
      if (axis.pendingDelegation?.agent === ev.agent) {
        axis.pendingDelegation = null;
      }
      break;
    case "action_approval_requested":
      axis.pendingActionApproval = {
        step: ev.step,
        action: ev.action,
        arguments: ev.arguments,
        branch: ev.branch,
        expires_at: ev.expires_at,
      };
      break;
    case "action_approval_decided":
      if (
        axis.pendingActionApproval?.step === ev.step &&
        axis.pendingActionApproval.action === ev.action
      ) {
        axis.pendingActionApproval = null;
      }
      break;
    // ADR 0039: the stop's reason arrives on the paired `failed`; the axis
    // keeps no separate state for it.
    case "budget_exceeded":
      break;
    case "agent_delegation_input_required":
      axis.pendingDelegationInput = {
        step: ev.step,
        agent: ev.agent,
        task_id: ev.task_id,
        prompt: ev.prompt,
        branch: ev.branch,
      };
      break;
    case "agent_delegation_input_provided":
      if (axis.pendingDelegationInput?.agent === ev.agent) {
        axis.pendingDelegationInput = null;
      }
      break;
    case "agent_delegation_progress": {
      const s = stepOf(axis, ev.step);
      if (ev.branch) s.branch = ev.branch;
      // Dedupe by ordinal (replay + live, or a duplicated broadcast, can
      // deliver the same update twice); keep ordered for rendering.
      if (!s.progress.some((p) => p.ordinal === ev.ordinal)) {
        s.progress.push({
          ordinal: ev.ordinal,
          state: ev.state,
          note: ev.note,
          agent: ev.agent,
        });
        s.progress.sort((a, b) => a.ordinal - b.ordinal);
      }
      break;
    }
  }
}

/* Fold one journal entry into the axis — the replay-side twin of applyEvent.
 * decision → the step's accepted proposal; receipt → its execution;
 * terminal → the end (incl. canceled, which has no RunEvent variant). */
export function applyJournalEntry(axis: AxisState, entry: JournalEntry): void {
  axis.fromReplay = true;
  switch (entry.kind) {
    case "decision": {
      const s = stepOf(axis, entry.step);
      s.action = entry.action;
      s.arguments = entry.arguments;
      break;
    }
    case "receipt": {
      const s = stepOf(axis, entry.step);
      s.executed = { ok: entry.ok, result: entry.result };
      break;
    }
    case "terminal":
      if (entry.status === "completed") {
        axis.terminal = { kind: "completed", output: entry.output ?? null };
      } else if (entry.status === "failed") {
        axis.terminal = { kind: "failed", error: entry.error ?? "failed" };
      } else if (entry.status === "incomplete") {
        axis.terminal = {
          kind: "incomplete",
          output: entry.output ?? null,
          reason: entry.error ?? "incomplete",
        };
      } else if (entry.status === "canceled") {
        axis.terminal = { kind: "canceled", error: entry.error ?? null };
      }
      break;
    case "rejected": {
      // ADR 0036: the journaled twin of the live `rejected` event — same
      // attempt+reason dedupe as applyEvent, so replay + live never double.
      // A refused proposal never becomes the step's action.
      const s = stepOf(axis, entry.step);
      const dup = s.rejections.some(
        (r) => r.attempt === entry.attempt && r.reason === entry.reason
      );
      if (!dup) s.rejections.push({ reason: entry.reason, attempt: entry.attempt });
      if (entry.branch) s.branch = entry.branch;
      break;
    }
    // ADR 0038: replayed in append order, so a decided request clears the
    // pending one exactly as the live pair does.
    case "action_approval_requested":
      axis.pendingActionApproval = {
        step: entry.step,
        action: entry.action,
        arguments: entry.arguments,
        branch: entry.branch ?? null,
        expires_at: null,
      };
      break;
    case "action_approval_decided":
      if (
        axis.pendingActionApproval?.step === entry.step &&
        axis.pendingActionApproval.action === entry.action
      ) {
        axis.pendingActionApproval = null;
      }
      break;
    // intent / note / trust_posture / plan_proposed / resumed: durable
    // bookkeeping the axis does not render (design/03 R-3).
    case "resumed":
    case "intent":
    case "note":
    case "trust_posture":
    case "plan_proposed":
      break;
  }
}

/* Ordered steps for rendering. */
export function axisSteps(axis: AxisState): AxisStep[] {
  return [...axis.steps.values()].sort((a, b) => a.step - b.step);
}

// ---- parallel plan fan-out (P1, ADR 0022, design/03 R-3 parallel variant) -

/* A plan node's id (`branch`) and its own index (`step`) are 1:1 — the
 * engine assigns one index per node, so each branch is exactly one
 * AxisStep, never a sub-sequence. "Lanes" are therefore just the
 * branch-tagged subset of axis.steps, in the plan's topological (step)
 * order — no separate bucketing structure is needed. */
export function axisBranches(axis: AxisState): AxisStep[] {
  return axisSteps(axis).filter((s) => s.branch !== null);
}

export function hasFanout(axis: AxisState): boolean {
  for (const s of axis.steps.values()) if (s.branch !== null) return true;
  return false;
}

/* Mirrors kernel-core's `is_a2a_action` (engine.rs) exactly: `delegate_to_*`
 * actions are A2A delegations, everything else is not. */
export function isA2aAction(action: string): boolean {
  return action.startsWith("delegate_to_");
}

/* Mirrors plan.rs's `REVIEW_ACTION` constant. A review node with no
 * explicit `action` in its plan falls back server-side to the node type's
 * name ("review") — both forms are treated as a review lane here. */
export type LaneKind = "a2a" | "review" | "tool";
export function laneKind(action: string): LaneKind {
  if (isA2aAction(action)) return "a2a";
  if (action === "review_verdict" || action === "review") return "review";
  return "tool";
}

/* Governance counter (design/03 R-3 parallel variant): count of
 * delegate_to_* branch steps that have REACHED a decision (executed or
 * rejected) — mirrors the kernel's own `authorize_delegation` counting
 * method (`run.steps.iter().filter(is_a2a_action)`, engine.rs), computed
 * client-side from the live axis rather than waiting on a Run refresh so it
 * updates in real time during a running fan-out. No "max" is shown: no HTTP
 * endpoint exposes `DelegationLimits.max_delegations_per_run` today
 * (grepped `crates/api/src/lib.rs` — absent) — showing a max would mean
 * either hardcoding a number that can drift from the daemon's real config,
 * or fabricating a denominator the UI can't verify. Both violate the
 * honesty convention (design/08), so this is deliberately "N so far", not
 * "N/max". */
export function countDelegationAttempts(axis: AxisState): number {
  let n = 0;
  for (const s of axis.steps.values()) {
    if (isA2aAction(s.action) && (s.executed !== null || s.rejections.length > 0)) n += 1;
  }
  return n;
}

/* Mirrors the a2a crate's `action_name` sanitization exactly
 * (crates/a2a/src/lib.rs: ASCII-alphanumeric lowercased, everything else
 * '_') — the only direction this mapping is safe to apply, since it is
 * lossy (distinct agent names can sanitize to the same string). Forward-only:
 * never attempt to reverse a `delegate_to_*` action back into an exact name. */
function sanitizeAgentName(name: string): string {
  return name
    .split("")
    .map((c) => (/[a-zA-Z0-9]/.test(c) ? c.toLowerCase() : "_"))
    .join("");
}

/* Real trust badge source for an a2a lane (ADR 0023): no trust field exists
 * on `proposed`/`executed` events (Part 5's contract only adds it to the
 * ask-pause events), so this joins the lane's `delegate_to_<sanitized>`
 * action against the registered-agents list (GET /api/v1/agents) by
 * sanitized-name match. `null` when no registered agent's sanitized name
 * matches — an honest "unknown", never a fabricated default. */
export function trustOfLaneAgent(action: string, agents: AgentView[]): string | null {
  const prefix = "delegate_to_";
  if (!action.startsWith(prefix)) return null;
  const suffix = action.slice(prefix.length);
  const match = agents.find((a) => sanitizeAgentName(a.name) === suffix);
  return match ? match.trust : null;
}

// ---- the bucketed event store ---------------------------------------------

type Listener = () => void;

const buckets = new Map<string, AxisState>();
const listeners = new Map<string, Set<Listener>>();
const globalListeners = new Set<(env: RunEventEnvelope) => void>();

export function axisFor(runId: string): AxisState {
  let axis = buckets.get(runId);
  if (!axis) {
    axis = newAxis(runId);
    buckets.set(runId, axis);
  }
  return axis;
}

function notify(runId: string) {
  listeners.get(runId)?.forEach((l) => l());
}

export function subscribeAxis(runId: string, l: Listener): () => void {
  let set = listeners.get(runId);
  if (!set) {
    set = new Set();
    listeners.set(runId, set);
  }
  set.add(l);
  return () => set.delete(l);
}

export function onAnyEvent(l: (env: RunEventEnvelope) => void): () => void {
  globalListeners.add(l);
  return () => globalListeners.delete(l);
}

/* Feed a live WS envelope: dedupe by seq, fold, notify.
 *
 * Forward-only by design (audit finding 3-b/#12). There is NO out-of-order
 * reorder buffer, and there must not be one: the kernel's per-run events go
 * out on a single broadcast channel that delivers them in seq order, and JS
 * ingest is single-threaded — so a live burst never arrives reordered. The
 * only real seq "gaps" come from events a client MISSED while disconnected;
 * those go to the durable journal and are re-supplied by the D-1 replay
 * (`ingestReplay`, keyed by step index), never re-sent live. A buffer that
 * blocked on a gap waiting for the missing seq would therefore DEADLOCK on
 * exactly that legitimate case (the missing seqs are never coming over the
 * wire) — so the earlier `pending`/`rest` reorder buffer was not only dead
 * code (`rest` was never populated) but would have been wrong to "fix" into a
 * blocking one. Ordering within the axis is anchored by step index in
 * `applyEvent`, not by seq; seq is used ONLY to dedupe re-delivered events
 * (the resume path re-emits from seq 1). */
export function ingestEnvelope(env: RunEventEnvelope): void {
  const axis = axisFor(env.run_id);
  // Streamed delegation progress (ADR 0027) rides seq 0 — advisory and OFF the
  // ordered stream, so it bypasses the dedupe gate (which would otherwise drop
  // every progress event once real events advanced lastSeq) and must NOT
  // advance lastSeq. applyEvent dedupes progress by ordinal, so re-delivery is
  // harmless.
  if (env.event.type === "agent_delegation_progress") {
    applyEvent(axis, env.event);
    notify(env.run_id);
    globalListeners.forEach((l) => l(env));
    return;
  }
  // Dedupe already-applied seqs (resume re-emits from 1); apply anything newer
  // immediately — a forward jump is a legitimate post-reconnect gap the replay
  // has already filled, never something to wait on.
  if (env.seq > axis.lastSeq) {
    axis.lastSeq = env.seq;
    applyEvent(axis, env.event);
  }
  notify(env.run_id);
  globalListeners.forEach((l) => l(env));
}

/* Feed a journal replay (D-1): rebuild/augment the axis in append order.
 * Idempotent with live events (keyed by step). */
export function ingestReplay(
  runId: string,
  info: { task: string; policy: string },
  journal: JournalEntry[]
): void {
  const axis = axisFor(runId);
  if (!axis.created) axis.created = { task: info.task, policy: info.policy };
  for (const entry of journal) applyJournalEntry(axis, entry);
  notify(runId);
}

/* Drop a bucket (e.g. after pruning); safe if absent. */
export function dropAxis(runId: string): void {
  buckets.delete(runId);
}

/* Test-only: reset the module-global bucket store so each test starts clean
 * (the axis store is a module singleton). Not used by the app. */
export function __resetAxesForTest(): void {
  buckets.clear();
  listeners.clear();
  globalListeners.clear();
}
