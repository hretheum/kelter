<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 1 (jobs-to-be-done + domain object model + states). Grounded in the real code, not assumptions. -->

# 00 — Jobs & objects

Stage 1 of the redesign. Nothing here is invented: every object, field and state
below is read from the running code (`crates/kernel-core/src/engine.rs`,
`crates/api/src/lib.rs`, `crates/model-config/src/lib.rs`,
`apps/desktop/src-tauri/src/lib.rs`). The IA in `01` is derived from this model —
if this model is wrong, stop here, not three artifacts later.

---

## 1. What Kelter actually is (grounding)

A desktop client for a **deterministic agent runtime**. The user gives it a
task; a swappable model **proposes** actions one at a time; the kernel
**validates** each proposal against a schema and **executes** only what passes,
journaling everything. The product's soul is not "a chatbot" — it is *watching a
governed agent work and trusting that a kernel is checking it*. The UI's job is
to make that legible.

The real primary loop, confirmed in code:

```
POST /api/v1/runs {task}      → a durable Run is created, executes in background
WS   /api/v1/events?token=…   → RunEvent stream (created→proposed→[rejected*]→executed→…→completed|failed)
GET  /api/v1/runs/{id}        → the durable Run (steps[], output, error, policy, cost)
GET  /api/v1/runs             → all runs, for history
```

**A Run is the first-class object.** It has its own identity, durable history,
and terminal result. Everything the user does orbits a run: compose one, watch
one, read one, revisit one.

---

## 2. Jobs-to-be-done

Ranked by how central they are to the primary loop. J1–J4 are the loop; J5–J6
are setup/ambient that must exist but must **not** sit on the loop.

| # | Job (user's words) | Trigger | Success | Frequency |
|---|---|---|---|---|
| **J1** | "I have a task — do it, and show me the work." | User has intent | A run is created from clean input; user is never confused about where they typed vs where output appears | every session, many× |
| **J2** | "Let me watch it think and act, and see the kernel checking it." | A run is running | User reads a legible execution axis (proposed → validated → executed → result), not a raw log; understands *this is output, not my input* | every run |
| **J3** | "Give me the result — let me copy/use it." | A run completed or failed | Output (or the failure reason) is readable and copyable in one gesture | every completed run |
| **J4** | "What did I run before, and how did it go?" | User returns / wants to compare | Past runs are listed with status + task + model + cost; one click opens the detail | often |
| **J5** | "Point Kelter at the right model and prove it's reachable." | First run, or switching model | Model is picked, tested against the real provider, saved; or (immutable deploy) shown honestly as read-only | rare — setup |
| **J6** | "Am I connected to my kernel? Reconnect me if not." | Connect / drop | Connection state is always visible; a drop is obvious and recoverable without losing place | ambient |

**Design consequence.** J1–J4 are four *facets of one object* (the run), not four
destinations. J5 is a genuinely separate concern (configuration) that belongs
off the loop. J6 is ambient status, not a screen. This is the seed of the IA
in `01`: **the run workspace + settings, with connection as chrome.**

**The failure we are correcting (from the current UI).** Today `submit-box`,
`runs`, and `events` are three sibling `<section>`s stacked vertically, and the
"Run" button sits *above* a `<pre id="event-log">` that dumps every run's events
together. The eye reads the big scrolling text area as a console *input*. The
root cause is IA + object model: there is no run-centric container, and output
is not visually bound to the run that produced it. We fix it at the source, not
with a label.

---

## 3. Domain objects & their states

### 3.1 Run — the first-class object

Fields (verbatim from `Run` in `engine.rs`):

