<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 4 full interaction spec: keyboard, focus, shortcuts, streaming, scroll, copy. Implementer writes zero new interaction decisions. -->

# 04 — Interaction specification

The contract for every interactive behavior. Paired with `03-screens`, this is
detailed enough that the implementer makes **no** new interaction decision.

---

## 1. Keyboard map (global)

| Shortcut (mac / win-linux) | Context | Action |
|---|---|---|
| `Enter` | composer textarea | **Submit run** |
| `Shift+Enter` | composer textarea | Insert newline |
| `⌘Enter` / `Ctrl+Enter` | composer textarea | Submit run (parity) |
| `⌘N` / `Ctrl+N` | anywhere | New run → focus composer |
| `⌘,` / `Ctrl+,` | anywhere | Open Settings |
| `⌘S` / `Ctrl+S` | Settings (editable) | Save model config |
| `Esc` | Settings / any sheet | Close → back to Runs, restore focus |
| `↑` / `↓` | master list focused | Move selection |
| `Enter` | master list focused | Open selected run in detail |
| `Home` / `End` | master list focused | First / last run |
| `⌘K` / `Ctrl+K` | anywhere | Focus master list (optional — D-5) |
| `Tab` / `Shift+Tab` | anywhere | Move through the focus order (§3) |

**Stop a run (D-2):** the timeline's **Stop** is a focusable `<button>`; `Enter`/
`Space` triggers the inline confirm; a second `Enter` on "Confirm" cancels the run
(`POST /runs/{id}/cancel`). No global shortcut (deliberate — cancel should be
intentional, not a hotkey slip). Hidden when the endpoint is flagged off.

**Rule:** the only place `Enter` submits is the composer. Everywhere else `Enter`
activates the focused control (button/row) — standard, predictable.

## 2. The Enter/Shift+Enter/⌘Enter contract (brief §6, hard requirement)

In the composer textarea `keydown`:

```
if key == "Enter" and not shiftKey:
    if (metaKey or ctrlKey) or (plain):   # plain Enter AND ⌘/Ctrl+Enter both submit
        preventDefault(); submit()
if key == "Enter" and shiftKey:
    # default: inserts newline (do NOT preventDefault)
```

- Plain `Enter` = submit (fast path). `Shift+Enter` = newline (multi-line tasks).
- `⌘/Ctrl+Enter` = submit too (users trained by chat apps expect it).
- IME composition guard: ignore `Enter` while `isComposing` (CJK input) — never
  submit mid-composition.

## 3. Focus order & focus management

### Focus order (Tab), per view

- **Runs (composer):** top-bar model chip → connection pill → ⚙ → New run →
  composer textarea → Run button → (action-space example chips).
- **Runs (timeline/result):** top bar (chip, pill, ⚙) → New run → step
  expand/collapse controls in DOM order → Copy button (result).
- **Settings:** Done/back → Provider radios → Preset → Base URL → Model → API key
  → Test → Save.
- **Connection sheet (trap):** Server → Token → Connect → close (⎋).

### Focus on view change (no lost focus, ever)

| Transition | Focus lands on |
|---|---|
| App opens (model OK) | composer textarea |
| `⌘N` / New run | composer textarea (selected) |
| Submit → timeline | timeline header (announced), not stolen mid-type |
| Select run from list | detail header (SR announces the run) |
| Open Settings | Done button, then first field is tabbable |
| Close Settings (`Esc`) | the ⚙ that opened it |
| Open connection sheet | first field; trap active |
| Close sheet | the connection pill |

### Focus ring

- Always visible, never removed. `:focus-visible` ring = 2px accent outline + 2px
  offset, meeting 3:1 contrast against both the control and its background (WCAG
  2.4.11 / 1.4.11). Applies to buttons, inputs, list rows, chips, pills.

## 4. Streaming & scroll behavior (brief §6)

### Auto-scroll (timeline R-3)

State machine:

