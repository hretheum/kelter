<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: connection loss & reconnection, distinguishing network drop from auth failure. -->

# Flow 07 — Reconnection / connection loss

**Job:** J6. **Surface:** global chrome — top-bar pill (G-1) + connection sheet
(G-2). **Signals:** WS `onclose`, REST error, `401`/`429` from `auth_gate`.

## Principle

The kernel owns all state and runs are durable (journal-backed). A dropped socket
**does not lose a run** — it keeps executing on the kernel and resumes on restart.
So reconnection is about *restoring the view*, never about data loss. Copy must say
so, to prevent panic.

## States & transitions (from `00 §3.4`)

```
connected ──WS onclose / REST fail──▶ disconnected ──auto-retry──▶ connecting ──▶ connected
                                             │
                                    401/429 ▼
                                       auth-failed  (needs a different fix)
```

| State | Pill | What the user sees / does |
|---|---|---|
| connected | ◉ green "connected" | normal |
| connecting | ◐ amber "connecting…" | transient; controls that need the socket disabled briefly |
| disconnected | ○ red "disconnected" | banner "Connection lost — your runs are safe on the kernel. Reconnecting…"; auto-retry with backoff; pill click opens G-2 |
| auth-failed | ⚠ red "auth failed" | G-2 opens: 401 → "Your token was rejected — check it." / 429 → "Too many attempts — wait ~30s and retry." (distinct remedies) |

## Steps

1. **Detect** — WS `onclose` or a REST call throwing; set `disconnected` and show
   the non-blocking banner. In-flight timeline shows the mid-run banner (Flow 04).
2. **Auto-retry** — reconnect the WS with backoff (e.g. 1s→2s→5s→10s cap). Each
   attempt flips the pill to `connecting`.
3. **Recover** — on success: pill `connected`, banner clears, `GET /runs` refresh
   reconciles the list, a running run's timeline re-binds and **catches up from
   `GET /api/v1/runs/{id}/events`** (D-1) — the full ordered event list from the
   journal, so proposals/rejections missed during the gap are recovered, not lost.
   Live WS events resume from there. (Fallback: `steps[]` + terminal state if
   replay is unavailable.)
4. **Auth failure** — do not silently retry forever; surface G-2 so the user can
   fix the token (remote) — a bad token won't fix itself with backoff.

## Bare-browser vs launcher

- Launcher-provisioned (fragment token): reconnection is automatic; the sheet is
  rarely shown.
- Bare browser: G-2 is the primary remedy (re-enter URL/token). This is the same
  form as first connect.

## Copy

- Drop banner: "Connection lost — your runs are safe on the kernel. Reconnecting…"
- Recovered (brief): "Reconnected."
- 401: "Token rejected. Check your API token." · 429: "Too many attempts. Wait a
  moment and try again."

## Exit

Back to `connected`; the view is reconciled without the user losing their place or
their runs.
