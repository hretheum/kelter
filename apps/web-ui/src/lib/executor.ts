/* The Claude Code executor (deploy/claude-code, roadmap KRW.1) as the UI sees
 * it: pure helpers over the tool results of `claude_code_run`,
 * `claude_code_opus` and `sprawdz`, the success contract a coding task
 * submits, and a budget stop in words (ADR 0039). Defensive throughout —
 * results are `unknown` and an older runner may omit fields. */

import type { BudgetStop } from "./api";

export const RUN_TOOLS = ["claude_code_run", "claude_code_opus"] as const;
export const CHECK_TOOL = "sprawdz";

const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
const str = (v: unknown): string | null => (typeof v === "string" && v.trim() ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/* The only contract a coding task needs: the runner refuses `sprawdz` for a
 * task that failed or changed nothing, so a passing check implies a
 * successful run with changes — on the Haiku/Sonnet path and the Opus path
 * alike (a contract naming `claude_code_run` would fail every Opus run). */
export const CODING_CONTRACT = {
  require: [{ kind: "action_succeeded", action: CHECK_TOOL }],
} as const;

/* The coding-task toggle appears only when the daemon actually offers the
 * executor; otherwise the contract would be unsatisfiable (422 at creation). */
export function executorAvailable(tools: { name: string }[]): boolean {
  const names = new Set(tools.map((t) => t.name));
  return names.has(CHECK_TOOL) && RUN_TOOLS.some((t) => names.has(t));
}

/* POST /runs body for the composer. `minutes` is an optional lower-only
 * wall-clock override (ADR 0039); blank/invalid → none. */
export function runRequest(task: string, coding: boolean, minutes?: string): Record<string, unknown> {
  const body: Record<string, unknown> = { task };
  if (coding) body.success_contract = CODING_CONTRACT;
  const m = minutes ? Number(minutes) : NaN;
  if (Number.isInteger(m) && m > 0) body.budget = { max_wall_secs: m * 60 };
  return body;
}

export interface ExecutorView {
  kind: "run" | "check";
  ok: boolean;
  /* One line for the step card. */
  headline: string;
  /* Why it failed, if it did — never hidden behind the collapsed pane. */
  error: string | null;
  /* The session used Opus without approval (runner's `naruszenie`). */
  violation: boolean;
  branch: string | null;
  costUsd: number | null;
  changes: string | null;
  /* The tail of a check's output, shown when the check failed. */
  outputTail: string | null;
}

export function executorView(action: string, result: unknown): ExecutorView | null {
  const isRun = (RUN_TOOLS as readonly string[]).includes(action);
  if (!isRun && action !== CHECK_TOOL) return null;
  if (!isObj(result)) return null;
  const ok = result.ok === true;
  const error = str(result.blad) ?? str(result.error);
  const branch = str(result.galaz);
  if (isRun) {
    const model = str(result.model) ?? (action === "claude_code_opus" ? "opus" : "?");
    const cost = num(result.koszt_usd);
    const secs = num(result.czas_s);
    const parts = [model, branch, secs != null ? `${secs}s` : null, cost != null ? `~$${cost.toFixed(2)}` : null];
    return {
      kind: "run",
      ok,
      headline: parts.filter(Boolean).join(" · "),
      error,
      violation: !!error && error.startsWith("naruszenie"),
      branch,
      costUsd: cost,
      changes: str(result.zmiany),
      outputTail: null,
    };
  }
  const name = str(result.sprawdzenie) ?? CHECK_TOOL;
  const exit = num(result.kod_wyjscia);
  const out = str(result.wyjscie);
  return {
    kind: "check",
    ok,
    headline: [name, branch, ok ? "passed" : exit != null ? `exit ${exit}` : "failed"].filter(Boolean).join(" · "),
    error: error ?? (!ok && exit != null ? `check exited with ${exit}` : null),
    violation: false,
    branch,
    costUsd: null,
    changes: null,
    outputTail: !ok && out ? (out.length > 1200 ? `…${out.slice(-1200)}` : out) : null,
  };
}

/* Claude Code's notional cost over a run's executed steps. Kelter's own
 * `cost_usd` covers only its model; this is shown beside it, never summed
 * into it (KRW.3: the kernel's budget does not see it yet). */
export function executorCostUsd(steps: { action: string; result: unknown }[]): number | null {
  let total = 0;
  let any = false;
  for (const s of steps) {
    if (!(RUN_TOOLS as readonly string[]).includes(s.action) || !isObj(s.result)) continue;
    const c = num(s.result.koszt_usd);
    if (c != null) {
      total += c;
      any = true;
    }
  }
  return any ? total : null;
}

const LIMIT_TEXT: Record<BudgetStop["limit"], (v: number) => string> = {
  usd: (v) => `$${v.toFixed(2)}`,
  daily_usd: (v) => `$${v.toFixed(2)} per day`,
  tokens: (v) => `${Math.round(v).toLocaleString("en-US")} tokens`,
  wall_time: (v) => `${Math.round(v / 60)} min`,
};

/* A budget stop in one sentence (ADR 0039): which limit, how much was used,
 * and that the kernel stopped *before* crossing it. */
export function budgetStopText(stop: BudgetStop | null | undefined): string | null {
  if (!stop) return null;
  const fmt = LIMIT_TEXT[stop.limit] ?? ((v: number) => String(v));
  const what = stop.limit === "daily_usd" ? "daily spending limit" : stop.limit === "wall_time" ? "time limit" : `${stop.limit} limit`;
  const where = stop.checkpoint === "model_call" ? "before the next model call" : "before the next action";
  return `Stopped by the ${what} (${fmt(stop.max)}): ${fmt(stop.used)} used, stopped ${where} at step ${stop.step + 1} — the limit was never crossed.`;
}
