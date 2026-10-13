/* One step on the execution axis (design/03 R-3): index + action + state
 * line; args/results collapsed by default; rejections visible (the
 * governance moment), never hidden. */

import { useState } from "react";
import { Spinner } from "@/components/ui/button";
import { cn, focusRing } from "@/lib/cn";
import { executorView } from "@/lib/executor";
import { pretty } from "@/lib/format";
import type { AxisStep } from "@/lib/events";

/* A2A results carry `remote_ms` (a2a/src/lib.rs) once a receipt lands — the
 * only honestly-known elapsed figure for a delegation (design/03 R-3
 * parallel variant). Narrow, defensive: `result` is `unknown`. */
function remoteMsOf(result: unknown): number | null {
  if (result && typeof result === "object" && "remote_ms" in result) {
    const v = (result as { remote_ms: unknown }).remote_ms;
    if (typeof v === "number") return v;
  }
  return null;
}

/* `{"error": "..."}` is the kernel's general failed-result convention
 * (engine.rs, not delegation-specific) — surfaced inline rather than left
 * hidden behind the collapsed result pane, so a circuit-open refusal folded
 * into a terminal `Executed{ok:false}` (ADR 0023 P4: happens only when the
 * primary's circuit is already open before the very first dial, so there's
 * no loud `Rejected` to show it — ungoverned attempts never generate one)
 * is as visible as an ordinary rejection, matching the "never hidden"
 * convention (design/00) rather than requiring an expand click to see why. */
function errorOf(result: unknown): string | null {
  if (result && typeof result === "object" && "error" in result) {
    const v = (result as { error: unknown }).error;
    if (typeof v === "string") return v;
  }
  return null;
}

/* Humanize an A2A task state ("TASK_STATE_WORKING" → "working") for the
 * streamed-progress trail (ADR 0027). Unknown/blank falls back to the raw
 * value rather than fabricating a friendlier label. */
function humanState(state: string): string {
  const s = state.replace(/^TASK_STATE_/, "").toLowerCase();
  return s || state;
}

