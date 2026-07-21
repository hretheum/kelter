/* Thin stateless client: the kernel owns all state; this app only renders it.
 * Works same-origin (served by the daemon) or against a remote kernel URL.
 * Behavior-preserving port of the previous vanilla api() (design/06 §4):
 * Bearer header, 204 → null, err.status set so callers branch (501/401/429). */

export interface ApiError extends Error {
  status: number;
}

const conn = {
  base: localStorage.getItem("ak.base") || "",
  token: localStorage.getItem("ak.token") || "",
};

// The desktop shell (and pairing links) pass the token in the URL fragment:
// http://127.0.0.1:PORT/#token=... — the fragment never reaches the server.
// A fragment token implies the launcher provisioned the connection
// (same-origin), so base resets (design/02-flows/01).
const hashToken = location.hash.match(/token=([^&]+)/);
if (hashToken) {
  conn.token = decodeURIComponent(hashToken[1]);
  conn.base = "";
}

export const hasProvisionedToken = Boolean(hashToken);

export function connection() {
  return { ...conn };
}

export function setConnection(base: string, token: string) {
  conn.base = base.trim().replace(/\/$/, "");
  conn.token = token.trim();
  localStorage.setItem("ak.base", conn.base);
  localStorage.setItem("ak.token", conn.token);
}

export function apiUrl(path: string): string {
  return (conn.base || "") + path;
}

export function wsUrl(path: string): string {
  const wsBase = (conn.base || location.origin).replace(/^http/, "ws");
  // Browsers cannot set Authorization headers on WebSocket — token in query.
  return `${wsBase}${path}?token=${encodeURIComponent(conn.token)}`;
}

