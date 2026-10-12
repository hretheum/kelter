/* R-4 (design/03): where a run settles. A failure is a RESULT, not an alert.
 * Copy affordances per design/04 §5; historical axis from journal replay
 * (D-1) with the honest fallback note when replay is unavailable. */

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { announce } from "@/lib/announcer";
import { copyValue } from "@/lib/clipboard";
import { axisSteps } from "@/lib/events";
import { formatCost, shortPolicy, STATUS_GLYPH } from "@/lib/format";
import { episodeView, outputView, verificationView } from "@/lib/result";
import { getRunEpisodes, setRunConfirmation } from "@/store/app";
import { useAppState, useAxis } from "@/store/hooks";
import type { Run, RunEpisodes } from "@/lib/api";
import { ArtifactPanel } from "./ArtifactPanel";
import { StepCard } from "./StepCard";

function CopyButton({ value, label }: { value: unknown; label: string }) {
  const [state, setState] = useState<"idle" | "copied" | "fallback">("idle");
  return (
    <Button
      variant="ghost"
      size="sm"
      aria-label={label}
      onClick={async () => {
        const ok = await copyValue(value);
        setState(ok ? "copied" : "fallback");
        if (ok) announce("Copied");
        window.setTimeout(() => setState("idle"), 1500);
      }}
    >
      {state === "copied" ? "Copied ✓" : state === "fallback" ? "Press ⌘C to copy" : label}
    </Button>
  );
}

const TONE = {
  completed: { badge: "success" as const, glyph: STATUS_GLYPH.completed, label: "Completed" },
  failed: { badge: "danger" as const, glyph: STATUS_GLYPH.failed, label: "Failed" },
  incomplete: { badge: "warning" as const, glyph: STATUS_GLYPH.incomplete, label: "Incomplete" },
  canceled: { badge: "quiet" as const, glyph: STATUS_GLYPH.canceled, label: "Canceled" },
};

/* ADR 0037: the success basis, and the human confirm/revoke control for an
 * unverified completed run (the only thing that admits it to learning). */
function VerificationLine({ run }: { run: Run }) {
  const v = verificationView(run);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // ADR 0040: the run's episode follows confirm/revoke; re-read it whenever
  // the confirmation changes (here or from another client).
  const [episodes, setEpisodes] = useState<RunEpisodes | null>(null);
  const confirmedAt = run.confirmation?.at_ms ?? null;
  useEffect(() => {
    let live = true;
    getRunEpisodes(run.id)
      .then((e) => live && setEpisodes(e))
      .catch(() => live && setEpisodes(null));
    return () => {
      live = false;
    };
  }, [run.id, run.status, confirmedAt]);
  const memoryText = episodeView(episodes);
  if (!v.text || v.tone === "contract_failed") return null;
  const act = async (confirm: boolean) => {
    setBusy(true);
    setErr(null);
    try {
      await setRunConfirmation(run.id, confirm);
      announce(confirm ? "Run confirmed" : "Confirmation revoked");
    } catch (e) {
      setErr(String((e as Error).message));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-2 flex flex-wrap items-center gap-2 text-caption" aria-label="Verification">
      <span className={v.tone === "unverified" ? "text-warning" : "text-muted-foreground"}>
        {v.text}
      </span>
      {v.canConfirm && (
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => act(true)}>
          Confirm it did the task
        </Button>
      )}
      {v.canRevoke && (
        <Button variant="ghost" size="sm" disabled={busy} onClick={() => act(false)}>
          Revoke confirmation
        </Button>
      )}
      {err && <span className="text-danger">{err}</span>}
      {memoryText && <span className="basis-full text-muted-foreground">{memoryText}</span>}
    </div>
  );
}

