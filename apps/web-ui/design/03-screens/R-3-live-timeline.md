<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec R-3 live timeline (execution axis). The core J2 screen. -->

# R-3 — Live timeline (detail pane, running run)

The execution axis. Renders one run's `RunEvent` stream (bucketed by `run_id`,
ordered by `seq`) as a step-keyed narrative — the opposite of a raw console. Owns
J2. **No editable field, no primary button** — structurally not-an-input.

## Structure & hierarchy

```
┌─ detail: timeline ───────────────────────────────────────┐
│  Summarize the RFC and file it  ◐ Kelter working…  [Stop]│  ← run header + Stop (D-2)
│  sonnet-5 · 00:07 · $0.004                                │  ← model · elapsed · running cost
│  ─────────────────────────────────────────────────────── │
│  ①  read_url                              validated ✓    │  ← step card (collapsed)
│      → 200, 4.2 KB                                        │
│  ②  write_file                                            │
│      ✕ rejected: path escapes workspace · retry 1        │  ← governance made visible
│  ②  write_file  {path:"out/summary.md"}   validated ✓    │  ← retry succeeded (expanded)
│      → wrote 1.1 KB to out/summary.md                    │
│  ③  finish                                running…       │
│                                                           │
│                                   [ ↓ New steps ]         │  ← appears only when scrolled up
└───────────────────────────────────────────────────────────┘
```

## Step card anatomy

- **Index** (`step`), **action** name, optional **rationale** (ReAct "why", muted,
  one line, expandable).
- **State line**: `proposed` → `validated ✓` / `rejected ✕ {reason} · retry n` /
  `executed ok:false ✕`.
- **Arguments / result**: collapsed by default; click to expand JSON; long content
  scrolls within the card.
- Cards animate in (subtle fade/slide, ≤150ms; respects reduced-motion).

## Header

- Task title (full, wraps).
- Live status badge: `◐ Kelter working…` (running) with a soft pulse.
- **Stop** control (D-2): `secondary`, danger-tinted; the only action on the
  timeline. Click → inline confirm ("Stop this run? · Confirm / Keep going") →
  `POST /api/v1/runs/{id}/cancel` → "Stopping…" → Canceled (R-4). Hidden when the
  cancel endpoint is flagged off (never shown-but-dead). `aria-label="Stop run"`.
- Meta row: model (`policy`), elapsed timer, running `cost_usd` (omit if null).

## Variants / states

| State | Rendering |
|---|---|
| pending/starting | header + "Starting…" placeholder, no cards |
| streaming | cards arriving; badge pulsing; auto-scroll at bottom |
| step-expanded | one card open (args + result) |
| rejected-visible | refusal chip under the step; axis awaits retry |
| user-scrolled-up | auto-scroll paused + "↓ New steps" pill |
| lagged | one-line "Some intermediate events were skipped (fast run)." |
| disconnected mid-run | banner "Connection lost — the run keeps running on the kernel. Reconnecting…"; cards frozen, not cleared |
| stopping (D-2) | Stop → "Stopping…" after inline confirm; awaiting cancel write |
| → terminal | transitions in place to R-4 (result: completed / failed / **canceled**) |

## Streaming / scroll (interaction-critical — see `04`)

- Auto-scroll to newest **only when pinned to bottom**. Scrolling up pauses it and
  shows "↓ New steps"; returning to bottom (or clicking the pill) resumes.
- Never move the viewport out from under a user who is reading.

## Copy

- Running header: "Kelter is working…"
- Rejected chip: "Kernel rejected this step: {reason} · retry {attempt}"
- Executed-failed: "Ran, but the tool reported an error."

## Accessibility

- The axis is a `role="log"` / `aria-live="polite"` region so new steps are
  announced without stealing focus. Each card is a heading-led group.
- Expand/collapse are real buttons with `aria-expanded`.
- The live pulse has a text equivalent ("working…") — never motion-only.

## API / data

- WS `/api/v1/events` (global; filter `run_id`; order `seq`). On reconnect,
  reconcile from `GET /api/v1/runs/{id}` (`steps[]`), since proposals aren't
  replayable (honest — Flow 07).

<!-- provenance: ai_authored | agent: claude-sonnet-5 | session_date: 2026-07-20 | human_validated: false | context: R-3 parallel fan-out variant, P1/P2 (ADR 0022) — extends the sequential live timeline above, does not replace it. -->

## Parallel variant — plan fan-out (P1, ADR 0022)

