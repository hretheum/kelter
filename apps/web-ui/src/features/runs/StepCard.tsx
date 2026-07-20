/* One step on the execution axis (design/03 R-3): index + action + state
 * line; args/results collapsed by default; rejections visible (the
 * governance moment), never hidden. */

import { useState } from "react";
import { Spinner } from "@/components/ui/button";
import { cn, focusRing } from "@/lib/cn";
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

      {/* Rejections are the visible proof the kernel validates (design/00). */}
      {step.rejections.map((r) => (
        <p
          key={r.attempt}
          className="mt-1 rounded-sm bg-danger/10 px-2 py-1 text-caption text-danger"
        >
          Kernel rejected this step: {r.reason} · retry {r.attempt}
        </p>
      ))}

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
