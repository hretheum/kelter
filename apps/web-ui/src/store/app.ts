/* Central app store — deliberately tiny (design/06 §1: the kernel owns all
 * state; this is a thin view-model over the API). useSyncExternalStore-style:
 * one mutable snapshot object, one listener set, immutable-ish updates via
 * replace-and-notify. */

import {
  api,
  connection,
  hasProvisionedToken,
  setConnection,
  wsUrl,
  type ApiError,
  type ModelConfigView,
  type Run,
  type RunEventEnvelope,
} from "@/lib/api";
import { ingestEnvelope, ingestReplay, type AxisState, axisFor } from "@/lib/events";
import type { JournalReplay } from "@/lib/api";

export type ConnState = "disconnected" | "connecting" | "connected" | "auth-failed";
export type View = "runs" | "settings";
export type ConfigStatus = "loading" | "editable" | "readonly" | "unknown";

export interface AppState {
  conn: ConnState;
  connError: string | null; // 401 vs 429 vs network — distinct copy (flow 07)
  sheetOpen: boolean;
  view: View;
  runs: Run[];
  runsLoaded: boolean;
  /* null → composer face; an id → that run's timeline/result face. */
  selectedRunId: string | null;
  config: { status: ConfigStatus; view: ModelConfigView | null };
  /* Capability flags (design/06 §1a): flip off on 404/405 so Stop / full
   * replay are HIDDEN on an older daemon, never dead. */
  cancelSupported: boolean;
  replaySupported: boolean;
  tools: { name: string; description: string }[];
}

let state: AppState = {
  conn: "disconnected",
  connError: null,
  sheetOpen: !hasProvisionedToken && !connection().token,
  view: "runs",
  runs: [],
  runsLoaded: false,
  selectedRunId: null,
  config: { status: "loading", view: null },
  cancelSupported: true,
  replaySupported: true,
  tools: [],
};

const listeners = new Set<() => void>();

export function getState(): AppState {
  return state;
}

export function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

function set(patch: Partial<AppState>) {
  state = { ...state, ...patch };
  listeners.forEach((l) => l());
}

// ---- connection lifecycle (design/02-flows/07) ----------------------------

let ws: WebSocket | null = null;
let retryTimer: number | null = null;
let retryDelay = 1000;
const RETRY_MAX = 10_000;
let intentionallyClosed = false;

function scheduleRetry() {
  if (retryTimer != null || state.conn === "auth-failed") return;
  retryTimer = window.setTimeout(() => {
    retryTimer = null;
    void connect();
  }, retryDelay);
  retryDelay = Math.min(retryDelay * 2, RETRY_MAX);
}

function connectWs() {
  if (ws) {
    intentionallyClosed = true;
    ws.close();
  }
  intentionallyClosed = false;
  const socket = new WebSocket(wsUrl("/api/v1/events"));
  socket.onopen = () => {
    retryDelay = 1000;
    set({ conn: "connected", connError: null });
  };
  socket.onclose = () => {
    if (intentionallyClosed) return;
    if (state.conn !== "auth-failed") {
      set({ conn: "disconnected" });
      scheduleRetry();
    }
  };
  socket.onmessage = (msg) => {
    const env = JSON.parse(msg.data) as RunEventEnvelope;
    ingestEnvelope(env);
    if (["created", "completed", "failed"].includes(env.event.type)) {
      void refreshRuns();
    }
  };
  ws = socket;
}

/* Connect: REST probe → WS → first-run routing (design/02-flows/01). */
export async function connect(): Promise<void> {
  set({ conn: "connecting", connError: null });
  try {
    await refreshRuns();
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 401) {
      set({ conn: "auth-failed", connError: "Token rejected. Check your API token.", sheetOpen: true });
    } else if (err.status === 429) {
      set({ conn: "auth-failed", connError: "Too many attempts. Wait a moment and try again.", sheetOpen: true });
    } else {
      set({ conn: "disconnected", connError: `Can't reach the server. ${err.message ?? ""}` });
      scheduleRetry();
    }
    return;
  }
  connectWs();
  set({ conn: "connected", sheetOpen: false });
  void routeByModelHealth();
  void loadTools();
}

export function reconnectWith(base: string, token: string) {
  setConnection(base, token);
  retryDelay = 1000;
  set({ conn: "disconnected", connError: null });
  void connect();
}

export function openSheet(open: boolean) {
  set({ sheetOpen: open });
}

// ---- runs -----------------------------------------------------------------

export async function refreshRuns(): Promise<void> {
  const runs = await api<Run[]>("/api/v1/runs");
  set({ runs: runs.slice().reverse(), runsLoaded: true }); // newest first
}

export function selectRun(id: string | null) {
  set({ selectedRunId: id, view: "runs" });
  if (id) void hydrateRun(id);
}