| Field | Type | UI meaning |
|---|---|---|
| `id` | UUID | run identity; short 8-char prefix for display |
| `task` | string | what the user asked (the title of the run) |
| `status` | enum | drives every visual state (below) |
| `steps[]` | `StepRecord[]` | durable executed actions: `{index, action, arguments, result, ok}` |
| `output` | JSON \| null | the result, when completed |
| `error` | string \| null | the failure reason, when failed |
| `policy` | string | which model drove it, e.g. `openrouter:anthropic/claude-sonnet-5` |
| `created_at` | u64 (epoch s) | for ordering + relative time |
| `usage` | Usage | model-call accounting |
| `agent_usage` | map | per-remote-agent accounting (A2A) |
| `trust_posture` | (name,label)[] | which extensions ran sandboxed/unsandboxed |
| `cost_usd` | f64 \| null | dollar cost; **null = unpriced, never show $0.00 as if real** |

**Run status state machine** (`RunStatus`, all five are real):

```
            create_run
   (none) ─────────────▶ Pending ──▶ Running ──┬─▶ Completed   (output present)
                                                ├─▶ Failed      (error present)
                                                └─▶ Canceled    (A2A cancel only today)
```

- **Pending** — created, not yet stepping. Brief; UI shows "starting…".
- **Running** — the model is proposing/kernel executing. The *live* state; the
  execution axis streams here.
- **Completed** — terminal; `output` is the payload of J3.
- **Failed** — terminal; `error` is the payload of J3 (an error is a first-class
  result, not an alert to dismiss).
- **Canceled** — terminal. Reachable from the A2A `tasks/cancel` path and now
  **from the web UI** via D-2 (approved 2026-07-19): `POST /api/v1/runs/{id}/cancel`
  mirrors the kernel's existing `cancel_run` + abort-handle. The live timeline
  gains a **Stop** control (R-3). `POST /runs/{id}/cancel` is a **net-new backend
  endpoint** the UI assumes (build dependency — `06 §1a`); until it lands, Stop is
  feature-flagged off (never a dead button).

### 3.2 Run event stream — the execution axis (per run, live)

`RunEvent` (tagged union, `engine.rs`), delivered in a `RunEventEnvelope`
`{run_id, seq, event}` over the WS. **The socket is a global broadcast of every
run's events** — the client MUST bucket by `run_id` and order by `seq` to build a
single run's axis. This is the technical fact the current UI ignores.

| Event | Payload | Axis meaning |
|---|---|---|
| `created` | `{task, policy}` | run opened |
| `proposed` | `{step, action, arguments, rationale?}` | model *wants* to do X (with optional ReAct "why") — **not yet done** |
| `rejected` | `{step, reason, attempt}` | kernel refused the proposal; model will retry — **the governance moment, made visible** |
| `executed` | `{step, action, ok, result}` | kernel let it run; here's the result |
| `reflection_written` | `{episode_id}` | a memory episode was recorded (quiet) |
| `completed` | `{output}` | terminal success |
| `failed` | `{error}` | terminal failure |

**A step's own lifecycle** (what the axis renders per step index):

```
proposed ──▶ rejected (attempt n) ──▶ proposed (attempt n+1) ──▶ … 
proposed ──▶ executed (ok:true)    ─ the happy path
proposed ──▶ executed (ok:false)   ─ ran but the tool failed
```

The `proposed → validated → executed → result` framing the brief asks for maps
exactly onto `proposed → (no rejection) → executed → result`. A `rejected` is
the *visible* proof the kernel is validating — it is a feature to surface, not
noise to hide.

**Live vs historical (resolved — D-1 in scope).** `proposed`, `rejected`, and
`rationale` are broadcast events, not fields on the durable `Run`. The WS only
carries them **live**. To make history equally rich, D-1 (approved 2026-07-19)
adds `GET /api/v1/runs/{id}/events` — a bearer-gated replay of the full ordered
`RunEvent` list from the journal (ADR 0007 already persists it). So:

- A run watched **live** shows the rich axis from the WS.
- A run opened from **history** fetches `GET /runs/{id}/events` and shows the
  **same full axis** (proposals, retries, rationale), falling back to `steps[]` +
  terminal output/error only if replay is unavailable (feature-flag off / older
  deployment).

`GET /runs/{id}/events` is a **net-new backend endpoint** the UI spec now assumes
(build dependency — `06 §1a`). Until it lands, R-4 renders the honest
reconstructed-record view (no fake proposals) behind the flag.

