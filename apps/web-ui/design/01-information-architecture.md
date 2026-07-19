<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 2 (information architecture, navigation model, screen map). Derived from 00-jobs-and-objects.md. This is the artifact the Stage-2 human gate protects. -->

# 01 — Information architecture

Stage 2. Derived strictly from `00-jobs-and-objects.md`. This is the document the
**human gate #1 (Eryk)** protects: if the object model and destination count are
right, the screens and visuals in later stages follow almost mechanically. If
they are wrong, everything downstream is wrong.

---

## 1. The core claim: two destinations, not eight

The brief's §3 lists eight flows. **They are not eight destinations.** Mapped onto
the object model, they collapse:

| §3 flow | Is it a destination? | Where it lives |
|---|---|---|
| 1. First-run / topology | **No** — owned by the launcher (Tauri), before web-ui loads | out of web-ui scope; web-ui reads the *result* (a token) |
| 2. Model / policy config | **Yes** — a genuinely separate concern, off the loop | **Settings** |
| 3. Compose a run | No — a state of the run object | Runs · composer |
| 4. Observe execution (live) | No — a state of the run object | Runs · timeline |
| 5. Run result | No — a state of the run object | Runs · result |
| 6. Run history / list | No — the master view of the run object | Runs · master list |
| 7. Reconnection | **No** — ambient status, not a place you navigate to | global chrome |
| 8. Global navigation | n/a — it *is* the nav model | the top bar |

