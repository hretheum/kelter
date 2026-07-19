<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec R-4 result (terminal run: completed/failed/canceled + historical). -->

# R-4 — Result (detail pane, terminal run)

Where a run settles. Renders `completed{output}` / `failed{error}` (live), or a
`Run` fetched from history. Owns J3. A failure is a **result**, not an alert.

## Structure & hierarchy

```
┌─ detail: result ─────────────────────────────────────────┐
│  Summarize the RFC and file it               ✓ Completed │  ← header + terminal status
│  sonnet-5 · 12 steps · 00:41 · $0.011                     │
│  ─────────────────────────────────────────────────────── │
│  Result                                    [ Copy output ]│
│  ┌─────────────────────────────────────────────────────┐ │
│  │ { "summary_path": "out/summary.md",                 │ │  ← output block (scrolls)
│  │   "words": 320 }                                    │ │
│  └─────────────────────────────────────────────────────┘ │
│  ▸ Execution record (12 steps)                            │  ← collapsible axis (from R-3 / steps[])
└───────────────────────────────────────────────────────────┘
```

## Variants / states

| State | Header | Body |
|---|---|---|
| completed (output) | `✓ Completed` (success) | output block + **Copy output** + collapsible step record |
| completed (no output) | `✓ Completed` | "Completed with no output." + step record |
| failed | `✕ Failed` (danger) | error block + **Copy error** + steps-so-far + **New run** |
| canceled | `⊘ Canceled` (muted) | "This run was canceled." + partial steps (via Stop or history) |
| historical | terminal status | **full axis** via `GET /runs/{id}/events` (D-1); reconstructed-record fallback + note if replay unavailable |
| loading (opened from history) | skeleton header | spinner while `GET /runs/{id}` (+ `…/events`) |

## Historical axis (D-1, in scope)

When opened from the master list, fetch `GET /api/v1/runs/{id}/events` and render
the **full** axis (proposals, rejections, rationale) — identical richness to the
live timeline. Only if replay is unavailable (endpoint flagged off / older
deployment) fall back to the reconstructed record from `steps[]` with a muted
note: "Showing the executed record. Full reasoning replay isn't available for this
run." `GET /runs/{id}/events` is a net-new endpoint (`06 §1a`).

## Copy

- Copy buttons: "Copy output" / "Copy error" → "Copied ✓" (~1.5s).
- Failed body lead: "This run failed." then the reason block.
- Cost null: "—" (tooltip: "No price configured for this model.").
- Canceled: "This run was canceled." (reached via the timeline's **Stop**, D-2.)

## Interaction

- **Copy** writes the raw `output`/`error` to clipboard (JSON stringified if
  object). Keyboard: focus Copy → `Enter`.
- **New run** (failed variant) opens R-2 composer (`⌘N`); the failed task can be
  pre-filled as a convenience (optional, note in handoff) — no auto-retry.
- Step record expand/collapse mirrors R-3 cards.

## Responsive

- Desktop: result + collapsible steps in the detail pane.
- Narrow: full page; back returns to the master list.

## Accessibility

- On terminal transition, an `aria-live="polite"` announcement: "Run completed" /
  "Run failed: {short reason}".
- Output/error blocks are `<pre>` with a real Copy `<button>` (not a click-div);
  `aria-label` distinguishes "Copy output" vs "Copy error".

## API / data

- Live: terminal event carries `output`/`error`. History: `GET /api/v1/runs/{id}`
  → `output` | `error` | `steps[]` | `status`, plus `GET /api/v1/runs/{id}/events`
  (D-1) for the full historical axis.
