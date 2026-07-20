/* R-3 (design/03): the live execution axis. role=log, milestone-throttled
 * announcements (design/04 §6), auto-scroll that pauses when the user scrolls
 * up (design/04 §4), Stop (D-2) with an inline confirm. Structurally
 * not-an-input: no editable field, no primary button. */

import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge, Banner } from "@/components/ui/badge";
import { announce } from "@/lib/announcer";
import { axisSteps } from "@/lib/events";
import { formatCost, formatElapsed, shortPolicy } from "@/lib/format";
import { cancelRun } from "@/store/app";
import { useAppState, useAxis } from "@/store/hooks";
import type { Run } from "@/lib/api";
import { DelegationAuthorizationPrompt } from "./DelegationAuthorizationPrompt";
import { DelegationInputPrompt } from "./DelegationInputPrompt";
import { FanoutLanes } from "./FanoutLanes";
import { SkillActivationPrompt } from "./SkillActivationPrompt";
import { StepCard } from "./StepCard";

const SCROLL_THRESHOLD = 48; // px from bottom = still "pinned" (design/04 §4)

export function Timeline({ run }: { run: Run }) {
  const s = useAppState();
  const { axis } = useAxis(run.id);
  const steps = axisSteps(axis);
  // Branch-tagged steps (parallel plan fan-out, ADR 0022) render as
  // swimlanes via FanoutLanes below, not in the ordinary flat list — same
  // underlying axis, just grouped differently, never both.
  const flatSteps = steps.filter((st) => st.branch === null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const [showPill, setShowPill] = useState(false);
  const [stopState, setStopState] = useState<"idle" | "confirm" | "stopping">("idle");
  const [stopError, setStopError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  const announced = useRef(new Set<string>());
  const prevPendingSkill = useRef<string | null>(null);
  const prevPendingDelegation = useRef<string | null>(null);
  const prevPendingDelegationInput = useRef<string | null>(null);

  // Live elapsed timer while running.
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, []);

  // Auto-scroll state machine (design/04 §4): scroll only while pinned.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (pinnedRef.current) {
      el.scrollTop = el.scrollHeight;
      setShowPill(false);
    } else {
      setShowPill(true);
    }
  }, [
    steps.length,
    axis.terminal,
    axis.pendingSkillActivation,
    axis.pendingDelegation,
    axis.pendingDelegationInput,
  ]);

  // Milestone announcements only (design/04 §6): rejected + executed + terminal.
  useEffect(() => {
    for (const st of steps) {
      for (const r of st.rejections) {
        const key = `rej-${st.step}-${r.attempt}`;
        if (!announced.current.has(key)) {
          announced.current.add(key);
          announce(`${st.action} rejected`);
        }
      }
      if (st.executed) {
        const key = `exec-${st.step}`;
        if (!announced.current.has(key)) {
          announced.current.add(key);
          announce(`${st.action} executed`);
        }
      }
    }
  }, [steps]);

  // Skill-activation pause/resolve (ADR 0021): the pause must not be missed
  // (assertive); the resolution is a quieter confirmation (polite).
  useEffect(() => {
    const cur = axis.pendingSkillActivation?.skill ?? null;
    const prev = prevPendingSkill.current;
    if (cur && cur !== prev) {
      announce(`${cur} wants to advise on this run — awaiting your decision`, "assertive");
    } else if (!cur && prev) {
      announce(`${prev} activation decided`);
    }
    prevPendingSkill.current = cur;
  }, [axis.pendingSkillActivation]);

  // Delegation-authorization pause/resolve (ADR 0023): a paused delegation
  // withholds a real action, so its pause is announced as assertively as the
  // skill-activation pause above — same urgency, different stakes.
  useEffect(() => {
    const cur = axis.pendingDelegation?.agent ?? null;
    const prev = prevPendingDelegation.current;
    if (cur && cur !== prev) {
      announce(`${cur} wants to send work to this agent — awaiting your authorization`, "assertive");
    } else if (!cur && prev) {
      announce(`${prev} delegation authorization decided`);
    }
    prevPendingDelegation.current = cur;
  }, [axis.pendingDelegation]);

  // Interactive-delegation input pause/resolve (ADR 0026): an authorized
  // delegation's remote agent asked for more input — announced assertively
  // like the pauses above (the run cannot progress until answered).
  useEffect(() => {
    const cur = axis.pendingDelegationInput?.agent ?? null;
    const prev = prevPendingDelegationInput.current;
    if (cur && cur !== prev) {
      announce(`${cur} needs more input to continue — awaiting your answer`, "assertive");
    } else if (!cur && prev) {
      announce(`${prev} received your input`);
    }
    prevPendingDelegationInput.current = cur;
  }, [axis.pendingDelegationInput]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.scrollTop - el.clientHeight;
    const pinned = dist < SCROLL_THRESHOLD;
    pinnedRef.current = pinned;
    if (pinned) setShowPill(false);
  };

  const jumpToLatest = () => {
    const el = scrollRef.current;
    if (!el) return;
    pinnedRef.current = true;
    el.scrollTop = el.scrollHeight;
    setShowPill(false);
  };

  const doStop = async () => {
    setStopState("stopping");
    setStopError(null);
    try {
      await cancelRun(run.id);
    } catch (e) {
      setStopState("idle");
      setStopError(
        s.cancelSupported
          ? `Stop failed: ${(e as Error).message}`
          : "This daemon doesn't support stopping runs."
      );
    }
  };

  return (
    <div className="relative flex h-full flex-col">
      <div className="border-b border-border px-4 py-3">
        <div className="flex items-start gap-3">
          <h2 className="flex-1 text-title3 font-semibold">{run.task}</h2>
          {run.status === "awaiting_delegation_input" ? (
            <Badge tone="warning" className="shrink-0">
              ◉ Awaiting your input
            </Badge>
          ) : run.status === "awaiting_skill_decision" ||
            run.status === "awaiting_delegation_decision" ? (
            <Badge tone="warning" className="shrink-0">
              ◉ Awaiting your decision
            </Badge>
          ) : (
            <Badge tone="accent" className="animate-pulse-live shrink-0">
              ◐ Kelter working…
            </Badge>
          )}
          {/* Stop (D-2): the only action on the timeline; inline confirm; the
              capability flag HIDES it on a daemon without the endpoint. */}
          {s.cancelSupported && stopState === "idle" && (
            <Button variant="danger" size="sm" aria-label="Stop run" onClick={() => setStopState("confirm")}>
              Stop
            </Button>
          )}
          {stopState === "confirm" && (
            <span className="flex items-center gap-1.5 text-caption">
              Stop this run?
              <Button variant="danger" size="sm" onClick={() => void doStop()}>
                Confirm
              </Button>
              <Button variant="ghost" size="sm" onClick={() => setStopState("idle")}>
                Keep going
              </Button>
            </span>
          )}
          {stopState === "stopping" && (
            <Button variant="danger" size="sm" loading disabled>
              Stopping…
            </Button>
          )}
        </div>
        <p className="mt-1 flex gap-3 text-caption text-muted-foreground">
          <span>{shortPolicy(run.policy)}</span>
          <span className="tnum">{formatElapsed(run.created_at, now)}</span>
          {run.cost_usd != null && <span className="tnum">{formatCost(run.cost_usd)}</span>}
        </p>
        {stopError && (
          <p role="alert" className="mt-1 text-caption text-danger">
            {stopError}
          </p>
        )}
        {s.conn !== "connected" && (
          <Banner tone="warning" className="mt-2">
            Connection lost — the run keeps running on the kernel. Reconnecting…
          </Banner>
        )}
      </div>

      <div
        ref={scrollRef}
        onScroll={onScroll}
        role="log"
        aria-label="Execution"
        aria-busy={!axis.terminal}
        className="flex-1 space-y-2 overflow-y-auto p-4"
      >
        {steps.length === 0 && (
          <p className="text-body text-muted-foreground">Starting…</p>
        )}
        {flatSteps.map((st) => (
          <StepCard key={st.step} step={st} />
        ))}
        <FanoutLanes axis={axis} now={now} />
        {axis.pendingSkillActivation && (
          <SkillActivationPrompt runId={run.id} pending={axis.pendingSkillActivation} />
        )}
        {axis.pendingDelegation && (
          <DelegationAuthorizationPrompt runId={run.id} pending={axis.pendingDelegation} />
        )}
        {axis.pendingDelegationInput && (
          <DelegationInputPrompt runId={run.id} pending={axis.pendingDelegationInput} />
        )}
        {axis.reflections > 0 && (
          <p className="text-caption2 text-muted-foreground">
            {axis.reflections} reflection{axis.reflections > 1 ? "s" : ""} written
          </p>
        )}
      </div>

      {showPill && (
        <Button
          variant="secondary"
          size="sm"
          onClick={jumpToLatest}
          className="absolute bottom-4 left-1/2 -translate-x-1/2 shadow-lg"
        >
          ↓ New steps
        </Button>
      )}
    </div>
  );
}