**Result: exactly two top-level destinations — `Runs` and `Settings` — plus
ambient connection chrome.** This is the honest minimum the brief's
FAILURE/ESCALATION clause explicitly asks us to prefer ("if correct IA needs
fewer screens, say so and simplify; do not add screens for show").

### Why two, justified

- **Runs is the product.** The primary loop (J1–J4) is one object seen in four
  states. Splitting compose / watch / result / history into separate places is
  what *created* the current confusion — the input drifted away from the output.
  Keeping them as one **master–detail workspace** binds output to the run that
  produced it and gives the composer a fixed, unmistakable home.
- **Settings is the only true second place.** Config (J5) is rare, setup-shaped,
  and the brief (§7.3) mandates it be **off** the primary loop. It is a
  destination because it is a mode you *leave the loop to enter and then return
  from* — not a panel you keep open while working.
- **Connection (J6) is not a destination.** You never "go to" connection; it is a
  persistent truth about the session. It belongs in chrome (a status control that
  can open a small sheet), never as a sibling screen.

Anything more (a Tools browser, a Learned-facts inspector, an A2A console) would
be adding screens for completeness the daily loop does not need. Those APIs exist
but are deferred to `07-open-decisions.md`, not promoted to destinations.

---

## 2. Navigation model

A **master–detail workspace** with a persistent top bar. macOS-flavored: the top
bar is the window's title region; the master list is a source-list sidebar; the
detail pane is the run.

```
┌───────────────────────────────────────────────────────────────┐
│  ● Kelter      [ model: sonnet-5 ▾ ]              ◉ connected  ⚙│  ← top bar (chrome)
├──────────────────┬────────────────────────────────────────────┤
│  + New run       │                                             │
│ ───────────────  │                                             │
│  ▸ Run  running  │            DETAIL PANE                      │
│  ▸ Run  done     │   (composer  |  live timeline  |  result)   │
│  ▸ Run  failed   │                                             │
│  ▸ Run  done     │                                             │
│   … history …    │                                             │
│                  │                                             │
│  master list     │                                             │
└──────────────────┴────────────────────────────────────────────┘
```

### 2.1 Top bar (always present)

- **App identity** + **connection status** pill (`00 §3.4` states). Click the
  pill → connection sheet (remote URL/token; the bare-browser case).
- **Current model** chip (read from `GET /config/model`) — *ambient awareness*,
  not an editor. Click → opens **Settings**. This satisfies "user always knows
  which model is running" without putting config on the loop.
- **Settings (⚙)** — opens the Settings destination. Keyboard: `Cmd/Ctrl+,`.

### 2.2 Master list (the run source-list)

- **`+ New run`** pinned at the top — the single, unambiguous entry into J1. This
  is the *only* thing that opens the composer. Keyboard: `Cmd/Ctrl+N`.
- Runs newest-first: status glyph + truncated task + relative time; a running run
  shows a live indicator. Selecting a run loads it into the detail pane.
- This is the home surface. On load, the detail pane shows either the composer
  (no run selected / first run) or the most recent run.

### 2.3 Detail pane (the run, in whatever state it's in)

One pane, three faces chosen by run status — never three separate places:

- **Composer** — when "New run" is active (no run bound yet). The input surface.
- **Live timeline** — when the bound run is `pending`/`running`. The execution
  axis streams here.
- **Result** — when the bound run is `completed`/`failed`/`canceled`. Output or
  error, copyable.

A live run flows composer → timeline → result *in place*: you submit, the same
pane becomes the timeline, then settles into the result. The output literally
appears where the run lives, next to (not under) the input that started it. This
is the structural fix for "is this a console?".

### 2.4 Settings destination

Reached from the top bar (model chip or ⚙). It **replaces the workspace** (full
route/view), with a clear "Done/back to runs" return — reinforcing that you left
the loop to configure and come back. Not a slideover you keep open. Contains only
Model configuration for now (`00 §3.3`), including the read-only (501) variant.

### 2.5 Connection sheet (ambient, modal-ish)

Opened from the status pill. Holds the remote URL + token form (bare browser) and
reconnect. Does not exist as a route; it is a transient surface over the
workspace. In the launcher-provisioned case it is rarely needed (token came via
fragment), matching today's `showSetup(!hashToken)`.

### 2.6 Navigation map (state, not pages)

```
                    ┌─────────────── top bar (persistent) ───────────────┐
                    │  connection pill        model chip           ⚙     │
                    └───────┬───────────────────────┬────────────────────┘
                            │ click                  │ click
                            ▼                        ▼
                    connection sheet            SETTINGS  ◀── Cmd+,
                    (remote/reconnect)          (model config; RO if 501)
                            
   RUNS  (home) ───────────────────────────────────────────────────────────
     master list ──select──▶ detail: timeline (running) / result (terminal)
        │
        └── + New run  (Cmd+N) ──▶ detail: composer ──submit(Enter)──▶ timeline ──▶ result
```

Two destinations (`Runs`, `Settings`), two ambient surfaces (connection sheet,
which is chrome-triggered). No deeper nesting.

---

## 3. Screen map (destinations → screens → states)

Every screen's full spec lands in `03-screens/` after the gate. Here is the
inventory the gate is approving, each with the states it must define (`00`
supplies the state machines). Empty / loading / error are called out per the
brief's §7.2.

### Runs (destination)

| Screen / region | States to spec |
|---|---|
| **R-1 Master list** | empty (no runs yet) · loading · populated · disconnected (stale) |
| **R-2 Composer** (detail) | empty (with action-space hint) · typing · submitting · validation-error (empty task → mirrors API 422) · disabled (disconnected) |
| **R-3 Live timeline** (detail) | pending/starting · streaming (steps arriving) · step expanded · rejected-visible · auto-scroll vs user-scrolled-up |
| **R-4 Result** (detail) | completed (output + copy) · failed (error + copy) · canceled (found in history) · historical (executed-record axis, no live proposals) |

### Settings (destination)

| Screen | States to spec |
|---|---|
| **S-1 Model config** | loading · editable (mutable/200) · testing · test-ok · test-failed · test-refused (SSRF) · saving · saved · error · **read-only (501)** |

### Global chrome (not destinations)

| Surface | States to spec |
|---|---|
| **G-1 Top bar** | connected · connecting · disconnected · auth-failed; model chip present/absent |
| **G-2 Connection sheet** | idle · connecting · error (401/429 vs network) · success |

**9 specced surfaces, 2 destinations.** Note there is *no* separate onboarding/
topology screen in web-ui — that is the launcher's job (documented as a boundary,
not a gap).

