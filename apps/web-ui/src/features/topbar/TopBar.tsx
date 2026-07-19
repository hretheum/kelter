/* G-1 (design/03): identity + model chip + connection pill + settings.
 * The skip link is the app's FIRST focusable element (design/04 §6a). */

import { Button } from "@/components/ui/button";
import { cn, focusRing } from "@/lib/cn";
import { shortPolicy } from "@/lib/format";
import { openSheet, showSettings } from "@/store/app";
import { useAppState } from "@/store/hooks";

const PILL: Record<string, { glyph: string; label: string; cls: string }> = {
  connected: { glyph: "◉", label: "connected", cls: "text-success" },
  connecting: { glyph: "◐", label: "connecting…", cls: "text-warning" },
  disconnected: { glyph: "○", label: "disconnected", cls: "text-danger" },
  "auth-failed": { glyph: "⚠", label: "auth failed", cls: "text-danger" },
};

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
