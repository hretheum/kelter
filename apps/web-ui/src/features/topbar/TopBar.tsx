/* G-1 (design/03): identity + model chip + connection pill + settings.
 * The skip link is the app's FIRST focusable element (design/04 §6a). */

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { cn, focusRing } from "@/lib/cn";
import { shortPolicy } from "@/lib/format";
import { openSheet, selectRun, showSettings } from "@/store/app";
import type { ApprovalItem } from "@/lib/api";
import { useAppState } from "@/store/hooks";

const PILL: Record<string, { glyph: string; label: string; cls: string }> = {
  connected: { glyph: "◉", label: "connected", cls: "text-success" },
  connecting: { glyph: "◐", label: "connecting…", cls: "text-warning" },
  disconnected: { glyph: "○", label: "disconnected", cls: "text-danger" },
  "auth-failed": { glyph: "⚠", label: "auth failed", cls: "text-danger" },
};

/* What a waiting run needs from you, in a few words. */
function waitingFor(a: ApprovalItem): string {
  const p = a.pending ?? {};
  if (a.kind === "action_approval") return p.action ? `approve ${p.action}` : "approve an action";
  if (p.skill) return `activate skill ${p.skill}`;
  if (p.agent) return `delegate to ${p.agent}`;
  return a.status.replaceAll("_", " ");
}

/* ADR 0038: every run waiting on a human, one click away — so an Opus
 * request (or any pause) is not missed until it auto-denies. Hidden when
 * nothing waits or the daemon has no /approvals route. */
function ApprovalsBadge({ items }: { items: ApprovalItem[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="relative" onKeyDown={(e) => e.key === "Escape" && setOpen(false)}>
      <button
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        aria-label={`${items.length} run${items.length === 1 ? "" : "s"} waiting for your decision`}
        className={cn(
          "inline-flex items-center gap-1.5 rounded-full bg-warning/15 px-2.5 h-6 text-caption text-warning",
          focusRing
        )}
      >
        <span aria-hidden="true">⏸</span> {items.length} waiting
      </button>
      {open && (
        <ul className="absolute right-0 top-8 z-20 w-80 space-y-1 rounded-md border border-border bg-card p-1.5 shadow-lg">
          {items.map((a) => (
            <li key={a.run_id}>
              <button
                onClick={() => {
                  selectRun(a.run_id);
                  setOpen(false);
                }}
                className={cn("block w-full rounded-sm px-2 py-1.5 text-left hover:bg-card-raised/60", focusRing)}
              >
                <span className="block truncate text-caption">{a.task}</span>
                <span className="block text-caption2 text-warning">
                  {waitingFor(a)}
                  {a.pending?.expires_at != null &&
                    ` · auto-deny ${new Date(a.pending.expires_at * 1000).toLocaleString()}`}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function TopBar() {
  const s = useAppState();
  const pill = PILL[s.conn];
  const model = s.config.view ? shortPolicy(s.config.view.engine) : null;

  return (
    <header
      data-tauri-drag-region
      className="vibrancy sticky top-0 z-10 flex items-center gap-3 border-b border-border px-4 h-11"
    >
      <a href="#main" className="skip-link">
        Skip to runs
      </a>
      <span className="font-semibold text-callout tracking-tight">Kelter</span>

      {/* Model chip (design/01 §2.1, gate-#1 approved): read-only awareness,
          click → Settings. Editing lives only in Settings, off the loop. */}
      {model && (
        <button
          onClick={() => showSettings(true)}
          aria-label={`Model: ${model}. Open model settings.`}
          className={cn(
            "inline-flex items-center gap-1 rounded-full bg-card-raised px-2.5 h-6",
            "text-caption text-muted-foreground hover:text-foreground transition-colors",
            focusRing
          )}
        >
          <span aria-hidden="true">▣</span> {model}
        </button>
      )}

      <div className="ml-auto flex items-center gap-2">
        {s.approvals && s.approvals.length > 0 && <ApprovalsBadge items={s.approvals} />}
        <button
          onClick={() => openSheet(true)}
          aria-label={`Connection: ${pill.label}. Open connection settings.`}
          aria-live="polite"
          className={cn(
            "inline-flex items-center gap-1.5 rounded-full bg-card-raised px-2.5 h-6",
            "text-caption",
            pill.cls,
            focusRing
          )}
        >
          <span aria-hidden="true">{pill.glyph}</span> {pill.label}
        </button>
        <Button
          variant="ghost"
          size="icon"
          aria-label="Settings"
          aria-keyshortcuts="Meta+Comma"
          onClick={() => showSettings(true)}
        >
          <span aria-hidden="true">⚙</span>
        </Button>
      </div>
    </header>
  );
}