export function StepCard({
  step,
  settled,
  opaqueRemote,
}: {
  step: AxisStep;
  /* True when the run is terminal: a step with no receipt (e.g. `finish`)
   * renders as a quiet record, not a pending "proposed…". */
  settled?: boolean;
  /* True for a still-running A2A delegation lane (design/03 R-3 parallel
   * variant, fan-out swimlanes): no single-delegation "opaque remote"
   * treatment existed before this — confirmed absent from this component,
   * not reinvented. Swaps the bare "proposed…" text for a genuinely
   * INDETERMINATE spinner + honest copy — never a percentage or fake step
   * count, since the kernel has no notion of remote progress to report. */
  opaqueRemote?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const state = step.executed
    ? step.executed.ok
      ? "executed"
      : "executed-failed"
    : settled
      ? "recorded"
      : step.rejections.length
        ? "retrying"
        : "proposed";
  const remoteMs = step.executed ? remoteMsOf(step.executed.result) : null;
  // Claude Code executor steps (KRW.1) get a readable summary; their failure
  // reason lives in `blad`, which the generic `error` lookup would miss.
  const exec = step.executed ? executorView(step.action, step.executed.result) : null;
  const errorMsg =
    state === "executed-failed" ? (exec?.error ?? errorOf(step.executed?.result)) : null;

  return (
    <div className="animate-slide-in rounded-md border border-border bg-card px-3 py-2">
      <button
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        className={cn(
          "flex w-full items-baseline gap-2 text-left rounded-sm",
          focusRing
        )}
      >
        <span className="shrink-0 text-caption2 text-muted-foreground tnum">
          {step.step + 1}
        </span>
        <span className="font-mono text-body">{step.action || "…"}</span>
        <span className="ml-auto flex shrink-0 items-center gap-1.5 text-caption2">
          {state === "proposed" && opaqueRemote && (
            <>
              <Spinner />
              <span className="text-ev-proposed">waiting on remote agent…</span>
            </>
          )}
          {state === "proposed" && !opaqueRemote && (
            <span className="text-ev-proposed">proposed…</span>
          )}
          {state === "recorded" && (
            <span className="text-muted-foreground">recorded</span>
          )}
          {state === "retrying" && (
            <span className="text-warning">retrying…</span>
          )}
          {state === "executed" && (
            <span className="text-success">
              validated ✓ · executed
              {remoteMs != null && ` · ${(remoteMs / 1000).toFixed(1)}s remote`}
            </span>
          )}
          {state === "executed-failed" && (
            <span className="text-danger">ran, tool error ✕</span>
          )}
        </span>
        <span className="shrink-0 text-caption2 text-muted-foreground" aria-hidden="true">
          {open ? "▾" : "▸"}
        </span>
      </button>

      {/* Rejections are the visible proof the kernel validates (design/00).
          Keyed by index, not `attempt`: ADR 0023 P4's governed resilience
          loop numbers retry-phase and fallback-phase attempts independently
          from 1, so `attempt: 1` can legitimately appear twice for one step
          — an `attempt`-only key would collide and drop a React node. */}
      {step.rejections.map((r, i) => (
        <p
          key={i}
          className="mt-1 rounded-sm bg-danger/10 px-2 py-1 text-caption text-danger"
        >
          Kernel rejected this step: {r.reason} · retry {r.attempt}
        </p>
      ))}

      {exec && (
        <p className="mt-1 font-mono text-caption2 text-muted-foreground">{exec.headline}</p>
      )}

      {errorMsg && (
        <p className="mt-1 rounded-sm bg-danger/10 px-2 py-1 text-caption text-danger">
          {exec?.violation ? "Policy violation: " : "Ran, but failed: "}
          {errorMsg}
        </p>
      )}

      {exec?.outputTail && (
        <pre className="mt-1 max-h-48 overflow-auto rounded-sm bg-danger/5 p-2 font-mono text-caption2 whitespace-pre-wrap break-words">
          {exec.outputTail}
        </pre>
      )}

      {exec?.kind === "run" && exec.ok && exec.changes && (
        <pre className="mt-1 max-h-32 overflow-auto rounded-sm bg-input p-2 font-mono text-caption2 whitespace-pre-wrap break-words">
          {exec.changes}
        </pre>
      )}

      {/* Streamed remote-delegation progress (ADR 0027): an advisory trail of
          intermediate updates the remote agent emitted while working. Shown
          as an honest, indeterminate trace — never a progress percentage or
          fabricated step count. Kept visible after the step settles as part
          of the audit trail; the newest line pulses only while still
          running. */}
      {step.progress.length > 0 && (
        <ol className="mt-1.5 space-y-0.5 border-l border-border/70 pl-2.5">
          {step.progress.map((p, i) => {
            const isLatest = i === step.progress.length - 1;
            const live = isLatest && !step.executed;
            return (
              <li
                key={p.ordinal}
                className={cn(
                  "text-caption2 text-muted-foreground",
                  live && "animate-pulse-live text-ev-proposed"
                )}
              >
                <span aria-hidden="true">↳ </span>
                {p.note}
                <span className="text-muted-foreground/70"> · {humanState(p.state)}</span>
              </li>
            );
          })}
        </ol>
      )}

      {step.rationale && (
        <p className="mt-1 text-caption2 text-muted-foreground italic">
          {step.rationale}
        </p>
      )}

      {open && (
        <div className="mt-2 space-y-2">
          {step.arguments != null && (
            <div>
              <p className="text-caption2 text-muted-foreground">arguments</p>
              <pre className="mt-0.5 max-h-48 overflow-auto rounded-sm bg-input p-2 font-mono text-caption whitespace-pre-wrap break-words">
                {pretty(step.arguments)}
              </pre>
            </div>
          )}
          {step.executed && (
            <div>
              <p className="text-caption2 text-muted-foreground">result</p>
              <pre className="mt-0.5 max-h-48 overflow-auto rounded-sm bg-input p-2 font-mono text-caption whitespace-pre-wrap break-words">
                {pretty(step.executed.result)}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