When the kernel accepts a plan whose nodes execute concurrently (ADR 0010 +
P1's readiness-driven executor), the axis's step-keyed narrative gains a
second shape alongside the sequential one above: N **swimlanes**, one per
plan node, rendered where the flat step list would otherwise be. This is an
**additive rendering mode of the same event stream** — `RunEvent`'s
`branch: string | null` (the plan node's own `id`) buckets a step into its
lane; `null` is today's ordinary step-mode event, unchanged. A run is either
step-mode (all `branch: null`, renders exactly as above) or plan-mode (all
steps branch-tagged, renders as this variant) — the two are never
interleaved in one run.

### Structure & hierarchy

```
┌─ detail: timeline (fan-out) ───────────────────────────────┐
│  Summarize three sources and merge   ◐ Kelter working…  [Stop]│
│  sonnet-5 · 00:14 · $0.011                                 │
│  ───────────────────────────────────────────────────────  │
│  ⑂ Fan-out — 3 branches          2 delegations so far      │  ← governance counter, no denominator
│  │ ⇄ delegate   delegate_to_researcher   ⧗ waiting…        │  ← opaque remote: spinner, no %
│  │ ⇄ delegate   delegate_to_analyst  validated ✓ · 1.8s remote│
│  │ ⚙ tool       read_url             validated ✓ · executed│
│  ☰ review        review_verdict        proposed…           │  ← join node: same list, later step index
│                                                              │
│                                   [ ↓ New steps ]           │
└──────────────────────────────────────────────────────────────┘
```

- A **fan-out region** (bordered card, left rail) replaces the flat step
  list for the run's plan nodes. Its header: "⑂ Fan-out — N branches" +
  the governance counter (below).
- Each **lane** is one plan node — one node index maps to exactly one axis
  step, never a sub-sequence, so "lane" and "step card" are the same object
  here (P1's executor assigns one index per node; concurrency changes
  *when* things run, not the step-keying model the whole axis is built on).
- Lanes render in the plan's topological (step-index) order, not completion
  order — Kahn's algorithm (`plan::validate`) already orders parents before
  children, so a join node (a node depending on multiple upstream
  branches) naturally appears after the branches it depends on. **No DAG
  diagram** — nothing else in this app renders a graph; a vertical rail +
  topological order reads as "diverge then converge" without one.
- **Lane kind** is inferred from the node's `action` (the event stream
  carries no explicit node-type field): `delegate_to_*` → A2A delegation;
  `review_verdict` (or the node-type fallback label a review node without
  an explicit action gets, `"review"`) → review; anything else → tool. A
  small kind badge (`⇄ delegate` / `☰ review` / `⚙ tool`) sits above each
  lane's card.
- Each lane **is** a `StepCard` (same component as the sequential view,
  unmodified rendering for proposed/retrying/executed/executed-failed/
  rejected) — mirrored, not reinvented. A governance-refused delegation is
  therefore a completely ordinary `Rejected`-shaped step: same danger chip,
  same copy shape ("Kernel rejected this step: {reason} · retry {attempt}"),
  only the reason string differs (e.g. "delegation budget exhausted…"). It
  is not a distinct visual vocabulary — the honesty convention (design/08)
  is explicit that a governed refusal must be indistinguishable in *kind*
  from any other kernel rejection.

### Opaque remote (indeterminate, never fake progress)

A still-running A2A lane (`delegate_to_*`, no receipt yet) shows a genuinely
**indeterminate** spinner + "waiting on remote agent…" — no percentage, no
fabricated step count, because the kernel has no notion of remote progress
to report (the brief's "ZERO fałszywego postępu" rule, restated in
design/08's honesty convention). **This did not exist before this variant**
— the sequential single-delegation case (a lone `delegate_to_*` step in an
ordinary run) rendered as a bare `StepCard` with plain "proposed…" text, no
special treatment; grounded and confirmed absent, not silently assumed.
`StepCard` gained an `opaqueRemote` prop for this — available to any future
single-delegation rendering too, not fan-out-only.

Once a lane's receipt lands, elapsed time shows precisely from the A2A
result's own `remote_ms` (server-measured). While still running, the same
elapsed reading has no server-known figure yet, so the lane shows a
client-measured wall-clock timer instead (identical technique to the run
header's own elapsed timer) — an honest "how long we've been waiting", not
a disguised progress bar.

### Governance counter

"**N delegations so far**" — a plain count, no denominator. The kernel's
`DelegationLimits.max_delegations_per_run` is not exposed over HTTP
anywhere today (grepped `crates/api/src/lib.rs`'s route table — absent);
showing "N/max" would mean either hardcoding a number that can silently
drift from the daemon's real config, or fabricating a max the UI cannot
verify. Both violate the honesty convention, so the max is omitted rather
than guessed. If a future daemon exposes delegation limits over HTTP, this
is the natural place to add the denominator back.

### Master list

Any run whose axis has observed a branch-tagged event gets a small "⑂"
indicator (glyph + `title` tooltip + `sr-only` "forked" text, mirroring the
existing status-glyph convention exactly — B-7: never color/glyph alone) on
its `R-1` row. **Honest limitation:** this is derived from the client's own
in-memory axis, which is only populated by events actually observed this
session (live WS broadcast while connected, or a prior visit's replay) — a
fan-out run that finished before the current session connected, and that
nobody has opened since, will not show the badge. No run-list field exists
server-side to answer "did this run fork" retroactively; documented as a
known gap here rather than worked around with a guess.

### Accessibility

- No new live region: the timeline's existing milestone-throttled
  `role="log"` announcer already scans every axis step regardless of
  `branch`, so a lane's rejection/execution is announced exactly like an
  ordinary step's — reused, not duplicated.
- Kind badges and the fan-out/governance-counter chips are glyph + text +
  color (B-7) — never color alone.
- **Known gap, disclosed rather than papered over:** A2A delegation targets
  carry no trust concept anywhere in this codebase today (grepped
  `crates/a2a/src/*.rs` and `crates/api/src/lib.rs` — the only `trust`
  field in the whole API surface is `SkillView.trust`; A2A's security
  control is SSRF-pinned origin allowlisting, not a trust label). The
  brief's "mirror the skills trust-badge convention" assumed A2A nodes have
  the same kind of trust data skills do — grounding shows they don't. A
  fabricated trust badge would violate the honesty convention worse than
  omitting one, so **no trust badge renders on A2A lanes**. If A2A ever
  gains a real per-agent trust concept, this is where its badge belongs.
