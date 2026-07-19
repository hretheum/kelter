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
}

export type RunEvent =
  | { type: "created"; task: string; policy: string }
  | {
      type: "proposed";
      step: number;
      action: string;
      arguments: unknown;
      rationale: string | null;
    }
  | { type: "rejected"; step: number; reason: string; attempt: number }
  | {
      type: "executed";
      step: number;
      action: string;
      ok: boolean;
      result: unknown;
    }
  | { type: "reflection_written"; episode_id: string }
  | { type: "completed"; output: unknown }
  | { type: "failed"; error: string };

export interface RunEventEnvelope {
  run_id: string;
  seq: number;
  event: RunEvent;
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
