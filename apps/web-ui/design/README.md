<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — index + validation trail for the design package. Text spec is the source of truth. -->

# Kelter web-ui — full redesign (IA · flows · screens · interactions · system)

Complete, implementable redesign of the Kelter web UI: information architecture,
flows, screen specs, interaction contract, design system, and an implementation
hand-off for a shadcn/ui + Tailwind v4 + Vite (RustEmbed) build. **The text spec
is the source of truth.**

Grounded in the real code (`crates/kernel-core`, `crates/api`,
`crates/model-config`, `apps/desktop/src-tauri`, ADR 0004 / 0015) — not
assumptions. The current vanilla `index.html`/`app.js`/`style.css` is what this
replaces; the migration map is in `06 §6`.

## Read in order

| File | Stage | What it settles |
|---|---|---|
| `00-jobs-and-objects.md` | 1 | jobs-to-be-done + domain objects & states (Run first-class; live/historical asymmetry; 200/501 config; connection states) |
| `01-information-architecture.md` | 2 | **2 destinations** (Runs, Settings) + connection chrome; navigation; screen map; IA self-critique |
| `02-flows/` (8) | 3 | every flow: steps, states, empty/loading/error |
| `03-screens/` (6) | 3 | R-1..R-4, S-1, G-1/G-2 — structure, copy, variants, responsive, a11y |
| `04-interaction-spec.md` | 4 | keyboard (Enter/⇧Enter/⌘Enter), focus, streaming/scroll, copy, announcements |
| `05-design-system.md` | 5 | HIG-flavored dark-first tokens as Tailwind v4 `@theme` + shadcn variants (all states) |
| `06-implementation-handoff.md` | 7 | stack mapping, per-screen composition, Vite→RustEmbed, migration map |
| `07-open-decisions.md` | — | decisions for Eryk + recommendations (incl. stack-fit flag) |
| `08-critique-and-a11y.md` | 8 | self design-critique + WCAG 2.2 AA audit (computed contrast) + fixes applied |

## Human gates (§10)

- **Gate #1 — after IA (Stage 2): APPROVED** by Eryk (2 destinations; model chip
  kept). Everything after was built on the approved skeleton.
- **Gate #2 — full spec acceptance: APPROVED** (Eryk, 2026-07-19), with **D-1
  (journal replay) + D-2 (cancel) folded into scope** — both are net-new
  bearer-gated endpoints (`06 §1a`), feature-flagged so the UI degrades where
  absent. The package is ready to become an implementation prompt for Fable.

## Validation trail (§9)

- Object model, IA justification, flow-by-flow, screen specs, design-critique, and
  a11y audit are all present (above).
- **§7.1 "5-second" user test (is-this-a-console): NOT YET RUN.** It requires ≥3
  real users and cannot be closed on paper. It is the one empirical gate; carried
  as a build-time evidence task (`08 Part D`). The design is *structured* to pass
  it (composer is the only input surface, labeled, and transforms into the
  timeline in place) — but "structured to pass" ≠ "passed".
- Automated axe/Lighthouse + a VoiceOver walkthrough of the streaming timeline are
  build-time gates (`08 Part D`).

## Optional (not built)

A clickable prototype is optional (§1) and the text spec is authoritative. A
lightweight self-contained HTML mock of the master–detail + composer→timeline→
result transition can be produced on request to de-risk the §7.1 test before the
full shadcn build — say the word.

## Naming

Product is **Kelter** throughout; the forbidden pre-rename codename appears
nowhere in this package.
