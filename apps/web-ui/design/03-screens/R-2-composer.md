<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec R-2 composer (the input surface). This is the anti-"is-this-a-console" screen. -->

# R-2 — Composer (detail pane, new run)

The input surface. The single place with an editable field + primary button.
Owns J1. Structurally distinct from the timeline so it can never be mistaken for
output.

## Structure & hierarchy

```
┌─ detail: composer ───────────────────────────────────────┐
│  Task for Kelter                                          │  ← explicit label (this is INPUT)
│  ┌─────────────────────────────────────────────────────┐ │
│  │ e.g. Read https://example.com, extract its          │ │  ← textarea, auto-focus
│  │ structure, and write a report to out/report.md      │ │
│  └─────────────────────────────────────────────────────┘ │
│  ⏎ to run · ⇧⏎ new line                 [   Run  ⌘⏎  ]   │  ← hint + primary button
│                                                           │
│  Kelter can:  read_url · write_file · http_get · …        │  ← action-space hint (D-3)
└───────────────────────────────────────────────────────────┘
```

## Content (copy)

- **Label:** "Task for Kelter" — names the surface as input and its recipient.
- **Placeholder:** "e.g. Read https://example.com, extract its structure, and
  write a markdown report to out/report.md"
- **Submit hint:** "⏎ to run · ⇧⏎ new line".
- **Button:** "Run" with `⌘⏎` chip.
- **Empty-state teaching (above/within, when the whole app has no runs):**
  - Title: "Give Kelter a task."
  - Subtitle: "It proposes each step, the kernel checks it, and you watch it run."
  - Example chips (click to fill): 2–3 realistic tasks.
  - Action space: "Kelter can: {tool names from GET /api/v1/tools}" (D-3; if the
    call fails, omit silently — never block the composer on it).

## Variants / states

| State | Rendering |
|---|---|
| empty | teaching copy + examples + action-space; Run disabled |
| typing | Run enabled; hint visible |
| submitting | textarea disabled, Run → spinner "Running…" |
| validation-error | inline "Type a task first" under the field, focus kept, no POST |
| disabled (disconnected) | field + Run disabled; note "Reconnect to run a task" |

## Interaction (see `04` for the full contract)

- Auto-focus on open. `Enter` submits, `Shift+Enter` newline, `⌘/Ctrl+Enter`
  submits. Empty/whitespace → validation-error, never a request.
- On 202: pane transitions **in place** to R-3 (live timeline) for the returned
  `run_id`; field clears; a row prepends to R-1.
- On error: preserve the typed text; inline error.

## Responsive

- Desktop: composer fills the detail pane, textarea ~3–6 rows auto-grow.
- Narrow: composer is a full page reached via New run; back returns to the list.

## Accessibility

- `<label for>` bound to the textarea (real label, not placeholder-as-label).
- Button `aria-keyshortcuts="Meta+Enter Enter"`; validation error via
  `aria-describedby` + `aria-invalid`.
- Focus ring always visible on the field and button.

## API

- `POST /api/v1/runs { task }` → 202 `{run_id}` | 422 (empty — pre-empted
  client-side) | 5xx (inline error, keep text).
