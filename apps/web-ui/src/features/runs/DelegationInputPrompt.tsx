/* ADR 0026: inline, non-modal prompt for an interactive-delegation pause
 * (kernel status `awaiting_delegation_input`). An AUTHORIZED delegation dialed
 * a remote agent that entered `input-required` — it needs more information
 * before it can finish. This is NOT an authorization gate (the dial already
 * happened, P2+P3 already passed): it collects the human's continuation text,
 * which the kernel forwards to the SAME remote task on resume. Mirrors
 * DelegationAuthorizationPrompt's inline shape; the difference is a free-text
 * answer instead of deny/allow buttons. */

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/input";
import { provideDelegationInput } from "@/store/app";
import type { PendingDelegationInputUi } from "@/lib/api";

export function DelegationInputPrompt({
  runId,
  pending,
}: {
  runId: string;
  pending: PendingDelegationInputUi;
}) {
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const send = async () => {
    const text = input.trim();
    if (!text) {
      setError("Type an answer first.");
      return;
    }
    setSending(true);
    setError(null);
    try {
      await provideDelegationInput(runId, text);
      // Resolves via the paired agent_delegation_input_provided WS event,
      // which clears axis.pendingDelegationInput — no local dismiss here.
    } catch (e) {
      setSending(false);
      setError(`Couldn't send that — ${(e as Error).message}`);
    }
  };

  return (
    <div
      role="status"
      className="animate-slide-in rounded-md border border-border bg-card px-3 py-2.5"
    >
      <p className="text-body">
        <span className="font-mono">{pending.agent}</span> needs more input to
        continue
      </p>
      <p className="mt-1.5 text-callout text-foreground">{pending.prompt}</p>

      <div className="mt-2 space-y-1.5">
        <label htmlFor="delegation-input" className="sr-only">
          Your answer to {pending.agent}
        </label>
        <Textarea
          id="delegation-input"
          rows={2}
          placeholder="Type your answer to the agent…"
          value={input}
          disabled={sending}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            // Cmd/Ctrl+Enter sends — a textarea's Enter inserts a newline.
            if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
              e.preventDefault();
              void send();
            }
          }}
        />
        <div className="flex items-center gap-2">
          <Button
            variant="primary"
            size="sm"
            loading={sending}
            disabled={sending}
            onClick={() => void send()}
          >
            Send to agent
          </Button>
          <span className="text-caption2 text-muted-foreground">⌘⏎ to send</span>
        </div>
      </div>

      {error && (
        <p role="alert" className="mt-1.5 text-caption text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
