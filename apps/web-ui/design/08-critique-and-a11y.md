<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 8 self-critique (design) + accessibility audit (WCAG 2.2 AA) run on the redesign itself, with contrast ratios computed from the 05 tokens and fixes applied before hand-off. -->

# 08 — Design critique & accessibility audit

Stage 8, run on **this** design before hand-off (brief §4.8 / §9: "fix before, not
after"). The named `design:design-critique` / `design:accessibility-review` skills
aren't installed here; this audit uses the installed `accessibility-compliance` +
`tailwind-design-system` skills and WCAG 2.2 AA as the bar. Contrast ratios are
**computed** from the `05` OKLCH tokens (via their ~hex), not asserted.

---

## Part A — Design critique (self)

| # | Critique | Verdict / fix |
|---|---|---|
| A-1 | Does the redesign actually kill "is this a console?" | **Yes, structurally.** The composer is the only surface with an input + primary button, is labeled "Task for Kelter", and *becomes* the timeline in place. The timeline has no input/button and reads as narration. The 5-second test (§7.1) targets exactly this — must still be run with 3 users (evidence pending, `09`). |
| A-2 | Is two destinations too spartan — does Runs overload? | **No.** Status selects one detail face at a time; the master list carries history. Confirmed at gate #1. |
| A-3 | Is the step axis skimmable when a run has 12 steps? | **Yes** — args/results collapse by default; only action + state show until expanded. Risk: a very chatty run. Mitigation: collapse-by-default + the "↓ New steps" pill so reading isn't interrupted. |
| A-4 | Does the visual language stay HIG, not generic shadcn? | **Yes** — 13px body, 28/32/36px controls, OKLCH blue-undertone dark, vibrancy/hairline elevation, tabular numerics. Documented as deliberate in `05 §5`. |
| A-5 | Empty states teach the first step? | **Yes** — list, composer, and result each have a titled empty state naming the next action. |
| A-6 | Any dead affordances (the config-501 mistake repeated)? | **None.** Stop (D-2) and full historical replay (D-1) are now in scope but **feature-flagged off** where the backend endpoint is absent — hidden, never shown-but-dead. Read-only 501 form; action-space hint fails silent. The design still refuses to *show* what the backend can't do; the difference is the endpoints are now planned (`06 §1a`), not deferred. |
| A-7 | Motion tasteful / not distracting? | ≤150ms, ease-out, one live pulse; all gated by reduced-motion. No bounce. |
| A-8 | Does anything fight the stack (FAILURE clause)? | **No** — `07` D-8: shadcn primitives fit; the hard WS-bucketing is app logic, not a bent component. Flagged, not worked around. |

**Design verdict:** no structural or visual blocker. The one item that can only be
closed empirically is A-1's 3-user test — left as an explicit evidence task in
`09`, not claimed as done.

---

## Part B — Accessibility audit (WCAG 2.2 AA)

### B-1 Contrast (1.4.3 text 4.5:1 · 1.4.11 non-text 3:1) — computed

Dark theme (the shipping theme), ratios from `05` tokens:

| Pair | Ratio | Req | Result |
|---|---|---|---|
| foreground `#e6e9ec` on background `#16191d` | **14.5:1** | 4.5 | ✅ AAA |
| foreground on card `#1d2127` | **13.3:1** | 4.5 | ✅ AAA |
| muted-foreground `#97a0ab` on background | **6.66:1** | 4.5 | ✅ AA |
| muted-foreground on card | **6.10:1** | 4.5 | ✅ AA |
| primary `#4ea1ff` (link/text) on card | **6.06:1** | 4.5 | ✅ AA |
| primary-foreground `#06121f` on primary (button) | **7.06:1** | 4.5 | ✅ AAA |
| success `#3fb950` on card | **6.37:1** | 4.5 | ✅ AA |
| danger `#f85149` on card | **4.82:1** | 4.5 | ✅ AA (tight) |
| warning `#e3a008` on card | **7.16:1** | 4.5 | ✅ AA |
| focus ring `#6cb6ff` on background | **8.2:1** | 3.0 | ✅ |

- **Finding B-1a (fixed by design):** the focus ring on the **primary button**,
  measured ring-vs-button-fill, is only **1.24:1** — fails 1.4.11 if the ring
  hugs the button. **Fix:** the ring is mandated with a **2px offset** rendered in
  the background color (`ring-offset-background`, `04 §3` / `05 §3.2`), so the
  visible adjacency is ring-vs-canvas = **8.2:1**. Confirmed present in the spec;
  the offset is non-optional.
- **Finding B-1b (noted):** danger-as-text is 4.82:1 — passes but tight. Danger is
  used mainly for glyphs/borders/short labels (UI 3:1 with wide margin); long
  error *bodies* render in `foreground` weight, with danger reserved for the
  label/border. No change needed; documented so implementers don't set long error
  prose in pure danger on card.
- Light theme ratios to be re-verified at implementation (secondary theme); the
  `05` light tokens target the same bar.

### B-2 Keyboard (2.1.1 · 2.4.3 · 2.4.7 · 2.1.2 no trap)

- Full keymap in `04 §1`; focus order + on-view-change handoff in `04 §3`; visible
  `:focus-visible` ring everywhere, never removed.
- Composer Enter/Shift+Enter/⌘Enter contract with IME guard (`04 §2`).
- Connection sheet is the only focus trap — and it has `Esc` + a close button (no
  trap without escape, 2.1.2). ✅

### B-3 Structure & semantics (1.3.1 · 4.1.2)

- `role=listbox`/`option` (master list), `role=log`+`aria-live` (timeline),
  `role=dialog`+`aria-modal` (sheet), `fieldset/legend` (Provider), real
  `<label for>` on every field, `aria-expanded` on collapsibles,
  `aria-invalid`/`aria-describedby` on errors. ✅
- **Fix applied B-3a:** a **skip link** ("Skip to runs") is now the first
  focusable element (`04 §6a`, `G-1`) — WCAG 2.4.1. Was missing; added.

### B-4 Live regions / announcements (4.1.3)

- **Fix applied B-4a:** the timeline `role=log` would flood a screen reader on a
  fast run. Announcements are now **throttled to milestones** (`rejected`, final
  `executed` per step, terminal), while the visual log still shows everything
  (`04 §6`). Failures announce `assertive`, everything else `polite`; no focus
  theft.
- Copy success, connection change, and test result all announce via polite live
  regions.

### B-5 Motion & preferences (2.3.3 · 1.4.12)

- `prefers-reduced-motion: reduce` disables slide/fade/pulse and makes
  auto-scroll instant (`04 §8`, `05 §2`). The live "working" state has a **text**
  equivalent ("Kelter is working…"), never motion-only. Text remains legible with
  spacing overrides (1.4.12) at 13px base. ✅

### B-6 Target size (2.5.8 AA, 24×24)

- Buttons 28/32/36px, icon 32px, copy 28px, rows full-width, pill/chips padded.
  All ≥24px with ≥8px spacing. Touch breakpoint bumps to 44px (`04 §9`). ✅

### B-7 Non-color signalling (1.4.1)

- Every status/event uses **glyph + text + color**, never color alone: R-1 glyphs
  (◔◐✓✕⊘), connection pill (◉○◐⚠ + label), rejected chip carries the word
  "rejected". ✅

---

## Part C — Fixes applied in this pass

| ID | Fix | Where |
|---|---|---|
| B-3a | Added skip link ("Skip to runs") as first focusable | `04 §6a`, `03 G-1` |
| B-4a | Throttled timeline announcements to milestones (anti-flood) | `04 §6` |
| B-1a | Confirmed mandatory focus-ring **offset** (else 1.24:1 on primary) | already in `04/05`; verified |
| B-1b | Documented: long error prose in `foreground`, danger for label/border only | `08 B-1` (note for implementers) |

## Part D — Residual / for implementation-time verification

- The **§7.1 3-user "5-second" test** — empirical, left as an evidence task (`09`);
  cannot be closed on paper.
- **Light theme** contrast re-verification (secondary theme).
- axe/Lighthouse pass on the built app (automated gate in CI).
- VoiceOver walkthrough of the timeline streaming (the one genuinely novel a11y
  surface).

No open **AA blocker** remains in the spec; the residuals are empirical checks
that belong to the build, not the design.
