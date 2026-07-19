<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec R-1 master list (run source-list). -->

# R-1 — Master list (run source-list)

The persistent left column of the Runs workspace. Owns J4 (history) and the entry
to J1 (New run).

## Structure & hierarchy

```
┌─ master (source-list) ──────────┐
│  ⌘N  [ + New run ]              │  ← pinned action, primary affordance
│  ─────────────────────────────  │
│  ◐ Summarize the RFC…      2m   │  ← running (pulse), selected
│  ✓ Extract table from…    14m   │
│  ✕ Write report to out/…   1h   │
│  ✓ Fetch and classify…    1h   │
│   … newest → oldest …           │
└─────────────────────────────────┘
```

- **New run** button pinned top; always visible, never scrolls away.
- **Rows**, newest-first: `[status glyph] [task (truncated, 1 line)] [relative time]`
  with a second muted line: `[model badge] · [cost | —]`.
- Selected row is marked (accent left-border + subtle fill). At most one selected.

## Content (copy)

- Button: **"+ New run"** (shortcut chip `⌘N` on the right).
- Row title: `run.task`, truncated with ellipsis, full text in `title`/tooltip.
- Time: relative (`2m`, `1h`, `Jul 18`), absolute in tooltip.
- Model badge: short form of `policy` (e.g. `sonnet-5`, `qwen-0.5b`).

## Status glyphs (color from tokens, never color-only — glyph carries meaning)

| status | glyph | token |
|---|---|---|
| pending | ◔ | muted |
| running | ◐ (pulse) | accent |
| completed | ✓ | success |
| failed | ✕ | danger |
| canceled | ⊘ | muted |

## Variants / states

| State | Rendering |
|---|---|
| empty | centered teaching block: "No runs yet — give Kelter its first task." + button (focuses composer). No empty rows. |
| loading | 4 skeleton rows (shimmer), New run active |
| populated | the list |
| disconnected/stale | list dimmed to ~60%, small header note "Reconnecting…"; rows preserved (never cleared) |

## Responsive

- Desktop: fixed ~280px column.
- Narrow (<720px): the list is the full-width first "page"; selecting a run pushes
  the detail as a second page with a back affordance. New run overlays as the
  composer page.

## Keyboard & focus

- List is a focusable listbox; `↑/↓` change selection, `Enter` opens into detail,
  `Home/End` jump. `⌘N` (global) creates a run regardless of focus.
- `role="listbox"`, rows `role="option"` with `aria-selected`; running row has an
  `aria-live="polite"` status update on completion.

## Data & API

- `GET /api/v1/runs` on connect + on `created/completed/failed` WS events (bucketed
  refresh, as today's `refreshRuns`). No pagination in v1 (scope note, Flow 06).
