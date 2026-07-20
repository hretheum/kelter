<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 7 implementation handoff. Maps the spec onto shadcn + Tailwind v4 + Vite embedded via RustEmbed. Written so a Fable implementation prompt writes zero new UX decisions. -->

# 06 — Implementation handoff (bridge to Fable)

Turns the spec into an unambiguous build plan on the chosen stack (shadcn/ui +
Tailwind v4 + Vite, embedded via RustEmbed — ADR 0004). A separate Fable prompt
should be able to build this **without inventing UX**. Where a real backend gap
exists, it is flagged, not papered over.

---

## 1. Stack decision, pinned

- **Build tool:** Vite → `apps/web-ui/dist/` (the `RustEmbed` folder changes from
  `apps/web-ui` to `apps/web-ui/dist` — see §5).
- **Framework:** **React 19** (shadcn's default; ref-as-prop, no `forwardRef`).
  Svelte/Vue ports are allowed but out of scope — pick React unless Eryk says
  otherwise (`07` D-6).
- **UI:** shadcn/ui components **copied into the repo** (`src/components/ui/*`) —
  we own them; no runtime UI dependency.
- **Styling:** Tailwind v4 CSS-first (`@theme` from `05 §2`), single `app.css`.
- **State:** minimal — a thin client over the kernel API (ADR 0004: kernel owns
  all state). No Redux/Zustand needed; React state + a tiny WS/event store.
- **Bundle budget:** ~2–2.5MB. System fonts only, inline SVG icons, tree-shaken
  shadcn. No chart/date megalibraries.

## 1a. Backend dependencies (D-1 + D-2, in scope — net-new endpoints)

The redesign assumes **two new bearer-gated endpoints** (approved by Eryk
2026-07-19). Both are *additive*, touch neither auth nor the kernel-inversion
invariants, and have kernel support already. They are **build dependencies** — the
Stop control and full historical replay depend on them. Ship each behind a
capability flag so, on a deployment without them, the UI **hides** the affordance
rather than showing a dead control (the config-501 rule).

| Endpoint | Verb | Behavior | Kernel hook that exists |
|---|---|---|---|
| `/api/v1/runs/{id}/cancel` | POST | abort the in-flight run + write durable `Canceled`; return the run | `Kernel::cancel_run` + the A2A abort-handle map in `ApiState` (mirror `a2a_cancel`) |
| `/api/v1/runs/{id}/events` | GET | full ordered `RunEvent[]` for the run, replayed from the journal | the durable journal (ADR 0007); reuse the `RunEvent` serialization |
| `/api/v1/runs/{id}/skill-activation` | POST | apply a human's `deny`/`allow_once`/`allow_always` decision on a paused (`awaiting_skill_decision`) run's activation request, then re-spawn execution; return the run | `Kernel::decide_skill_activation` (ADR 0021) |
| `/api/v1/skills` | GET | list installed skills (`SkillView[]`: name/description/trust/provenance/active/has_scripts/has_extension_manifest) | `SkillRegistry::list` via `SkillsController` (ADR 0021 Part 6) |
| `/api/v1/skills` | POST | install from a URL only (`{url}` → `SkillInstallReport`) — folder/bundle install stays CLI-only | `SkillRegistry::install_from_url` |
| `/api/v1/skills/{name}` | DELETE | remove an installed skill | `SkillRegistry::remove` |
| `/api/v1/skills/{name}/activate` | POST | operator-activate (persistent, not run-scoped) | `SkillRegistry::activate` |
| `/api/v1/skills/{name}/deactivate` | POST | operator-deactivate | `SkillRegistry::deactivate` |
| `/api/v1/skills/policy` | GET/POST | read/write `model_activation`/`ask_fallback`/`ask_timeout_secs` | `RegistrySkillGate::get_policy`/`set_policy` (runtime-mutable, ADR 0021 Part 6) |

- **Capability signalling:** the UI probes support (e.g. a `capabilities` field on
  `GET /healthz` or a `404`/`501` from the endpoint) and flags Stop / full-replay
  on/off accordingly. Define the exact signal with the backend during the build
  (small; not a UX decision).
- **Auth:** both go through the same `auth_gate` (bearer + rate limiter) as every
  other route. Cancel is a state-changing POST; replay is read-only.

## 2. Proposed source layout (`apps/web-ui/`)

```
apps/web-ui/
  index.html                # Vite entry (replaces the current hand-written one)
  src/
    main.tsx                # mount
    app.tsx                 # shell: top bar + workspace/settings routing
    lib/
      api.ts                # fetch wrapper (Bearer, 204, err.status) — port of today's api()
      events.ts             # WS connect + bucket-by-run_id + seq ordering
      clipboard.ts          # copy with fallback
      shortcuts.ts          # global keymap (04 §1)
    store/
      connection.ts         # conn state machine (07-flow) + reconnect backoff
      runs.ts               # run list + per-run event buckets
      config.ts             # model config (200/501)
    components/ui/*          # shadcn primitives (button, input, dialog, …)
    features/
      topbar/                # G-1
      connection-sheet/      # G-2
      runs/
        MasterList.tsx       # R-1
        Composer.tsx         # R-2
        Timeline.tsx         # R-3
        Result.tsx           # R-4
        RunDetail.tsx        # status → composer|timeline|result switch
      settings/
        ModelConfig.tsx      # S-1 (mutable + read-only)
    styles/app.css           # @theme + base
  dist/                      # Vite output → RustEmbed (gitignored or built in CI)
```

Old `app.js` / `style.css` / hand-written `index.html` are **removed** (their
logic is ported into `lib/` + `features/`; nothing is lost — mapping in §6).

## 3. Per-screen composition (component → shadcn → props/handlers)

### G-1 Top bar (`features/topbar`)
- Layout: flex row; left drag-region inset for Tauri traffic-lights.
- Model chip → `Button variant=ghost` + `Badge`; onClick → route Settings; text
  from `config.get()` short `policy`.
- Connection pill → `Badge` as `Button`; state from `store/connection`; onClick →
  open G-2. `aria-live` on state text.
- ⚙ → `Button variant=ghost size=icon`; onClick → route Settings; `⌘,` bound in
  `shortcuts.ts`.

### G-2 Connection sheet (`features/connection-sheet`)
- `Dialog` (native popover pattern, `05 Pattern 5`), focus-trap, `Esc` close.
- Fields: Server `Input`, Token `Input type=password`. Connect `Button` (loading).
- Handler: `api("/api/v1/runs")` probe → on ok `events.connect()` + close; on
  error branch 401/429/network → the three copy variants (`03 G-2`).

### R-1 Master list (`features/runs/MasterList`)
- `role=listbox`; rows `role=option`. Row = status glyph + task + time + model
  badge + cost. Data from `store/runs.list`; refresh on `created/completed/failed`.
- `+ New run` pinned `Button`; onClick → `runs.newComposer()` (`⌘N`).
- `↑/↓/Enter/Home/End` per `04 §1`.

### R-2 Composer (`features/runs/Composer`)
- `Textarea` (auto-grow) + `Button` Run + hint + action-space chips.
- keydown handler = the exact contract in `04 §2` (Enter/Shift+Enter/⌘Enter, IME
  guard). Empty → inline error, no POST. On 202 → `runs.bindLive(run_id)` (pane →
  Timeline), clear field, prepend row.
- Empty state: teaching copy + example chips (fill textarea) + `GET
  /api/v1/tools` names (fail-silent).

### R-3 Timeline (`features/runs/Timeline`)
- Subscribes to `store/runs.eventsFor(run_id)` (bucketed, `seq`-ordered).
- Renders step cards via `Collapsible`; args/results collapsed default. Rejected →
  danger chip. Header: task + live `Badge` (pulse) + model + elapsed + cost.
- Auto-scroll state machine (`04 §4`) + "↓ New steps" pill. `role=log`,
  `aria-live=polite`, `aria-busy` while running.
- Terminal event → `runs.markTerminal` → pane swaps to Result in place.

### R-4 Result (`features/runs/Result`)
- Output/error `Card` + `<pre>` + `Copy` button (`lib/clipboard`). Collapsible
  execution record (reuse Timeline cards in static mode for `steps[]`).
- Historical: `GET /runs/{id}` → reconstruct from `steps[]` + honesty note.
- Failed variant adds "New run"; canceled variant renders note. Cost null → "—".

### S-1 Model config (`features/settings/ModelConfig`)
- Provider `RadioGroup`, Preset `Select`, Base URL/Model `Input`, Key
  `Input type=password` (API only). Test/Save `Button`s (loading).
- Mutable (200): full flow from `03 S-1`. Read-only (501): all disabled + banner
  (`Alert`). Save order key→model; result line = 3 test variants incl. SSRF
  refusal. `⌘S` save, `Esc` back.

## 4. Shared client logic (ported, not reinvented)

- `lib/api.ts` — **port of today's `api()`** verbatim in behavior: Bearer header,
  `204 → null`, `err.status` set for 501/401/429 branching. Keep it; it already
  encodes hard-won correctness.
- `lib/events.ts` — WS connect with `?token=`, **bucket by `run_id`**, order by
  `seq`, handle `Lagged` gap and reconnect gap-fill via `GET /runs/{id}/events`
  (D-1 replay), merging replayed events with the live stream by `seq` (dedupe).
  Also used to hydrate a historical run's full axis (R-4). This is the new
  structural piece the current single-`<pre>` lacked.
- `store/connection.ts` — the state machine + reconnect backoff (`Flow 07`).
- Token intake: read `#token=` fragment (as today, `lib/api` init) — the launcher
  contract is unchanged; do **not** add a topology picker (that's the launcher).

## 5. Vite build under RustEmbed (the embed change)

Today `crates/api/src/lib.rs` embeds the folder directly:

```rust
#[derive(RustEmbed)]
#[folder = "../../apps/web-ui"]     // ← currently the hand-written files
struct WebUi;
```

After migration the embedded folder is the **Vite output**:

```rust
#[derive(RustEmbed)]
#[folder = "../../apps/web-ui/dist"]  // ← Vite build artifact
struct WebUi;
```

- `static_asset` fallback (SPA): unknown non-asset paths already fall back to
  `index.html` via the `WebUi::get(path)` + `index.html` default — keep that; it
  is exactly what an SPA router needs.
- **Build ordering:** `apps/web-ui` must `vite build` **before** `cargo build`
  compiles `crates/api` (RustEmbed reads at compile time). Wire this into the
  build: an `apps/web-ui` npm build step in CI + a documented local `pnpm build`,
  and/or a `build.rs` that shells `vite build` (decision D-7). Ship a committed
  `dist/` OR a CI step — do not let `cargo build` embed a stale/empty `dist/`.
- Keep `manifest.webmanifest`, `sw.js`, `icon.svg` as Vite `public/` assets so the
  PWA/service-worker path (browser/remote topology) still works.

## 6. Migration map — what replaces what

| Today | Becomes | Notes |
|---|---|---|
| `index.html` (hand-written sections) | Vite `index.html` + `app.tsx` shell | sections → destinations/states |
| `app.js` `api()` | `lib/api.ts` | behavior preserved (status branching) |
| `app.js` `connectWs/logEvent` | `lib/events.ts` + `Timeline` | now bucketed per run |
| `app.js` `refreshRuns` | `store/runs.ts` + `MasterList` | list unchanged in data |
| `app.js` config block | `features/settings/ModelConfig` | 200/501 both, unchanged API |
| `app.js` `routeByModelHealth` | `store` first-run routing (`Flow 01`) | same logic, new landing |
| `style.css` | `styles/app.css` (`@theme`) | tokens replace ad-hoc vars |
| `setConfigEditable(false…)` | S-1 read-only variant | same honest 501 handling |

The migration itself changes nothing in the *existing* API contract. Two endpoints
are **added** (D-1 replay, D-2 cancel — §1a), in scope per Eryk, feature-flagged so
the UI degrades gracefully where they're absent.

## 7. Definition-of-done hooks for the implementer

- The 5-second test (`brief §7.1`): composer is labeled "Task for Kelter", has the
  only input+primary button; timeline has no input — verify with 3 users.
- Every screen in `03` builds its empty/loading/error variants (`§7.2`).
- Keyboard contract from `04` fully wired (Enter/Shift+Enter/⌘Enter + nav).
- Contrast + focus verified against `08` (WCAG AA).
- `vite build` produces `dist/` < ~2.5MB; `cargo build` embeds it; both topologies
  (standalone webview + browser/remote) load the same artifact.
- Read-only (501) and disconnected states are reachable and correct (not just the
  happy path).
