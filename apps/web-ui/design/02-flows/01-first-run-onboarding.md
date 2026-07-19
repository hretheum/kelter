<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: first-run / onboarding. Boundary with the Tauri launcher is explicit. -->

# Flow 01 — First-run / onboarding

**Job:** J6 + J5 setup. **Destinations touched:** launcher (out of web-ui) →
Runs → (maybe) Settings.

## Boundary (critical)

Topology is chosen in the **Tauri launcher** (`apps/desktop/dist/`), *before* the
web-ui loads. The launcher hands the web-ui a `url` + `token` (token via URL
fragment `#token=…`). The web-ui therefore **never asks about topology** and never
shows a standalone/remote picker. Its "first run" is only: *am I connected, and is
a model reachable?* A bare browser tab (no launcher, no fragment) is the one case
where the web-ui must collect a URL + token itself.

## Entry conditions

| How web-ui was opened | What it has | First screen |
|---|---|---|
| Desktop standalone (launcher) | token via fragment, same-origin | auto-connect → model-health routing |
| Desktop remote (launcher) | token + remote url via fragment | auto-connect → model-health routing |
| Bare browser (no fragment) | nothing | **G-2 connection sheet** (URL + token) |

## Steps (happy path, launcher-provisioned)

1. Web-ui reads the fragment token → auto-connects (`GET /api/v1/runs` + WS open).
2. **Connection resolves** → top bar pill = `connected`, model chip populated from
   `GET /api/v1/config/model`.
3. **Model-health routing** (the real first-run decision, from today's
   `routeByModelHealth`):
   - `GET /config/model` → **200** and `POST /config/model/test` → `ok:true` →
     land on **Runs** (R-1 master list + composer). Done.
   - `GET` **200** but test **not ok / unreachable** → land on **Settings (S-1)**
     with a "let's point Kelter at a reachable model" framing.
   - `GET` **501** (immutable deploy) → land on **Runs**; Settings is read-only
     (the model is already fixed at deploy time — nothing to configure).
   - `GET` other error → land on **Settings (S-1)** editable (first-run configure).

## States

- **Loading** — "Connecting to your kernel…" on the workspace shell (skeleton
  master list + a centered spinner in the detail pane). Never a blank window.
- **Empty (connected, no runs, model OK)** — Runs with an **empty master list**
  and the composer focused, showing its teaching empty state (see Flow 03 / R-2).
- **Error (can't connect)** — G-2 connection sheet with the failure reason
  (distinguish 401/429 auth from network — Flow 07).

## Copy

- Loading: "Connecting to your kernel…"
- Route-to-settings reason: "Kelter can't reach a model yet. Pick one and test the
  connection to get started." (never a bare error dump)
- Immutable (501) landing note in Settings: "This deployment's model is fixed at
  deploy time." (read-only, honest)

## Exit

User is on Runs with a focused composer (the teaching first step), or on Settings
having been told exactly why. No dead run-view, ever.
