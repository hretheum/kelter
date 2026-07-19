<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec R-3 live timeline (execution axis). The core J2 screen. -->

# R-3 — Live timeline (detail pane, running run)

The execution axis. Renders one run's `RunEvent` stream (bucketed by `run_id`,
ordered by `seq`) as a step-keyed narrative — the opposite of a raw console. Owns
J2. **No editable field, no primary button** — structurally not-an-input.

## Structure & hierarchy

```
┌─ detail: timeline ───────────────────────────────────────┐
│  Summarize the RFC and file it  ◐ Kelter working…  [Stop]│  ← run header + Stop (D-2)
│  sonnet-5 · 00:07 · $0.004                                │  ← model · elapsed · running cost
│  ─────────────────────────────────────────────────────── │
│  ①  read_url                              validated ✓    │  ← step card (collapsed)
│      → 200, 4.2 KB                                        │
│  ②  write_file                                            │
│      ✕ rejected: path escapes workspace · retry 1        │  ← governance made visible
│  ②  write_file  {path:"out/summary.md"}   validated ✓    │  ← retry succeeded (expanded)
│      → wrote 1.1 KB to out/summary.md                    │
│  ③  finish                                running…       │
│                                                           │
│                                   [ ↓ New steps ]         │  ← appears only when scrolled up
└───────────────────────────────────────────────────────────┘
```

## Step card anatomy

- **Index** (`step`), **action** name, optional **rationale** (ReAct "why", muted,
  one line, expandable).
- **State line**: `proposed` → `validated ✓` / `rejected ✕ {reason} · retry n` /
  `executed ok:false ✕`.
- **Arguments / result**: collapsed by default; click to expand JSON; long content
  scrolls within the card.
- Cards animate in (subtle fade/slide, ≤150ms; respects reduced-motion).

## Header

- Task title (full, wraps).
- Live status badge: `◐ Kelter working…` (running) with a soft pulse.
- **Stop** control (D-2): `secondary`, danger-tinted; the only action on the
  timeline. Click → inline confirm ("Stop this run? · Confirm / Keep going") →
  `POST /api/v1/runs/{id}/cancel` → "Stopping…" → Canceled (R-4). Hidden when the
  cancel endpoint is flagged off (never shown-but-dead). `aria-label="Stop run"`.
- Meta row: model (`policy`), elapsed timer, running `cost_usd` (omit if null).

## Variants / states

| State | Rendering |
|---|---|
| pending/starting | header + "Starting…" placeholder, no cards |
| streaming | cards arriving; badge pulsing; auto-scroll at bottom |
| step-expanded | one card open (args + result) |
| rejected-visible | refusal chip under the step; axis awaits retry |
| user-scrolled-up | auto-scroll paused + "↓ New steps" pill |
| lagged | one-line "Some intermediate events were skipped (fast run)." |
| disconnected mid-run | banner "Connection lost — the run keeps running on the kernel. Reconnecting…"; cards frozen, not cleared |
| stopping (D-2) | Stop → "Stopping…" after inline confirm; awaiting cancel write |
| → terminal | transitions in place to R-4 (result: completed / failed / **canceled**) |

## Streaming / scroll (interaction-critical — see `04`)

- Auto-scroll to newest **only when pinned to bottom**. Scrolling up pauses it and
  shows "↓ New steps"; returning to bottom (or clicking the pill) resumes.
- Never move the viewport out from under a user who is reading.

## Copy

- Running header: "Kelter is working…"
- Rejected chip: "Kernel rejected this step: {reason} · retry {attempt}"
- Executed-failed: "Ran, but the tool reported an error."

## Accessibility

- The axis is a `role="log"` / `aria-live="polite"` region so new steps are
  announced without stealing focus. Each card is a heading-led group.
- Expand/collapse are real buttons with `aria-expanded`.
- The live pulse has a text equivalent ("working…") — never motion-only.

## API / data

- WS `/api/v1/events` (global; filter `run_id`; order `seq`). On reconnect,
  reconcile from `GET /api/v1/runs/{id}` (`steps[]`), since proposals aren't
  replayable (honest — Flow 07).
