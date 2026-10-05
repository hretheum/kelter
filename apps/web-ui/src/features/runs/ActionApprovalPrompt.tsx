/* ADR 0038: inline, non-modal prompt for a paused MUTATING action (kernel
 * status `awaiting_action_approval`). The operator's `[approval]` policy said
 * "ask": the model proposed an action that changes state, and nothing has
 * executed. Allow dispatches exactly that proposal (once); deny tells the
 * model to pick something else. In plan mode a `review_verdict` request is a
 * review node waiting for a human verdict. Mirrors
 * DelegationAuthorizationPrompt's shape; only the copy and endpoint differ. */

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { pretty } from "@/lib/format";
import { decideActionApproval } from "@/store/app";
import type { ActionApprovalDecision, PendingActionApprovalUi } from "@/lib/api";

export function ActionApprovalPrompt({
  runId,
  pending,
}: {
  runId: string;
  pending: PendingActionApprovalUi;
}) {
  const [resolving, setResolving] = useState<ActionApprovalDecision | null>(null);
  const [error, setError] = useState<string | null>(null);
  const review = pending.action === "review_verdict";

  const decide = async (decision: ActionApprovalDecision) => {
    setResolving(decision);
    setError(null);
    try {
      await decideActionApproval(runId, pending.action, decision);
    } catch (e) {
      setResolving(null);
      setError(`Couldn't send that — ${(e as Error).message}`);
    }
  };

  const expires =
    pending.expires_at != null
      ? new Date(pending.expires_at * 1000).toLocaleTimeString()
      : null;

  return (
    <div
      role="status"
      className="animate-slide-in rounded-md border border-border bg-card px-3 py-2.5"
    >
      <p className="text-body">
        {review ? (
          <>
            Review node <span className="font-mono">{pending.branch}</span> waits for your
            verdict on the upstream results.
          </>
        ) : (
          <>
            <span className="font-mono">{pending.action}</span> changes state and needs your
            approval before it runs.
          </>
        )}
      </p>
      <p className="mt-1.5 flex flex-wrap items-center gap-1.5 text-caption2 text-muted-foreground">
        <Badge tone="warning">◉ nothing has executed</Badge>
        {pending.branch && !review && <span>plan node {pending.branch}</span>}
        {expires && <span>denied automatically at {expires}</span>}
      </p>
      <pre className="mt-1.5 max-h-40 overflow-auto rounded-sm bg-input p-2 font-mono text-caption whitespace-pre-wrap break-words">
        {pretty(pending.arguments)}
      </pre>
      <div className="mt-2 flex flex-wrap gap-1.5">
        <Button
          variant="ghost"
          size="sm"
          disabled={resolving !== null}
          loading={resolving === "deny"}
          onClick={() => void decide("deny")}
        >
          {review ? "Reject" : "Deny"}
        </Button>
        <Button
          variant="primary"
          size="sm"
          disabled={resolving !== null}
          loading={resolving === "allow"}
          onClick={() => void decide("allow")}
        >
          {review ? "Approve" : "Allow once"}
        </Button>
      </div>
      {error && (
        <p role="alert" className="mt-1.5 text-caption text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
