/* Parallel plan fan-out swimlanes (P1, ADR 0022; design/03 R-3 parallel
 * variant). Same event stream as the flat timeline, bucketed by `branch`
 * (design/00 §3.2's one-reducer-many-views idea, extended). Each lane
 * reuses StepCard verbatim — args/results, rejections, and executed states
 * all render through the IDENTICAL component as an ordinary step, so a
 * governance-refused delegation is visually indistinguishable in KIND from
 * any other kernel rejection (design/08 honesty convention): mirror, don't
 * reinvent. No DAG diagram — a plan node's `step` index already reflects
 * its topological position (Kahn's algorithm, plan.rs), so sorting lanes by
 * `step` reads as "diverge then converge" without a graphical renderer,
 * which nothing else in this app uses. */

import { Badge } from "@/components/ui/badge";
import {
  axisBranches,
  countDelegationAttempts,
  laneKind,
  trustOfLaneAgent,
  type AxisState,
  type LaneKind,
} from "@/lib/events";
import { formatElapsed } from "@/lib/format";
import { useAppState } from "@/store/hooks";
import { StepCard } from "./StepCard";

const LANE_BADGE: Record<LaneKind, { tone: "accent" | "quiet"; label: string }> = {
  a2a: { tone: "accent", label: "⇄ delegate" },
  review: { tone: "quiet", label: "☰ review" },
  tool: { tone: "quiet", label: "⚙ tool" },
};

export function FanoutLanes({ axis, now }: { axis: AxisState; now: number }) {
  const s = useAppState();
  const lanes = axisBranches(axis);
  if (lanes.length === 0) return null;
  const attempted = countDelegationAttempts(axis);

  return (
    <div className="animate-slide-in rounded-md border border-border/80 bg-card-raised/40 p-2.5">
      <div className="flex items-center justify-between gap-2 px-0.5">
        <p className="flex items-center gap-1.5 text-caption font-medium text-foreground">
          <span aria-hidden="true">⑂</span>
          Fan-out — {lanes.length} branch{lanes.length > 1 ? "es" : ""}
        </p>
        {/* No denominator: no endpoint exposes the daemon's real
            max-delegations-per-run (design/03 R-3 parallel variant honesty
            note) — showing "N/max" would mean guessing a number the UI
            can't verify. */}
        {attempted > 0 && (
          <Badge tone="quiet">
            {attempted} delegation{attempted > 1 ? "s" : ""} so far
          </Badge>
        )}
      </div>

      <div className="mt-2 space-y-2 border-l-2 border-border pl-3">
        {lanes.map((lane) => {
          const kind = laneKind(lane.action);
          const running = !lane.executed && lane.rejections.length === 0;
          // Real trust badge (ADR 0023): joined client-side against the
          // registered-agents list (GET /api/v1/agents) by the same name
          // sanitization the a2a crate uses to derive `delegate_to_<name>`
          // (crates/a2a/src/lib.rs `action_name`) — no trust field exists on
          // proposed/executed events, only the registry route does, so this
          // is the honest source rather than a fabricated one.
          const trust = kind === "a2a" ? trustOfLaneAgent(lane.action, s.agents) : null;
          return (
            <div key={lane.step}>
              <div className="mb-1 flex items-center gap-1.5">
                <Badge tone={LANE_BADGE[kind].tone}>{LANE_BADGE[kind].label}</Badge>
                {trust != null && (
                  <Badge tone={trust === "trusted" ? "success" : "quiet"}>
                    {trust === "trusted" ? "✓ trusted" : "○ untrusted"}
                  </Badge>
                )}
                {/* Elapsed time while running: client wall-clock since this
                    lane was first observed — remote_ms (the more precise,
                    server-known figure) only exists once a receipt lands,
                    and StepCard already shows that once executed. */}
                {running && kind === "a2a" && (
                  <span className="text-caption2 text-muted-foreground tnum">
                    {formatElapsed(lane.receivedAt / 1000, now)}
                  </span>
                )}
              </div>
              <StepCard step={lane} opaqueRemote={kind === "a2a" && running} />
            </div>
          );
        })}
      </div>
    </div>
  );
}