export function Result({ run, onNewRun }: { run: Run; onNewRun: () => void }) {
  const s = useAppState();
  const { axis } = useAxis(run.id);
  const steps = axisSteps(axis);
  const [recordOpen, setRecordOpen] = useState(false);
  const status = (run.status === "completed" ||
  run.status === "failed" ||
  run.status === "incomplete" ||
  run.status === "canceled"
    ? run.status
    : "completed") as keyof typeof TONE;
  const tone = TONE[status];

  // Terminal announcement (design/04 §6): failure is assertive.
  useEffect(() => {
    if (status === "failed") {
      announce(`Run failed: ${(run.error ?? "").slice(0, 80)}`, "assertive");
    } else if (status === "incomplete") {
      announce("Run incomplete: not counted as done", "assertive");
    } else if (status === "completed") {
      announce("Run completed");
    }
  }, [run.id, status, run.error]);

  const output =
    axis.terminal?.kind === "completed" || axis.terminal?.kind === "incomplete"
      ? axis.terminal.output
      : run.output;
  const error =
    run.error ??
    (axis.terminal?.kind === "failed"
      ? axis.terminal.error
      : axis.terminal?.kind === "incomplete"
        ? axis.terminal.reason
        : null);
  // Prose-first result rendering (ADR 0029): the summary as readable text,
  // the rest of the payload behind a disclosure — never a raw dump up front.
  const view = outputView(output);
  // The artifact panel accompanies a settled successful run; a failed run's
  // face stays focused on the error.
  const showArtifacts = status === "completed";

  return (
    <div
      className={`mx-auto w-full p-4 ${
        showArtifacts ? "max-w-6xl md:flex md:items-start md:gap-6" : "max-w-3xl"
      }`}
    >
      <div className="min-w-0 flex-1 md:max-w-3xl">
        <div className="flex items-start gap-3">
          <h2 className="flex-1 text-title3 font-semibold">{run.task}</h2>
          <Badge tone={tone.badge} className="shrink-0">
            {tone.glyph} {tone.label}
          </Badge>
        </div>
        <p className="mt-1 flex gap-3 text-caption text-muted-foreground">
          <span>{shortPolicy(run.policy)}</span>
          <span className="tnum">
            {run.steps.length} step{run.steps.length === 1 ? "" : "s"}
          </span>
          <span className="tnum" title={run.cost_usd == null ? "No price configured for this model." : undefined}>
            {formatCost(run.cost_usd)}
          </span>
        </p>

        {status === "completed" && (
          <section className="mt-4" aria-label="Result">
            <div className="flex items-center justify-between">
              <h3 className="text-callout font-medium">Result</h3>
              {output != null && <CopyButton value={output} label="Copy output" />}
            </div>
            <VerificationLine run={run} />
            {view.prose ? (
              <p className="mt-1 text-body whitespace-pre-wrap">{view.prose}</p>
            ) : (
              <p className="mt-1 text-body text-muted-foreground">
                {view.detail ? "Completed." : "Completed with no output."}
              </p>
            )}
            {view.detail && (
              <details className="mt-2">
                <summary className="cursor-pointer rounded-sm text-caption text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
                  Raw output
                </summary>
                <pre className="mt-1 max-h-96 overflow-auto rounded-md border border-border bg-card p-3 font-mono text-caption whitespace-pre-wrap break-words">
                  {view.detail}
                </pre>
              </details>
            )}
          </section>
        )}

        {status === "failed" && (
          <section className="mt-4" aria-label="Error">
            <div className="flex items-center justify-between">
              <h3 className="text-callout font-medium text-danger">
                This run failed.
              </h3>
              {error && <CopyButton value={error} label="Copy error" />}
            </div>
            <pre className="mt-1 max-h-96 overflow-auto rounded-md border border-danger/40 bg-danger/5 p-3 font-mono text-caption whitespace-pre-wrap break-words">
              {error ?? "failed"}
            </pre>
            <Button variant="secondary" size="sm" className="mt-3" onClick={onNewRun}>
              New run
            </Button>
          </section>
        )}

        {status === "incomplete" && (
          /* ADR 0033: the model said "done", but nothing it ran succeeded —
           * show the kernel's verdict first and the claim only as unverified. */
          <section className="mt-4" aria-label="Incomplete">
            <h3 className="text-callout font-medium text-warning">
              {verificationView(run).tone === "contract_failed"
                ? "Success contract not met — not counted as done."
                : "Finished without any successful action — not counted as done."}
            </h3>
            <p className="mt-1 text-body text-muted-foreground">{error ?? "incomplete"}</p>
            {view.prose && (
              <>
                <h4 className="mt-3 text-caption font-medium text-muted-foreground">
                  The model's own summary (unverified)
                </h4>
                <p className="mt-1 text-body whitespace-pre-wrap">{view.prose}</p>
              </>
            )}
            <Button variant="secondary" size="sm" className="mt-3" onClick={onNewRun}>
              New run
            </Button>
          </section>
        )}

        {status === "canceled" && (
          <p className="mt-4 text-body text-muted-foreground">
            This run was canceled.
          </p>
        )}

        {steps.length > 0 && (
          <section className="mt-5" aria-label="Execution record">
            <button
              onClick={() => setRecordOpen(!recordOpen)}
              aria-expanded={recordOpen}
              className="text-caption text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background rounded-sm"
            >
              {recordOpen ? "▾" : "▸"} Execution record ({steps.length} step
              {steps.length === 1 ? "" : "s"})
            </button>
            {!s.replaySupported && axis.fromReplay && (
              <p className="mt-1 text-caption2 text-muted-foreground">
                Showing the executed record. Full reasoning replay isn't
                available for this run.
              </p>
            )}
            {recordOpen && (
              <div className="mt-2 space-y-2">
                {steps.map((st) => (
                  <StepCard key={st.step} step={st} settled />
                ))}
              </div>
            )}
          </section>
        )}
      </div>

      {showArtifacts && s.runFilesSupported && <ArtifactPanel runId={run.id} />}
    </div>
  );
}
