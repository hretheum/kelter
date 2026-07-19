<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage: open decisions for Eryk, each with a recommendation. Includes the stack-fit flag the brief's FAILURE clause requires. -->

# 07 — Open decisions (for Eryk)

Every decision the spec deliberately did **not** make unilaterally, with a
recommendation. Backend-shaped items (D-1, D-2) are the ones that need a product
call because they touch the API contract; the rest are build-time choices.

| # | Decision | Recommendation | Blocking? |
|---|---|---|---|
| **D-1** | **Historical runs are lossy** — `proposed`/`rejected`/`rationale` aren't stored on `Run`, so a run opened from history shows only `steps[]`. Add a journal-replay endpoint (`GET /runs/{id}/events`) so history shows the full axis? | **RESOLVED → IN SCOPE (Eryk, 2026-07-19).** Add `GET /api/v1/runs/{id}/events` (bearer-gated) returning the full ordered `RunEvent` list from the journal (ADR 0007 already holds it). History now shows the **full** axis; the "lossy historical" honesty note is removed (R-4, Flow 05, `00 §3.2`). **Net-new backend endpoint** — a build dependency, flagged in `06 §1a`. | Backend task; UI ready. |
| **D-2** | **No web-ui run cancel.** `Canceled` is reachable only via A2A `tasks/cancel`. Add `POST /runs/{id}/cancel` so the timeline can offer Stop? | **RESOLVED → IN SCOPE (Eryk, 2026-07-19).** Add `POST /api/v1/runs/{id}/cancel` (bearer-gated) mirroring the kernel's existing `cancel_run` + A2A abort-handle path. The live timeline gains a **Stop** control (R-3), with an inline confirm. **Net-new backend endpoint** — a build dependency, flagged in `06 §1a`. | Backend task; UI ready. |
| **D-3** | **Action-space hint** in the composer empty state (`GET /api/v1/tools`). Show "Kelter can: …"? | **Yes — cheap, teaches the first step.** Fail-silent if the call errors; never block the composer. | No. |
| **D-4** | **Model chip in the top bar** (read-only awareness + Settings doorway). | **Keep — approved at gate #1.** Read-only only; editing stays in Settings. | Resolved. |
| **D-5** | **`⌘K` to focus the master list** (jump-to-history). | **Include — low cost, aids keyboard nav.** Not load-bearing; drop if it clashes with a future command palette. | No. |
| **D-6** | **Framework:** React 19 (shadcn default) vs a Svelte/Vue port for a lighter runtime. | **React 19.** Matches shadcn out of the box, keeps the bundle within budget, least friction. Revisit only if bundle overshoots ~2.5MB. | No — build-time. |
| **D-7** | **Vite build ordering under RustEmbed** — commit `dist/`, a CI build step, or a `build.rs` that shells `vite build`. | **CI build step + documented local `pnpm build`;** keep `dist/` gitignored. A `build.rs` shell is convenient but couples `cargo build` to Node — avoid unless you want one-command builds. | No — build-time, but must be settled before shipping (stale `dist/` risk). |
| **D-8** | **Stack-fit flag (brief FAILURE clause): does shadcn fit the execution stream?** | **Yes, with a caveat, no workaround needed.** The *visuals* (step cards, collapse, badges, dialog) map cleanly to shadcn `Collapsible`/`Card`/`Badge`/`Dialog`. The *hard part* — bucketing a global WS broadcast by `run_id` + `seq` ordering + auto-scroll pause — is **app logic in `lib/events.ts`**, not a component concern, so shadcn neither helps nor hinders it. No shadcn primitive is being bent to fit. Flagged per the brief; **no silent workaround.** | No. |

## Notes

- **D-1 / D-2 are the only contract-touching items — both now IN SCOPE.** Both are
  *additive* (new bearer-gated endpoints), neither weakens auth nor touches the
  kernel-inversion invariants (`cancel_run` already exists; the journal already
  holds the events). They are **build dependencies**: the Stop control and full
  historical replay in the UI spec assume these two endpoints land — see `06 §1a`.
  Until they land, those two affordances must be **feature-flagged off** rather
  than shipped as dead controls (the config-501 rule still applies).
- **Boundary reminder (not a decision):** topology onboarding stays in the Tauri
  launcher; the web-ui redesign does not absorb it. If you ever want a single
  unified shell, that's a separate, larger piece (and would still respect ADR
  0004's "same web app in the browser" constraint — the browser can't start an
  in-process kernel).
