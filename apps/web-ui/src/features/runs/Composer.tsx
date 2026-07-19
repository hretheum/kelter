/* R-2 (design/03): the input surface — the ONLY place with an editable field
 * and a primary button; labeled "Task for Kelter"; Enter contract per
 * design/04 §2 (plain Enter & ⌘Enter submit, Shift+Enter newline, IME guard). */

import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Label, Textarea } from "@/components/ui/input";
import { submitRun } from "@/store/app";
import { useAppState } from "@/store/hooks";

const EXAMPLES = [
  "Read https://example.com, extract its structure, and write a markdown report to out/report.md",
  "List the files in the workspace and summarize what this project does",
];

export function Composer() {
  const s = useAppState();
  const [task, setTask] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);
  const disconnected = s.conn !== "connected";
  const firstRun = s.runsLoaded && s.runs.length === 0;

  useEffect(() => {
    ref.current?.focus(); // design/04 §3: focus lands in the task field
  }, []);

  const submit = async () => {
    const trimmed = task.trim();
    if (!trimmed) {
      // Mirrors the API's 422 without a round-trip (design/02-flows/03).
      setError("Type a task first");
      ref.current?.focus();
      return;
    }
    setError(null);
    setBusy(true);
    try {
      await submitRun(trimmed);
      setTask(""); // pane becomes the timeline in place (store selects the run)
    } catch (e) {
      setError(`Submit failed: ${(e as Error).message}`); // keep the text
    } finally {
      setBusy(false);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // The Enter contract (design/04 §2), with the IME composition guard.
    if (e.key !== "Enter" || e.nativeEvent.isComposing) return;
    if (e.shiftKey) return; // Shift+Enter → newline (default)
    e.preventDefault(); // plain Enter AND ⌘/Ctrl+Enter both submit
    void submit();
  };

  return (
    <div className="mx-auto w-full max-w-2xl p-6">
      {firstRun && (
        <div className="mb-6 text-center">
          <h2 className="text-title2 font-semibold">Give Kelter a task.</h2>
          <p className="mt-1 text-body text-muted-foreground">
            It proposes each step, the kernel checks it, and you watch it run.
          </p>
        </div>
      )}

      <div className="space-y-1.5">
        <Label htmlFor="task">Task for Kelter</Label>
        <Textarea
          id="task"
          ref={ref}
          rows={4}
          placeholder={`e.g. ${EXAMPLES[0]}`}
          value={task}
          disabled={busy || disconnected}
          error={!!error}
          aria-describedby={error ? "task-error" : "task-hint"}
          onChange={(e) => {
            setTask(e.target.value);
            if (error) setError(null);
          }}
          onKeyDown={onKeyDown}
        />
        <div className="flex items-center justify-between">
          <p id="task-hint" className="text-caption2 text-muted-foreground">
            ⏎ to run · ⇧⏎ new line
          </p>
          <Button
            onClick={() => void submit()}
            loading={busy}
            disabled={disconnected || !task.trim()}
            aria-keyshortcuts="Enter Meta+Enter"
          >
            {busy ? "Running…" : "Run"}
            <kbd className="text-caption2 opacity-70" aria-hidden="true">
              ⌘⏎
            </kbd>
          </Button>
        </div>
        {error && (
          <p id="task-error" role="alert" className="text-caption text-danger">
            {error}
          </p>
        )}
        {disconnected && (
          <p className="text-caption text-muted-foreground">
            Reconnect to run a task.
          </p>
        )}
      </div>

      {firstRun && (
        <div className="mt-6 space-y-2">
          <p className="text-caption text-muted-foreground">Try one of these:</p>
          {EXAMPLES.map((ex) => (
            <button
              key={ex}
              onClick={() => {
                setTask(ex);
                ref.current?.focus();
              }}
              className="block w-full rounded-md border border-border bg-card px-3 py-2 text-left text-caption text-muted-foreground hover:text-foreground hover:bg-card-raised/60 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            >
              {ex}
            </button>
          ))}
        </div>
      )}

      {/* Action-space hint (design/07 D-3): teaches the first step; the store
          fails silent if /tools errors — never blocks the composer. */}
      {s.tools.length > 0 && (
        <p className="mt-6 text-caption2 text-muted-foreground">
          Kelter can:{" "}
          {s.tools
            .slice(0, 8)
            .map((t) => t.name)
            .join(" · ")}
        </p>
      )}
    </div>
  );
}
