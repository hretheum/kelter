/* R-1 (design/03): the run source-list. listbox semantics, arrow-key nav,
 * pinned "+ New run", status glyph + text (never color-only). */

import { useRef } from "react";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/badge";
import { cn, focusRing } from "@/lib/cn";
import { hasFanout } from "@/lib/events";
import {
  absoluteTime,
  formatCost,
  relativeTime,
  shortPolicy,
  STATUS_GLYPH,
} from "@/lib/format";
import { selectRun } from "@/store/app";
import { useAppState, useAxis } from "@/store/hooks";

/* Fan-out indicator (P1, ADR 0022): a run whose axis has observed any
 * branch-tagged event. Best-effort, not authoritative — the axis is only
 * populated by events actually seen this session (live WS broadcast, or a
 * prior visit's replay), so a fan-out run nobody has opened or watched live
 * since reconnecting won't show the badge yet. No backend field exists to
 * answer "did this run fork" retroactively from the run-list endpoint
 * alone; this is a known, disclosed limitation, not a bug (design/08
 * honesty convention) — see design/03 R-3 parallel variant. */
function FanoutBadge({ runId }: { runId: string }) {
  const { axis } = useAxis(runId);
  if (!hasFanout(axis)) return null;
  return (
    <span className="shrink-0 text-primary" title="This run forked into parallel branches">
      <span aria-hidden="true">⑂</span>
      <span className="sr-only"> forked</span>
    </span>
  );
}

const STATUS_CLS: Record<string, string> = {
  pending: "text-muted-foreground",
  running: "text-primary",
  awaiting_skill_decision: "text-warning",
  completed: "text-success",
  failed: "text-danger",
  canceled: "text-muted-foreground",
};

export function MasterList({
  onNewRun,
  listRef,
}: {
  onNewRun: () => void;
  listRef?: React.Ref<HTMLUListElement>;
}) {
  const s = useAppState();
  const internalRef = useRef<HTMLUListElement>(null);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const ids = s.runs.map((r) => r.id);
    if (!ids.length) return;
    const cur = s.selectedRunId ? ids.indexOf(s.selectedRunId) : -1;
    let next: number | null = null;
    if (e.key === "ArrowDown") next = Math.min(cur + 1, ids.length - 1);
    else if (e.key === "ArrowUp") next = Math.max(cur - 1, 0);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = ids.length - 1;
    else if (e.key === "Enter" && cur >= 0) {
      e.preventDefault();
      selectRun(ids[cur]);
      return;
    }
    if (next != null) {
      e.preventDefault();
      selectRun(ids[next]);
    }
  };

  return (
    <nav
      aria-label="Runs"
      className={cn(
        "flex h-full w-full md:w-[280px] shrink-0 flex-col border-r border-border bg-card",
        s.conn !== "connected" && "opacity-60"
      )}
    >
      <div className="p-2 border-b border-border">
        <Button
          variant="secondary"
          className="w-full justify-between"
          onClick={onNewRun}
          aria-keyshortcuts="Meta+N"
        >
          <span>
            <span aria-hidden="true">＋</span> New run
          </span>
          <kbd className="text-caption2 text-muted-foreground" aria-hidden="true">
            ⌘N
          </kbd>
        </Button>
      </div>

      {s.conn !== "connected" && s.runsLoaded && (
        <p className="px-3 py-1.5 text-caption2 text-warning" role="status">
          Reconnecting… showing your last known runs.
        </p>
      )}

      {!s.runsLoaded ? (
        <div className="space-y-2 p-3" aria-label="Loading runs">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-10 w-full" />
          ))}
        </div>
      ) : s.runs.length === 0 ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 p-6 text-center">
          <p className="text-body text-muted-foreground">
            No runs yet — give Kelter its first task.
          </p>
          <Button variant="ghost" size="sm" onClick={onNewRun}>
            Start one
          </Button>
        </div>
      ) : (
        <ul
          ref={listRef ?? internalRef}
          role="listbox"
          aria-label="Run history"
          tabIndex={0}
          onKeyDown={onKeyDown}
          className={cn("flex-1 overflow-y-auto outline-none", focusRing)}
        >
          {s.runs.map((run) => {
            const selected = run.id === s.selectedRunId;
            return (
              <li
                key={run.id}
                role="option"
                aria-selected={selected}
                onClick={() => selectRun(run.id)}
                className={cn(
                  "cursor-pointer border-b border-border/60 px-3 py-2",
                  "hover:bg-card-raised/60 transition-colors",
                  selected && "bg-card-raised border-l-2 border-l-primary"
                )}
              >
                <div className="flex items-baseline gap-2">
                  <span
                    className={cn("shrink-0", STATUS_CLS[run.status], run.status === "running" && "animate-pulse-live")}
                    title={run.status}
                  >
                    {STATUS_GLYPH[run.status]}
                    <span className="sr-only"> {run.status}</span>
                  </span>
                  <FanoutBadge runId={run.id} />
                  <span className="flex-1 truncate text-body" title={run.task}>
                    {run.task}
                  </span>
                  <span
                    className="shrink-0 text-caption2 text-muted-foreground tnum"
                    title={absoluteTime(run.created_at)}
                  >
                    {relativeTime(run.created_at)}
                  </span>
                </div>
                <div className="mt-0.5 flex gap-2 pl-5 text-caption2 text-muted-foreground">
                  <span className="truncate">{shortPolicy(run.policy)}</span>
                  <span className="tnum">{formatCost(run.cost_usd)}</span>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </nav>
  );
}
