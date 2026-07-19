<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: global navigation between the two destinations + ambient surfaces. -->

# Flow 08 — Global navigation

**Job:** the connective tissue for J1–J6. **Surfaces:** top bar (G-1) + the two
destinations (Runs, Settings) + connection sheet (G-2).

## Model

Two destinations, reached from a persistent top bar; connection is chrome. There
is no deep hierarchy — you are always one action from anywhere.

```
        ┌───────────────── TOP BAR (persistent) ─────────────────┐
        │ ●Kelter   [model: sonnet-5 ▾]        ◉ connected    ⚙  │
        └───┬───────────────┬──────────────────────┬─────────────┘
            │ pill          │ model chip           │ ⚙ / Cmd+,
            ▼               ▼                       ▼
     connection sheet    SETTINGS (S-1)          SETTINGS (S-1)
        (G-2)            model config            model config
            
   ── RUNS (home) ─────────────────────────────────────────────
      master list ⇄ detail (composer | timeline | result)
```

## Rules

- **Runs is home.** The app always opens to Runs (after first-run routing).
  Selecting a run or composing one stays within Runs.
- **Settings is a round-trip.** Entering Settings replaces the workspace; `Esc` or
  "Done" returns to Runs exactly where you were. You do not "browse" between them;
  you leave the loop and come back.
- **Connection is never a destination.** The pill reflects state and opens the
  sheet (G-2); the sheet closes back onto whatever you were doing.
- **The model chip** is read-only awareness + a Settings doorway (approved at gate
  #1). It never edits inline.

## Keyboard (global — full list in `04-interaction-spec.md`)

| Shortcut | Action |
|---|---|
| `Cmd/Ctrl+N` | New run (focus composer) |
| `Cmd/Ctrl+,` | Open Settings |
| `Esc` | Close Settings / connection sheet → back to Runs |
| `Cmd/Ctrl+K` | Focus the master list (jump to history) — optional, D-5 |
| `↑/↓ + Enter` | Navigate + open runs in the master list |

## Focus handoff on view change

- Composer opened → focus the task field.
- Run selected → focus the detail pane header (screen-reader announces the run).
- Settings opened → focus the first control; `Esc` returns focus to the top-bar
  ⚙ that opened it.
- Sheet opened → focus-trap inside; `Esc` restores focus to the pill.

## States

No screen of its own — navigation is expressed through the destinations' states.
The one persistent element (top bar) has the states in G-1 (connection) + the
model chip (present when config is readable, hidden/"—" when unknown).

## Exit

n/a — this *is* the connective flow; every other flow enters/exits through it.
