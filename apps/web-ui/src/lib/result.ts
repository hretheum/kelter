import type { Run, RunEpisodes } from "./api";

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

/* ADR 0037: how a settled run's success was (not) verified, as one line for
 * the result face. `canConfirm`/`canRevoke` mirror exactly what the kernel
 * accepts on /runs/{id}/confirmation, so the UI never offers a 409. */
export interface VerificationView {
  tone: "verified" | "unverified" | "confirmed" | "contract_failed" | null;
  text: string | null;
  canConfirm: boolean;
  canRevoke: boolean;
}

export function verificationView(run: Run): VerificationView {
  const none: VerificationView = { tone: null, text: null, canConfirm: false, canRevoke: false };
  if (run.status === "incomplete") {
    if (run.verification === "contract" && run.contract_report && !run.contract_report.satisfied) {
      const unmet = run.contract_report.conditions.filter((c) => !c.met).length;
      return {
        ...none,
        tone: "contract_failed",
        text: `Success contract not met (${unmet} of ${run.contract_report.conditions.length} condition(s) failed).`,
      };
    }
    return none;
  }
  if (run.status !== "completed") return none;
  if (run.verification === "contract") {
    return { ...none, tone: "verified", text: "Verified by the success contract." };
  }
  // `verification` absent/null on a completed run = it ended before ADR 0037:
  // judged by the ADR 0033 floor only, so unverified too (maintainer's
  // decision, 2026-10-05) and confirmable the same way.
  if (run.verification === "none" || run.verification == null) {
    if (run.confirmation) {
      return {
        ...none,
        tone: "confirmed",
        text: `Unverified by the kernel — confirmed by ${run.confirmation.principal}. Used for learning.`,
        canRevoke: true,
      };
    }
    return {
      ...none,
      tone: "unverified",
      text: "Unverified — no success contract. Not used for learning until a human confirms it.",
      canConfirm: true,
    };
  }
  return none;
}

/* ADR 0038: how memory holds the run's reflection episode — the third state
 * (`unverified`) is never shown as a success. `null` when there is nothing to
 * say (no episode, or a backend that keeps no history). */
export function episodeView(e: RunEpisodes | null): string | null {
  if (!e || !e.history_supported || e.current.length === 0) return null;
  const cur = e.current[0];
  const by = cur.provenance === "human" ? ", set by a human" : "";
  const label =
    cur.outcome === "unverified"
      ? "unverified — in context, not as a success pattern"
      : cur.outcome === "success"
        ? "success"
        : "failure";
  return `Memory: ${label} (episode v${cur.version}${by}).`;
}
