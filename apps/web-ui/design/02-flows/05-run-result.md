<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: run result (completed/failed/canceled), copy affordances. Errors are first-class results. -->

# Flow 05 — Run result

**Job:** J3 — get the result and use it. **Destination:** Runs · result (R-4).
**Source:** the terminal `completed{output}` / `failed{error}` event (live), or
`GET /api/v1/runs/{id}` (`output`/`error`/`steps[]`) when opened from history.

## Principle

A **failure is a result, not an alert.** Both success and failure render in the
same result surface, copyable, with the run's step record above — never a
transient toast the user can't get back.

## Steps (completed)

1. Header flips to `Completed` with final elapsed time, model, total cost
   (`cost_usd` — show "—" when null, never a fake $0.00).
2. **Output** renders in a result block: JSON pretty-printed (or text if the
   output is a string). A **Copy** button copies the raw output. Long output
   scrolls within the block.
3. The executed **step record** stays visible above/collapsible (the axis from
   Flow 04 for a live run; reconstructed from `steps[]` for a historical one).

## Steps (failed)

1. Header flips to `Failed` (red).
2. **Error** renders in an error block (the `error` string), with **Copy**.
3. Steps up to the failure remain visible so the user sees *where* it broke.
4. A "New run" affordance is offered (re-compose; no automatic retry — the kernel
   has no UI retry endpoint, and we don't fake one).

## Steps (canceled)

- Reached from the live timeline's **Stop** (D-2, Flow 04) or found in history
  (`status:canceled`): header `Canceled`, whatever steps completed before
  cancellation, note "This run was canceled." No output.

## Historical vs live (D-1 in scope)

- **Live** result: full axis (proposals, rejections, rationale) above the output,
  built from the WS stream.
- **Historical** result (opened from master list): the full axis is fetched via
  `GET /api/v1/runs/{id}/events` (D-1) — **same richness as live** (proposals,
  rejections, rationale), not just `steps[]`. Fallback only when replay is flagged
  off / unavailable: reconstruct from `steps[]` + output/error with the honest
  note "Showing the executed record. Full reasoning replay isn't available for
  this run." `GET /runs/{id}/events` is a net-new endpoint (build dependency,
  `06 §1a`).

## States (R-4)

| State | Rendering |
|---|---|
| completed (output) | result block + Copy + step record |
| failed (error) | error block + Copy + steps-so-far + New run |
| canceled | canceled note + partial steps (reachable via Stop or history) |
| historical | full axis via `GET /runs/{id}/events` (D-1); reconstructed-record fallback + note if replay unavailable |
| empty output | "Completed with no output." (completed but `output:null`) |

## Copy

- Copy button: "Copy output" / "Copy error"; on click → "Copied ✓" for ~1.5s.
- Failed banner: "This run failed." + the reason block (not a dismissible alert).
- Cost null: "—" with tooltip "No price configured for this model."

## Exit

User copies/uses the result, starts a New run (`Cmd+N`), or selects another run
from the master list (J4, Flow 06).
