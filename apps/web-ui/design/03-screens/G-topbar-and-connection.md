<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec G-1 top bar + G-2 connection sheet (global chrome). -->

# G-1 / G-2 — Top bar & connection sheet (global chrome)

Persistent chrome, not destinations. G-1 is always present; G-2 is a transient
sheet opened from the connection pill. Owns J6 + the doorways to Settings.

## G-1 — Top bar

### Structure

```
┌───────────────────────────────────────────────────────────────┐
│  ● Kelter      [ sonnet-5 ▾ ]              ◉ connected      ⚙  │
│    identity      model chip                 conn pill    settings│
└───────────────────────────────────────────────────────────────┘
```

- **Skip link** (a11y, `08`): a visually-hidden "Skip to runs" is the very first
  focusable element; appears on focus, jumps to `#main` (the workspace). Bypasses
  the top bar for keyboard/SR users (WCAG 2.4.1).
- **Identity**: "Kelter" wordmark (left). In the Tauri window this sits in the
  draggable title region (native traffic-lights to its left; add left inset).
- **Model chip** (approved gate #1): read-only, short `policy` (e.g. `sonnet-5`).
  Click → Settings (S-1). Absent/"—" when config unreadable.
- **Connection pill**: the J6 status (below). Click → G-2 sheet.
- **Settings ⚙**: opens S-1 (`Cmd/Ctrl+,`).

### Connection pill states

| state | pill | color token |
|---|---|---|
| connected | ◉ connected | success |
| connecting | ◐ connecting… | muted/amber |
| disconnected | ○ disconnected | danger |
| auth-failed | ⚠ auth failed | danger |

- Non-color cue always present (glyph + text), never color-only.
- On `disconnected`, a non-blocking banner also appears in the workspace (Flow 07)
  — the pill alone isn't the only signal.

### Accessibility

- Pill is a `<button>` with `aria-label` reflecting state ("Connection: connected.
  Open connection settings.") and `aria-live="polite"` on state change.
- Model chip is a `<button>` "Model: sonnet-5. Open model settings."

## G-2 — Connection sheet

Opened from the pill. Holds the remote URL + token (bare-browser case) and
reconnect. Focus-trapped; `Esc` closes back to Runs.

### Structure

```
┌─ Connection ─────────────────────  [ ⎋ ] ┐
│  Server    [ http://homelab:7466        ] │  ← empty = same origin (standalone)
│  API token [ ···················        ] │
│  [ Connect ]                              │
│  ⚠ Token rejected. Check your API token.  │  ← error line (context-specific)
└───────────────────────────────────────────┘
```

### Variants / states

| State | Rendering |
|---|---|
| idle | fields (prefilled from stored base/token if any) + Connect |
| connecting | Connect → spinner "Connecting…" |
| error-network | "Can't reach {url}. Check the server address." |
| error-401 | "Token rejected. Check your API token." |
| error-429 | "Too many attempts. Wait ~30s and try again." |
| success | sheet closes; pill → connected; list refreshes |

### Behavior (from Flow 07)

- Bare browser: primary remedy — enter URL/token, `Connect` = `GET /runs` probe →
  open WS on success. Launcher-provisioned: rarely shown (token via fragment);
  used for reconnect/switch-daemon.
- Distinguish auth (401/429) from network in copy — different remedies.

### Accessibility

- `role="dialog"` `aria-modal="true"`, labelled by "Connection"; focus first
  field on open, restore focus to the pill on close.
- Token field `type="password"` `autocomplete="off"`; error via `aria-describedby`.

## API

- `GET /api/v1/runs` (auth probe on Connect), WS `/api/v1/events?token=`,
  `GET /api/v1/config/model` (model chip). 401/429 surfaced distinctly.