### 3.3 Model / policy configuration — the setup object

From `ModelConfigView` (`api`) + `DaemonModelConfig` (`model-config`):

| Field | UI meaning |
|---|---|
| `engine` | `provider:model`, e.g. `local:qwen2.5-0.5b-instruct` |
| `provider` | `local` \| `ollama` \| `openrouter` (drives the health probe) |
| `model` | model id |
| `base_url` | provider endpoint |
| `api_key_set` | **bool only — the key value is never returned** |

**Config mutability state (this is a real application state, not an edge case):**

```
GET /api/v1/config/model
   ├─ 200 ─▶ MUTABLE   → editable form, Test, Save, hot-swap
   └─ 501 ─▶ IMMUTABLE → read-only, "configured at deploy time" (honest)
```

Both standalone desktop and daemon-with-controller are **mutable** (200) after
the Phase 8.5 follow-up. A daemon without a controller is **immutable** (501).
The UI must render both — an editable form that would 501 on Save is exactly the
half-baked state we forbid.

**Test-connection sub-states:** idle → testing → ok (real `2xx` from provider) →
failed (real error) → **refused** (SSRF allowlist: origin not permitted). The
allowlist refusal is a distinct, honest outcome, not a generic failure.

**Key handling invariant (must be honored in copy + behavior):** the key is
write-only from the UI's perspective — POSTed to `.../key`, stored `0600`, never
echoed, never logged, `api_key_set` is the only readback. UI copy must say
"leave blank to keep the stored key."

### 3.4 Connection / topology — the ambient object

Two axes, both real:

**Topology** (decided by the launcher *before* web-ui loads — ADR 0004):
- **standalone** — kernel in-process; same-origin; token via URL fragment.
- **remote** — webview/browser points at a remote daemon URL; token via fragment
  or the bare-browser connection form.

The web-ui **never chooses topology** — the launcher already did, and a plain
browser tab can't start an in-process kernel anyway. The web-ui only *knows* it
via how it got its token (fragment = provisioned by launcher/pairing; empty =
bare browser must fill the form).

**Connection state** (live, ambient):

```
                 auto-connect / Connect
   disconnected ───────────────────────▶ connecting ──┬─▶ connected   (REST ok + WS open)
        ▲                                              └─▶ auth-failed (401/429) 
        └──────────────── WS onclose / REST error ─────────── drop
```

- `connected` — REST reachable and WS open (`setConn(true)` today).
- `connecting` — request in flight.
- `disconnected` — WS closed or REST failed; the reconnect affordance appears.
- `auth-failed` — 401 (bad token) or 429 (rate-limited); distinct from a network
  drop, needs a different remedy (fix token vs wait/retry).

**Cancel (resolved — D-2 in scope).** The primary spec puts a live **Stop**
control on the timeline (R-3), backed by the net-new `POST /api/v1/runs/{id}/cancel`
(build dependency, `06 §1a`; feature-flagged off until the endpoint lands, so it is
never a dead button).

### 3.5 Action space (`GET /api/v1/tools`) — latent, currently unused

The API exposes the live action set (`{name, description}[]`), but no screen uses
it. It is a strong candidate for the composer's empty state ("here's what this
agent can do") without becoming its own destination. Treated as an enhancement,
not a new section — `07-open-decisions.md` item D-3.

---

## 4. Object → job → surface (the bridge to IA)

| Object | Serves jobs | Lives in (see `01`) |
|---|---|---|
| Run (compose) | J1 | Run workspace · composer |
| Run (live) | J2 | Run workspace · timeline |
| Run (terminal) | J3 | Run workspace · result |
| Run (list) | J4 | Run workspace · master list |
| Model config | J5 | Settings (off the loop) |
| Connection | J6 | Global chrome (status + sheet) |
| Action space | J1 (assist) | Run workspace · composer empty state |

Four of seven surfaces are facets of **one object in one workspace**. That is the
whole argument for a small IA, made in `01`.