```
pinnedToBottom = true
on new step:
    if pinnedToBottom: scrollToBottom()
    else: show "↓ New steps" pill (count optional)
on user scroll:
    pinnedToBottom = (distanceFromBottom < THRESHOLD ≈ 48px)
    if pinnedToBottom: hide pill
on click "↓ New steps" pill:
    scrollToBottom(); pinnedToBottom = true; hide pill
```

- Never scroll the viewport while the user is reading above the fold. The pill is
  the only nudge; it does not move content.
- Reduced motion: `scrollToBottom` is instant (no smooth) when
  `prefers-reduced-motion: reduce`.

### Streaming rendering

- Bucket WS envelopes by `run_id`; render only the bound run. Order by `seq`
  (monotonic per run). Out-of-order `seq` → insert by position, don't append
  blindly.
- `Lagged` (server dropped events for a slow consumer): show the one-line skipped
  note; reconcile terminal state from `GET /runs/{id}` if the run finished during
  the gap.
- Step cards default **collapsed** (args/results hidden). Expansion is per-card,
  remembered while the run is open.

### "Run in progress" vs "finished"

- Running: pulsing badge + live elapsed timer + (if priced) live cost. Timeline is
  `aria-busy="true"`.
- Finished: badge flips to terminal, timer freezes, `aria-busy="false"`, an
  `aria-live` announcement fires once (§6).

## 5. Copy affordances (brief §6)

- **Copy output / Copy error** (R-4): copies the raw value (object → 2-space JSON;
  string → as-is) via `navigator.clipboard.writeText`. Success → button label
  swaps to "Copied ✓" for 1.5s then reverts; failure (no clipboard permission) →
  "Press ⌘C to copy" with the block text-selected as fallback.
- **Copy step result** (optional, R-3/R-4 expanded card): same pattern, per card.
- Keyboard: Copy buttons are real `<button>`s in the focus order; `Enter`/`Space`
  activate.

## 6. Announcements (screen-reader live regions)

| Event | Region | Politeness | Text |
|---|---|---|---|
| connection change | pill | polite | "Connection: {state}" |
| step milestone | timeline (`role=log`) | polite | "{action} rejected" / "{action} executed" |
| run completed | detail | polite | "Run completed" |
| run failed | detail | assertive | "Run failed: {short reason}" |
| test result | Settings result line | polite | the result text |
| copied | visually-hidden live | polite | "Copied" |

- Live regions never steal focus. Assertive only for failure (a result the user
  must notice); everything else polite.
- **Throttle the axis (a11y fix, `08`).** Do **not** announce every `proposed`
  and `executed` — a fast run would flood a screen reader. Announce only
  **milestones**: `rejected` (governance moment) and the final `executed` per
  step, plus the terminal event. The visual `role=log` still renders every event;
  the *announced* set is the throttled subset.

## 6a. Skip link (a11y fix, `08`)

A visually-hidden **"Skip to runs"** link is the first focusable element; on focus
it appears (top-left) and jumps focus to the main workspace (`#main`,
`tabindex=-1`), bypassing the top bar for keyboard/screen-reader users (WCAG
2.4.1). Present in both topologies.

## 7. Empty / loading / error interaction defaults

- **Loading**: skeletons for lists, spinner-in-place for single blocks; never a
  blank pane; controls that need the socket are disabled, not hidden.
- **Empty**: teaching copy that names the *first action* and offers the control to
  do it (composer focus / New run). No dead ends.
- **Error**: inline, near the cause, with a remedy; preserve user input; failures
  that are *results* (a failed run) render as content, not dismissible toasts.

## 8. Motion

- Durations ≤150ms for micro-transitions (card in, pane swap, pill). Easing:
  `cubic-bezier(0.2, 0, 0, 1)` (macOS-ish ease-out).
- `prefers-reduced-motion: reduce` → disable slide/fade/pulse; keep instant state
  changes and the text equivalents. No motion is ever the *only* signal.

## 9. Pointer / hit targets

- Minimum hit target 28×28px (macOS-dense) with ≥8px spacing; list rows are full-
  width click targets; chips/pills have adequate padding. Touch (narrow/remote on
  a tablet): bump to 44×44px via the responsive breakpoint.