/* Selecting a run from history: fetch the durable Run and (D-1) the journal
 * replay for the full axis; 404/405 on replay → capability off + steps[]
 * fallback (design/03 R-4). */
async function hydrateRun(id: string): Promise<void> {
  try {
    const run = await api<Run>(`/api/v1/runs/${id}`);
    upsertRun(run);
    if (state.replaySupported) {
      try {
        const replay = await api<JournalReplay>(`/api/v1/runs/${id}/events`);
        ingestReplay(id, { task: replay.task, policy: replay.policy }, replay.journal);
      } catch (e) {
        const err = e as ApiError;
        if (err.status === 404 || err.status === 405) {
          // Older daemon without D-1 — hide full replay for the session and
          // fall back to the executed record from steps[].
          set({ replaySupported: false });
          replayFromSteps(run);
        }
      }
    } else {
      replayFromSteps(run);
    }
  } catch {
    /* run vanished (pruned) — the list refresh will reconcile */
  }
}

function replayFromSteps(run: Run) {
  ingestReplay(
    run.id,
    { task: run.task, policy: run.policy },
    run.steps.map((s) => [
      { kind: "decision" as const, step: s.index, action: s.action, arguments: s.arguments },
      { kind: "receipt" as const, step: s.index, ok: s.ok, result: s.result },
    ]).flat()
  );
  const axis = axisFor(run.id);
  if (run.status === "completed" && !axis.terminal) {
    ingestReplay(run.id, { task: run.task, policy: run.policy }, [
      { kind: "terminal", status: "completed", output: run.output ?? null },
    ]);
  } else if (run.status === "failed" && !axis.terminal) {
    ingestReplay(run.id, { task: run.task, policy: run.policy }, [
      { kind: "terminal", status: "failed", error: run.error ?? "failed" },
    ]);
  } else if (run.status === "canceled" && !axis.terminal) {
    ingestReplay(run.id, { task: run.task, policy: run.policy }, [
      { kind: "terminal", status: "canceled", error: run.error ?? null },
    ]);
  }
}

function upsertRun(run: Run) {
  const idx = state.runs.findIndex((r) => r.id === run.id);
  const runs = state.runs.slice();
  if (idx >= 0) runs[idx] = run;
  else runs.unshift(run);
  set({ runs });
}

export async function submitRun(task: string): Promise<string> {
  const res = await api<{ run_id: string }>("/api/v1/runs", {
    method: "POST",
    body: JSON.stringify({ task }),
  });
  set({ selectedRunId: res.run_id, view: "runs" });
  void refreshRuns();
  return res.run_id;
}

/* Stop (D-2). 404/405 → daemon without the endpoint: flip the capability off
 * (button disappears) and rethrow so the caller can explain. */
export async function cancelRun(id: string): Promise<Run> {
  try {
    const run = await api<Run>(`/api/v1/runs/${id}/cancel`, { method: "POST" });
    upsertRun(run);
    // The kernel emits no WS event for cancel — reflect it on the axis now.
    ingestReplay(id, { task: run.task, policy: run.policy }, [
      { kind: "terminal", status: "canceled", error: run.error ?? null },
    ]);
    return run;
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 404 || err.status === 405) set({ cancelSupported: false });
    throw e;
  }
}

// ---- model config (design/02-flows/02) ------------------------------------

export async function loadConfig(): Promise<void> {
  try {
    const view = await api<ModelConfigView>("/api/v1/config/model");
    set({ config: { status: "editable", view } });
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 501) {
      set({ config: { status: "readonly", view: null } });
    } else {
      set({ config: { status: "unknown", view: null } });
    }
  }
}

/* First-run routing (flow 01): model reachable → Runs; 501 → Runs (read-only
 * config); otherwise → Settings with the "point Kelter at a model" framing. */
async function routeByModelHealth(): Promise<void> {
  await loadConfig();
  const cfg = state.config;
  if (cfg.status === "readonly") {
    set({ view: "runs" });
    return;
  }
  if (cfg.status !== "editable" || !cfg.view) {
    set({ view: "settings" });
    return;
  }
  try {
    const res = await api<{ ok: boolean }>("/api/v1/config/model/test", {
      method: "POST",
      body: JSON.stringify({
        provider: cfg.view.provider || "local",
        base_url: cfg.view.base_url ?? "",
      }),
    });
    set({ view: res.ok ? "runs" : "settings" });
  } catch {
    set({ view: "settings" });
  }
}

export function showSettings(show: boolean) {
  set({ view: show ? "settings" : "runs" });
}

async function loadTools(): Promise<void> {
  try {
    const res = await api<{ tools: { name: string; description: string }[] }>(
      "/api/v1/tools"
    );
    set({ tools: res.tools });
  } catch {
    /* fail-silent (design/03 R-2): the hint is optional, never blocking */
  }
}

// ---- boot -----------------------------------------------------------------

export function boot() {
  if (connection().token) void connect();
}

export type { AxisState };
