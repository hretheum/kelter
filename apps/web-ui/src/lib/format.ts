/* Display formatting helpers (design/03 R-1). */

/* Relative time for list rows ("2m", "1h", "Jul 18"); absolute in tooltips. */
export function relativeTime(epochSecs: number): string {
  const delta = Math.max(0, Date.now() / 1000 - epochSecs);
  if (delta < 60) return "now";
  if (delta < 3600) return `${Math.floor(delta / 60)}m`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h`;
  return new Date(epochSecs * 1000).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

export function absoluteTime(epochSecs: number): string {
  return new Date(epochSecs * 1000).toLocaleString();
}

/* Short model badge from the policy string: "openrouter:anthropic/claude-sonnet-5"
 * → "claude-sonnet-5", "local:qwen2.5-0.5b-instruct" → "qwen2.5-0.5b-instruct". */
export function shortPolicy(policy: string): string {
  const model = policy.split(":").slice(1).join(":") || policy;
  const last = model.split("/").pop() || model;
  return last;
}

/* Dollar cost: null = unpriced → "—", never a fabricated $0.00 (design/03 R-4). */
export function formatCost(cost: number | null | undefined): string {
  if (cost == null) return "—";
  return `$${cost.toFixed(cost < 0.01 ? 4 : 3)}`;
}

export function formatElapsed(startedEpochSecs: number, nowMs: number): string {
  const s = Math.max(0, Math.floor(nowMs / 1000 - startedEpochSecs));
  const mm = String(Math.floor(s / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  return `${mm}:${ss}`;
}

export function pretty(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

/* Status glyphs (design/03 R-1): glyph carries meaning — never color-only. */
export const STATUS_GLYPH: Record<string, string> = {
  pending: "◔",
  running: "◐",
  awaiting_skill_decision: "◉",
  awaiting_delegation_decision: "◉",
  awaiting_delegation_input: "◉",
  completed: "✓",
  failed: "✕",
  canceled: "⊘",
};
