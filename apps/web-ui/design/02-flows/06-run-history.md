<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: run history / master list. -->

# Flow 06 — Run history / list

**Job:** J4. **Destination:** Runs · master list (R-1). **API:** `GET
/api/v1/runs` (refreshed on `created`/`completed`/`failed` events, as today).

## Steps

1. **Load** — `GET /api/v1/runs` on connect; render newest-first.
2. **Row** — per run: status glyph (color-coded), truncated task (title), relative
   time, model badge, cost (if priced). A running run shows a live pulse.
3. **Select** — click a row → detail pane loads that run:
   - running → live timeline (Flow 04), re-bound to its `run_id`.
   - terminal → result (Flow 05, historical variant).
4. **Live updates** — a new run prepends a row; status changes update the glyph in
   place (driven by the same WS events, bucketed).

## States (R-1)

| State | Rendering |
|---|---|
| empty (no runs) | teaching empty state: "No runs yet — give Kelter its first task." + a button that focuses the composer |
| loading | 3–4 skeleton rows |
| populated | the list |
| disconnected (stale) | list dimmed + "Showing last known runs — reconnecting…" (Flow 07); no destructive clearing |

## Selection & keyboard

- `↑/↓` move selection within the list when it has focus; `Enter` opens the
  selected run in the detail pane.
- `Cmd/Ctrl+N` always creates a new run (composer), regardless of selection.
- Selected row is visibly marked (matches detail pane).

## Scope note

No search/filter/delete in v1 (the API offers no delete; retention/pruning is a
server concern — ADR 0017). Adding client-side filtering is a later enhancement,
not a v1 destination. Kept deliberately simple.

## Copy

- Empty: "No runs yet — give Kelter its first task."
- Stale: "Reconnecting… showing your last known runs."

## Exit

A run is selected into the detail pane (Flow 04/05), or a new run is composed
(Flow 03).
