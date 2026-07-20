/* ADR 0021: inline, non-modal prompt for a paused skill self-activation
 * (kernel status `awaiting_skill_decision`). Advisory only — nothing here
 * changes the action space; `allow_always` only rewrites a trust LABEL
 * (steering exposure), never capability. Mirrors Timeline.tsx's Stop-confirm
 * shape, not ConnectionSheet's modal: a security-dialog framing is exactly
 * what this must NOT read as. */

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { decideSkillActivation } from "@/store/app";
import { useAppState } from "@/store/hooks";
import type { PendingSkillActivationUi, SkillActivationDecision } from "@/lib/api";

export function SkillActivationPrompt({
  runId,
  pending,
}: {
  runId: string;
  pending: PendingSkillActivationUi;
}) {
  const s = useAppState();
  const [resolving, setResolving] = useState<SkillActivationDecision | null>(null);
  const [error, setError] = useState<string | null>(null);

  const decide = async (decision: SkillActivationDecision) => {
    setResolving(decision);
    setError(null);
    try {
      await decideSkillActivation(runId, pending.skill, decision);
      // Resolves via the paired skill_activation_decided WS event, which
      // clears axis.pendingSkillActivation — no local dismiss here.
    } catch (e) {
      setResolving(null);
      setError(
        s.skillActivationSupported
          ? `Couldn't send that — ${(e as Error).message}`
          : "This daemon doesn't support skill-activation decisions."
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
        <span className="font-mono">{pending.skill}</span> wants to advise on
        this run — {pending.description}
      </p>
      <p className="mt-1.5 flex flex-wrap items-center gap-1.5 text-caption2 text-muted-foreground">
        <Badge tone={trusted ? "success" : "quiet"}>
          {trusted ? "✓ trusted" : "○ untrusted"}
        </Badge>
        <span>
          {pending.provenance_source_kind}: {pending.provenance_source}
        </span>
      </p>

      {s.skillActivationSupported ? (
        <div className="mt-2 flex flex-wrap gap-1.5">
          <Button
            variant="ghost"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "deny"}
            onClick={() => void decide("deny")}
          >
            Not this run
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "allow_once"}
            onClick={() => void decide("allow_once")}
          >
            Allow this run
          </Button>
          <Button
            variant="secondary"
            size="sm"
            disabled={resolving !== null}
            loading={resolving === "allow_always"}
            onClick={() => void decide("allow_always")}
          >
            Always allow
          </Button>
        </div>
      ) : (
        <p className="mt-2 text-caption text-muted-foreground">
          This daemon doesn't support skill-activation decisions.
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
