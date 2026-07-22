/* Turning a run's terminal output into readable prose (ADR 0029).
 *
 * The kernel's own `finish` action yields `{ summary }`, and plan mode yields
 * `{ summary, nodes }` — but the output is formally `unknown` (a policy could
 * finish with anything), so this degrades by shape instead of trusting one:
 * prose when a summary-like string exists, and the rest of the payload as a
 * collapsible detail — never a raw dump as the primary rendering, never a
 * throw. */

/* Keys tried, in order, for the human-readable sentence of an object output.
 * `summary` is the kernel contract (finish_spec); the rest are graceful
 * degradation for foreign shapes (e.g. an A2A remote's result). */
const PROSE_KEYS = ["summary", "message", "text", "result"] as const;

export interface OutputView {
  /* The readable sentence(s), or null when the output has no prose-like part. */
  prose: string | null;
  /* Pretty-printed remainder (or whole output) for the collapsed disclosure;
   * null when the prose already covers everything. */
  detail: string | null;
}

export function outputView(output: unknown): OutputView {
  if (output == null) return { prose: null, detail: null };

  if (typeof output === "string") {
    const trimmed = output.trim();
    return trimmed ? { prose: trimmed, detail: null } : { prose: null, detail: null };
  }

  if (typeof output === "object" && !Array.isArray(output)) {
    const record = output as Record<string, unknown>;
    for (const key of PROSE_KEYS) {
      const value = record[key];
      if (typeof value === "string" && value.trim()) {
        const rest = Object.fromEntries(
          Object.entries(record).filter(([k]) => k !== key)
        );
        return {
          prose: value.trim(),
          detail: Object.keys(rest).length ? safeStringify(rest) : null,
        };
      }
    }
  }

  /* Numbers, booleans, arrays, prose-less objects: no sentence to show —
   * the payload goes to the disclosure instead. */
  return { prose: null, detail: safeStringify(output) };
}

function safeStringify(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    /* circular or otherwise unserializable — still never a throw */
    return String(value);
  }
}
