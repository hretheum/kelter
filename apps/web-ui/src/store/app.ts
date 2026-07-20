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
  type AgentPolicyUpdate,
  type AgentPolicyView,
  type AgentRegisterRequest,
  type AgentView,
  type ApiError,
  type DelegationDecision,
  type ModelConfigView,
  type Run,
  type RunEventEnvelope,
  type SkillActivationDecision,
  type SkillInstallReport,
  type SkillsPolicyUpdate,
  type SkillsPolicyView,
  type SkillView,
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
  /* ADR 0021: flips off on 404/405 from the skill-activation decide endpoint
   * — hides the inline prompt's buttons on a daemon without it. */
  skillActivationSupported: boolean;
  /* ADR 0021 Part 6: flips off on 404/405 from any /api/v1/skills route —
   * independent of skillActivationSupported (a deployment can have one
   * without the other during rollout) — hides the whole Settings section. */
  skillsManagementSupported: boolean;
  /* ADR 0023: flips off on 404/405 from the delegation-decide endpoint —
   * hides the inline authorization prompt's buttons on a daemon without it.
   * Mirrors skillActivationSupported exactly. */
  delegationAuthSupported: boolean;
  /* ADR 0023 Part 3: flips off on 404/405 from any /api/v1/agents route —
   * independent of delegationAuthSupported, mirrors skillsManagementSupported. */
  agentsManagementSupported: boolean;
  /* Registered agents, for the a2a lane trust badge (ADR 0023) — loaded
   * fail-silent alongside `tools`, same optional-context convention (design/03
   * R-2): a lane renders without a badge rather than blocking on this. */
  agents: AgentView[];
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
  skillActivationSupported: true,
  skillsManagementSupported: true,
  delegationAuthSupported: true,
  agentsManagementSupported: true,
  agents: [],
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
  void loadAgents();
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

/* Skill-activation decision (ADR 0021). The kernel emits its own
 * RunEvent::SkillActivationDecided from decide_skill_activation, so — unlike
 * cancelRun — no manual axis update is needed here: the WS event clears
 * pendingSkillActivation and the run's status flips back to running. */
export async function decideSkillActivation(
  runId: string,
  skill: string,
  decision: SkillActivationDecision
): Promise<Run> {
  try {
    const run = await api<Run>(`/api/v1/runs/${runId}/skill-activation`, {
      method: "POST",
      body: JSON.stringify({ skill, decision }),
    });
    upsertRun(run);
    return run;
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 404 || err.status === 405) set({ skillActivationSupported: false });
    throw e;
  }
}

/* Delegation-authorization decision (ADR 0023). Mirrors decideSkillActivation
 * exactly, but the thing being decided is a REAL action gate — authorizing
 * this run to send work to a remote agent — not advice. The kernel emits its
 * own RunEvent::AgentDelegationDecided from decide_delegation, so no manual
 * axis update is needed here: the WS event clears pendingDelegation and the
 * run's status flips back to running. */
export async function decideDelegation(
  runId: string,
  agent: string,
  decision: DelegationDecision
): Promise<Run> {
  try {
    const run = await api<Run>(`/api/v1/runs/${runId}/delegation`, {
      method: "POST",
      body: JSON.stringify({ agent, decision }),
    });
    upsertRun(run);
    return run;
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 404 || err.status === 405) set({ delegationAuthSupported: false });
    throw e;
  }
}

// ---- agent trust/policy management (ADR 0023 Part 3) -----------------------

/* Shared 404/405 → capability-off handling, mirroring skillsApiCall. */
async function agentsApiCall<T>(path: string, opts?: RequestInit): Promise<T> {
  try {
    return await api<T>(path, opts);
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 404 || err.status === 405) set({ agentsManagementSupported: false });
    throw e;
  }
}

export const fetchAgents = (): Promise<AgentView[]> => agentsApiCall<AgentView[]>("/api/v1/agents");

export const fetchAgentsPolicy = (): Promise<AgentPolicyView> =>
  agentsApiCall<AgentPolicyView>("/api/v1/agents/policy");

export const registerAgent = (req: AgentRegisterRequest): Promise<AgentView> =>
  agentsApiCall<AgentView>("/api/v1/agents", {
    method: "POST",
    body: JSON.stringify(req),
  });

export const removeAgent = (name: string): Promise<null> =>
  agentsApiCall<null>(`/api/v1/agents/${encodeURIComponent(name)}`, { method: "DELETE" });

export const setAgentsPolicy = (update: AgentPolicyUpdate): Promise<AgentPolicyView> =>
  agentsApiCall<AgentPolicyView>("/api/v1/agents/policy", {
    method: "POST",
    body: JSON.stringify(update),
  });

// ---- skills management (ADR 0021 Part 6) -----------------------------------

/* Shared 404/405 → capability-off handling, mirroring cancelRun/
 * decideSkillActivation — every skills-CRUD route degrades the same way. */
async function skillsApiCall<T>(path: string, opts?: RequestInit): Promise<T> {
  try {
    return await api<T>(path, opts);
  } catch (e) {
    const err = e as ApiError;
    if (err.status === 404 || err.status === 405) set({ skillsManagementSupported: false });
    throw e;
  }
}

export const fetchSkills = (): Promise<SkillView[]> => skillsApiCall<SkillView[]>("/api/v1/skills");

export const fetchSkillsPolicy = (): Promise<SkillsPolicyView> =>
  skillsApiCall<SkillsPolicyView>("/api/v1/skills/policy");

export const installSkill = (url: string): Promise<SkillInstallReport> =>
  skillsApiCall<SkillInstallReport>("/api/v1/skills", {
    method: "POST",
    body: JSON.stringify({ url }),
  });

export const removeSkill = (name: string): Promise<null> =>
  skillsApiCall<null>(`/api/v1/skills/${encodeURIComponent(name)}`, { method: "DELETE" });

export const activateSkill = (name: string): Promise<null> =>
  skillsApiCall<null>(`/api/v1/skills/${encodeURIComponent(name)}/activate`, { method: "POST" });

export const deactivateSkill = (name: string): Promise<null> =>
  skillsApiCall<null>(`/api/v1/skills/${encodeURIComponent(name)}/deactivate`, { method: "POST" });

export const setSkillsPolicy = (update: SkillsPolicyUpdate): Promise<SkillsPolicyView> =>
  skillsApiCall<SkillsPolicyView>("/api/v1/skills/policy", {
    method: "POST",
    body: JSON.stringify(update),
  });

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

async function loadAgents(): Promise<void> {
  try {
    set({ agents: await fetchAgents() });
  } catch {
    /* fail-silent, same convention as loadTools — an a2a lane just renders
     * without a trust badge rather than blocking on this. */
  }
}

// ---- boot -----------------------------------------------------------------

export function boot() {
  if (connection().token) void connect();
}

export type { AxisState };
