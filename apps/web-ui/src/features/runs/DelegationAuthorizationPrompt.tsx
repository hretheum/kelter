/* ADR 0023: inline, non-modal prompt for a paused delegation authorization
 * (kernel status `awaiting_delegation_decision`). Unlike SkillActivationPrompt,
 * this is NOT advisory: `delegate_to_X` dials a real endpoint and sends real
 * work with a real effect, so a deny withholds AUTHORIZATION TO ACT, not
 * advice. `allow_always` rewrites the target agent's trust LABEL (steering
 * future runs' silent-delegation eligibility), never a capability outside
 * P2's existing budget/depth/cycle floor. Mirrors SkillActivationPrompt.tsx's
 * shape (Timeline.tsx's Stop-confirm framing, not a modal) — the mechanics
 * are identical, only the copy and endpoint differ. */

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { decideDelegation } from "@/store/app";
import { useAppState } from "@/store/hooks";
import type { DelegationDecision, PendingDelegationUi } from "@/lib/api";

export function DelegationAuthorizationPrompt({
  runId,
  pending,
}: {
  runId: string;
  pending: PendingDelegationUi;
}) {
  const s = useAppState();
  const [resolving, setResolving] = useState<DelegationDecision | null>(null);
  const [error, setError] = useState<string | null>(null);

  const decide = async (decision: DelegationDecision) => {
    setResolving(decision);
    setError(null);
    try {
      await decideDelegation(runId, pending.agent, decision);
      // Resolves via the paired agent_delegation_decided WS event, which
      // clears axis.pendingDelegation — no local dismiss here.
    } catch (e) {
      setResolving(null);
      setError(
        s.delegationAuthSupported
          ? `Couldn't send that — ${(e as Error).message}`
          : "This daemon doesn't support delegation-authorization decisions."
      );
    }
  };

  const trusted = pending.trust === "trusted";

  return (
    <div
      role="status"
      className="animate-slide-in rounded-md border border-border bg-card px-3 py-2.5"
    >
      <p className="text-body">
        <span className="font-mono">{pending.agent}</span> wants to send work
        to this agent — {pending.description}
      </p>
      <p className="mt-1.5 flex flex-wrap items-center gap-1.5 text-caption2 text-muted-foreground">
        <Badge tone={trusted ? "success" : "quiet"}>
          {trusted ? "✓ trusted" : "○ untrusted"}
        </Badge>
        <span>
          {pending.provenance_source_kind}: {pending.provenance_source}
        </span>
      </p>

      {s.delegationAuthSupported ? (
        <div className="mt-2 flex flex-wrap gap-1.5">
          <Button
            variant="ghost"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "deny"}
            onClick={() => void decide("deny")}
          >
            Don't send this run
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "allow_once"}
            onClick={() => void decide("allow_once")}
          >
            Authorize this run
          </Button>
          <Button
            variant="secondary"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "allow_always"}
            onClick={() => void decide("allow_always")}
          >
            Always authorize
          </Button>
        </div>
      ) : (
        <p className="mt-2 text-caption text-muted-foreground">
          This daemon doesn't support delegation-authorization decisions.
        </p>
      )}

      {error && (
        <p role="alert" className="mt-1.5 text-caption text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
