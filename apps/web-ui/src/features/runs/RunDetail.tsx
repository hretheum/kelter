/* The detail pane: one pane, three faces chosen by run status (design/01
 * §2.3) — composer XOR live timeline XOR result — never three places. */

import { useEffect, useRef } from "react";
import { selectRun } from "@/store/app";
import { useAppState } from "@/store/hooks";
import { Composer } from "./Composer";
import { Result } from "./Result";
import { Timeline } from "./Timeline";

export function RunDetail() {
  const s = useAppState();
  const run = s.selectedRunId
    ? s.runs.find((r) => r.id === s.selectedRunId)
    : null;
  const headerRef = useRef<HTMLDivElement>(null);
  const lastRunId = useRef<string | null>(null);

  // Focus handoff on selecting a run (design/04 §3): announce via focus on
  // the pane container, not stolen mid-type (composer manages its own focus).
  useEffect(() => {
    if (run && run.id !== lastRunId.current) {
      lastRunId.current = run.id;
      headerRef.current?.focus();
    }
    if (!run) lastRunId.current = null;
  }, [run]);

  if (!run) return <Composer />;

  const live =
    run.status === "pending" ||
    run.status === "running" ||
    run.status === "awaiting_skill_decision" ||
    run.status === "awaiting_delegation_decision";
  return (
    <div
      ref={headerRef}
      tabIndex={-1}
      aria-label={`Run: ${run.task} (${run.status})`}
      className="h-full outline-none"
    >
      {live ? (
        <Timeline run={run} />
      ) : (
        <Result run={run} onNewRun={() => selectRun(null)} />
      )}
    </div>
  );
}
