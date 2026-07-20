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
 * Honest scope (design/07 D-1): the journal holds decisions/receipts/terminal,
 * not rejected proposals or rationale — those are live-WS-only. A replayed
 * axis therefore has proposals + results but no retry detail. */

import type {
  JournalEntry,
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
      // Keyed dedupe: replay + live can both deliver; attempts are unique.
      if (!s.rejections.some((r) => r.attempt === ev.attempt)) {
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
    case "completed":
      axis.terminal = { kind: "completed", output: ev.output };
      break;
    case "failed":
      axis.terminal = { kind: "failed", error: ev.error };
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
      } else if (entry.status === "canceled") {
        axis.terminal = { kind: "canceled", error: entry.error ?? null };
      }
      break;
    // intent / note / trust_posture / plan_proposed: durable bookkeeping the
    // axis does not render (design/03 R-3).
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

// ---- the bucketed event store ---------------------------------------------

type Listener = () => void;

const buckets = new Map<string, AxisState>();
/* Out-of-order guard: per-run pending envelopes with seq gaps are rare (one
 * broadcast channel), but sorting-by-seq on insert keeps the axis correct. */
const pending = new Map<string, RunEventEnvelope[]>();
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

/* Feed a live WS envelope: bucket by run_id, order by seq, fold, notify. */
export function ingestEnvelope(env: RunEventEnvelope): void {
  const axis = axisFor(env.run_id);
  const queue = pending.get(env.run_id) ?? [];
  queue.push(env);
  queue.sort((a, b) => a.seq - b.seq);
  // Apply everything not yet applied, in seq order. Duplicates (replayed
  // after reconnect) are skipped by seq; folding is keyed so re-application
  // would be harmless anyway.
  const rest: RunEventEnvelope[] = [];
  for (const e of queue) {
    if (e.seq <= axis.lastSeq) continue;
    axis.lastSeq = e.seq;
    applyEvent(axis, e.event);
  }
  pending.set(env.run_id, rest);
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
  pending.delete(runId);
}