---

## 4. Responsive / window behavior (set now, detailed later)

- **Desktop window (default, ~1100×800, Tauri):** master–detail side by side.
- **Narrow (< ~720px, browser/remote on a small window):** master list and
  detail become a single back-navigable stack (list → run → back). Top bar
  persists; the model chip collapses to an icon.
- The whole app is one web artifact (ADR 0004) serving both the webview and a
  plain browser tab; nothing here assumes Tauri except the launcher, which is out
  of scope.

---

## 5. IA self-critique (pre-gate)

Honest pass over this IA before asking for approval. Full `design-critique` +
`accessibility-review` run on the *visuals* in Stage 8; this is the *structural*
critique the gate needs.

1. **"Is two destinations too few — will Runs get overloaded?"** The risk is real
   if the detail pane tries to be four things at once. It does not: status
   *selects* one face at a time (composer XOR timeline XOR result). The master
   list carries history. No single view is overloaded; the object's state does
   the routing. **Mitigation accepted.**
2. **"Where does a *new* run go while a previous one is still running?"** The
   master list holds many runs; "New run" opens a fresh composer in the detail
   pane while the running one keeps streaming in the background (its list row
   shows the live indicator). Selecting it returns to its timeline. This is only
   coherent *because* the WS is a global broadcast we bucket by `run_id` (`00
   §3.2`) — the architecture supports concurrent runs; the IA must too.
   **Confirmed the IA matches the runtime.**
3. **"Model chip in the top bar — is that config creeping onto the loop?"** No: the
   chip is read-only *awareness* and a doorway; editing happens only in Settings,
   off the loop. Line held per §7.3. But it is a judgment call worth Eryk's
   confirmation → `07-open-decisions.md` D-4.
4. **"Historical runs are lossy (no proposals/rejections)."** Surfaced as D-1;
   **resolved after gate #1 (Eryk, 2026-07-19)** — journal replay
   (`GET /runs/{id}/events`) is now in scope, so history shows the full axis
   (R-4, Flow 05, `00 §3.2`), with the honest reconstructed-record view as the
   flagged-off fallback. Not an IA flaw; a scope choice, now settled.
5. **"No Cancel affordance."** Surfaced as D-2; **resolved after gate #1 (Eryk,
   2026-07-19)** — `POST /runs/{id}/cancel` is now in scope and the live timeline
   gains a **Stop** (R-3), feature-flagged off where the endpoint is absent (never
   a dead button — the config-501 rule holds). **The IA refuses to
   promise what the backend can't do.**
6. **"Connection as chrome, not a screen — is reconnection discoverable enough?"**
   The pill changes state visibly and becomes the click target for the sheet;
   `disconnected` is not a silent state. Detailed in G-1/G-2. **Acceptable.**

No structural blocker found. The one item genuinely needing a human call before
visuals is **D-4** (model chip on the top bar) and the two backend-shaped
decisions **D-1 / D-2**, all recorded for the gate.

---

## 6. What the gate is approving

1. **Object model** (`00`): Run as first-class; the live/historical asymmetry;
   the config-mutability and connection states as real application states.
2. **Two destinations** — `Runs` (master–detail workspace) + `Settings` — with
   **connection as ambient chrome** and **topology owned by the launcher**.
3. **The navigation model** in §2 and the **screen inventory** in §3.

If approved, Stage 3+ produces `02-flows/`, `03-screens/`, `04-interaction-spec`,
`05-design-system`, `06-implementation-handoff`, `07-open-decisions`, and the
Stage-8 critique/a11y pass — no further structural decisions, only detailing this
skeleton. Per the brief, **visuals are not polished before this is accepted.**