export async function api<T = unknown>(
  path: string,
  opts: RequestInit = {}
): Promise<T> {
  const res = await fetch(apiUrl(path), {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${conn.token}`,
      ...(opts.headers || {}),
    },
  });
  if (!res.ok) {
    const err = new Error(`${res.status} ${await res.text()}`) as ApiError;
    err.status = res.status; // callers branch on it (e.g. 501 = not mutable)
    throw err;
  }
  if (res.status === 204) return null as T; // no body (e.g. key stored)
  return res.json() as Promise<T>;
}

// ---- typed views of the kernel contract (crates/api) ----------------------

export type RunStatus =
  | "pending"
  | "running"
  | "awaiting_skill_decision"
  | "awaiting_delegation_decision"
  | "awaiting_delegation_input"
  | "completed"
  | "failed"
  | "canceled";

export interface StepRecord {
  index: number;
  action: string;
  arguments: unknown;
  result: unknown;
  ok: boolean;
}

export interface Run {
  id: string;
  task: string;
  status: RunStatus;
  steps: StepRecord[];
  output: unknown;
  error: string | null;
  policy: string;
  created_at: number;
  cost_usd: number | null;
  active_skills: string[];
}

/* A human's answer to a paused activate_skill request (ADR 0021). Deny/allow
 * are both non-executions handled identically by the kernel today — only
 * allow_always additionally elevates the skill's trust label persistently. */
export type SkillActivationDecision = "deny" | "allow_once" | "allow_always";

export interface PendingSkillActivationUi {
  step: number;
  skill: string;
  description: string;
  trust: string;
  provenance_source_kind: string;
  provenance_source: string;
}

/* A human's answer to a paused delegate_to_X request (ADR 0023, mirrors
 * SkillActivationDecision field-for-field). Unlike skill activation, this
 * authorizes a REAL action — sending work to a remote agent — not advice;
 * deny/allow_once are both non-dispatches, allow_always additionally
 * elevates the target agent's trust label persistently. */
export type DelegationDecision = "deny" | "allow_once" | "allow_always";

export interface PendingDelegationUi {
  step: number;
  agent: string;
  description: string;
  trust: string;
  provenance_source_kind: string;
  provenance_source: string;
  // Plan node id (P1, ADR 0022) if this pause occurred inside a fan-out
  // branch; null for an ordinary step-mode pause.
  branch: string | null;
}

/* An interactive-delegation input request (ADR 0026): an AUTHORIZED delegation
 * whose remote task entered `input-required` and is paused for the human's
 * continuation text. Distinct from PendingDelegationUi (which gates whether to
 * dial at all) — here the dial already happened and the remote asked back. */
export interface PendingDelegationInputUi {
  step: number;
  agent: string;
  task_id: string;
  prompt: string;
  branch: string | null;
}

export type RunEvent =
  | { type: "created"; task: string; policy: string }
  | {
      type: "proposed";
      step: number;
      action: string;
      arguments: unknown;
      rationale: string | null;
      // Plan node id for a parallel-plan-fanout node (P1, ADR 0022); `null`
      // for ordinary step-mode events — today's single-lane behavior.
      branch: string | null;
    }
  | { type: "rejected"; step: number; reason: string; attempt: number; branch: string | null }
  | {
      type: "executed";
      step: number;
      action: string;
      ok: boolean;
      result: unknown;
      branch: string | null;
    }
  | { type: "reflection_written"; episode_id: string }
  | { type: "completed"; output: unknown }
  | { type: "failed"; error: string }
  | ({ type: "skill_activation_requested" } & PendingSkillActivationUi)
  | {
      type: "skill_activation_decided";
      step: number;
      skill: string;
      decision: SkillActivationDecision;
      principal: string;
    }
  | ({ type: "agent_delegation_requested" } & PendingDelegationUi)
  | {
      type: "agent_delegation_decided";
      step: number;
      agent: string;
      decision: DelegationDecision;
      principal: string;
      branch: string | null;
    }
  | ({ type: "agent_delegation_input_required" } & PendingDelegationInputUi)
  | {
      type: "agent_delegation_input_provided";
      step: number;
      agent: string;
      principal: string;
      branch: string | null;
    }
  // Streamed progress from a running remote delegation (ADR 0027). Advisory
  // observation only — never a validated action. Rides envelope `seq` 0
  // (off the ordered stream); clients order/dedupe by `ordinal` within a step.
  | {
      type: "agent_delegation_progress";
      step: number;
      agent: string;
      task_id: string;
      state: string;
      note: string;
      ordinal: number;
      branch: string | null;
    };

export interface RunEventEnvelope {
  run_id: string;
  seq: number;
  event: RunEvent;
}

/* Skills CRUD (ADR 0021 Part 6 addendum). Literal snake_case field names,
 * same convention as everything else in this file. */
export interface SkillView {
  name: string;
  description: string;
  trust: string;
  provenance_source_kind: string;
  provenance_source: string;
  active: boolean; // operator-active (the registry's persistent active set — NOT run-scoped)
  has_scripts: boolean;
  has_extension_manifest: boolean;
}

export interface SkillInstallReport {
  installed: string[];
  skipped: [string, string][]; // [name, reason][] — a Rust tuple serializes as a 2-element array
}

export interface SkillsPolicyView {
  model_activation: string; // "trusted" | "all" | "ask"
  ask_fallback: string; // "deny" | "queue" — only consulted when model_activation == "ask"
  ask_timeout_secs: number;
}

export interface SkillsPolicyUpdate {
  model_activation: string;
  ask_fallback?: string;
  ask_timeout_secs?: number;
}

/* Agent trust/policy CRUD (ADR 0023 Part 3). Literal snake_case field names,
 * same convention as SkillView. No "install a local agent" concept — a
 * network agent only ever registers by card URL. */
export interface AgentView {
  name: string;
  description: string;
  trust: string;
  source_kind: string;
  source: string;
  registered_at: number;
}

export interface AgentRegisterRequest {
  card_url: string;
  token_env?: string;
}

export interface AgentPolicyView {
  policy: string; // "trusted" | "all" | "ask" — mirrors SkillsPolicyView.model_activation's wire values
  ask_fallback: string; // "deny" | "queue" — only consulted when policy == "ask"
  ask_timeout_secs: number;
}

export interface AgentPolicyUpdate {
  policy: string;
  ask_fallback?: string;
  ask_timeout_secs?: number;
}

export interface ModelConfigView {
  engine: string;
  base_url: string | null;
  provider: string;
  model: string;
  api_key_set: boolean;
}

export interface ModelTestResult {
  ok: boolean;
  detail: string;
}

/* Journal replay (D-1, GET /runs/{id}/events). Entries are the kernel's
 * JournalEntry, kind-tagged. */
export interface JournalReplay {
  run_id: string;
  task: string;
  policy: string;
  status: RunStatus;
  journal: JournalEntry[];
}

export type JournalEntry =
  | {
      kind: "decision";
      step: number;
      action: string;
      arguments: unknown;
    }
  | {
      kind: "intent";
      step: number;
      action: string;
      arguments: unknown;
      idempotency_key: string;
    }
  | { kind: "note"; step: number; key: string; value: unknown }
  | { kind: "receipt"; step: number; ok: boolean; result: unknown }
  | { kind: "trust_posture"; entries: [string, string][] }
  | { kind: "plan_proposed"; plan: unknown }
  | {
      kind: "terminal";
      status: RunStatus;
      output?: unknown;
      error?: string | null;
    };
