<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: observing a live run as an execution axis (not a raw console). Grounded in the real RunEvent stream. -->

# Flow 04 — Observe execution (live)

**Job:** J2 — the product's soul: watch a governed agent work. **Destination:**
Runs · live timeline (R-3). **API:** WS `/api/v1/events` (global broadcast,
bucket by `run_id`, order by `seq`).

## The model (from `00 §3.2`)

Events arrive for *all* runs; the timeline renders **one** run by filtering
`envelope.run_id === boundRunId` and ordering by `seq`. Each event advances a
**step-keyed axis**, not a flat log:

```
Step 1  ▸ read_url {url:"…"}                    proposed   ← "wants to"
        ↳ ✓ validated → executed  ok:true        result     ← kernel let it, here's the result
Step 2  ▸ write_file {path:"…"}                 proposed
        ✗ rejected: path escapes workspace (attempt 1)      ← the governance moment, shown
        ▸ write_file {path:"out/…"}             proposed   (attempt 2)
        ↳ ✓ executed  ok:true                    result
…
✓ Completed   {output}                                      ← terminal
```

## Steps

1. **Bind** — on entering (from submit or selecting a running run), subscribe to
   the WS (already open) and start bucketing this `run_id`.
2. **Render header** — task title, live status badge (`running`), model (`policy`),
   elapsed time, running cost if priced.
3. **Stream steps** — each `proposed` opens/updates a step card; `rejected`
   appends a refusal chip under the step (with reason + attempt) and the axis
   waits for the retry `proposed`; `executed` resolves the step with ok/result;
   `reflection_written` is a quiet footnote.
4. **Terminal** — `completed` → transition to Result (Flow 05) in place;
   `failed` → Result in its failed variant. The status badge flips accordingly.

## Stop a run (D-2, in scope)

The running header carries a **Stop** control (`secondary`/danger-tinted). It is
the *only* action on the timeline, and it is not the composer's primary button —
so it doesn't reintroduce input/output confusion.

1. Click **Stop** → inline confirm (the button becomes "Stop this run? · Confirm /
   Keep going") to prevent accidental loss of in-flight work. No heavy modal.
2. Confirm → `POST /api/v1/runs/{id}/cancel`; button → "Stopping…".
3. The kernel aborts + writes the durable `Canceled` state; the run transitions to
   Result (Flow 05) canceled variant, showing whatever steps completed.
4. Backed by the net-new `POST /runs/{id}/cancel` (`06 §1a`); if the endpoint is
   flagged off (older deployment), Stop is **hidden**, never shown-but-dead.

## Streaming / scroll behavior (interaction-critical)

- **Auto-scroll** to the newest step **only while the user is at the bottom**. If
  the user scrolls up to read, auto-scroll **pauses** and a "↓ New steps" pill
  appears; clicking it (or scrolling back to bottom) resumes. Never yank the
  viewport away from something the user is reading.
- Each step card is collapsible; **arguments/results collapse by default** to keep
  the axis skimmable, expand on click. Long results scroll within the card.

## States (R-3)

| State | Rendering |
|---|---|
| pending/starting | header + "Starting…" placeholder, no steps yet |
| streaming | step cards arriving; live badge pulsing |
| step-expanded | one card open showing arguments + result JSON |
| rejected-visible | refusal chip under the affected step (not hidden) |
| user-scrolled-up | auto-scroll paused + "↓ New steps" pill |
| lagged (WS `Lagged`) | subtle "some events were skipped" note (the server drops for slow consumers — `stream_events`) |
| disconnected mid-run | banner "Connection lost — the run keeps running on the kernel; reconnecting…" (Flow 07); run is durable, not lost |

## "This is output, not input" reinforcement

- The timeline has **no editable field and no primary button** — structurally it
  cannot be mistaken for the composer.
- Header label: **"Execution — Kelter is working"** while running.
- Each step reads as narration ("proposed", "kernel validated", "executed"),
  never as a prompt.

## Copy

- Running header: "Kelter is working…"
- Rejected chip: "Kernel rejected this step: {reason} · retry {attempt}"
- Lagged: "Some intermediate events were skipped (fast run)."

## Exit

Run reaches a terminal status → Result (Flow 05), same pane. The run row in the
master list updates its status glyph.
