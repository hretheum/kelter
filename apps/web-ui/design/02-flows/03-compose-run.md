<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: composing and submitting a run. The composer is the input surface, unmistakably distinct from output. -->

# Flow 03 — Compose & submit a run

**Job:** J1. **Destination:** Runs · composer (R-2). **API:** `POST /api/v1/runs
{task}` → 202 `{run_id}`; 422 on empty.

## The point of this flow

This is where we kill "is this a console?". The composer is a **distinct, labeled
input** with its own affordances; it is never confused with the timeline because
(a) it is the *only* surface with an editable field and a primary "Run" button,
(b) it carries an explicit label ("Task for Kelter"), and (c) on submit it
transforms *in place* into the timeline, so the user watches their input become a
running run in the same spot — cause and effect, not two mystery boxes.

## Entry

- `+ New run` in the master list (`Cmd/Ctrl+N`), or the app's first-run empty
  state (Flow 01). Composer takes the detail pane; the field is auto-focused.

## Steps

1. **Focus** lands in the task textarea (auto).
2. **Type** the task. The empty state (before typing) teaches: a one-line "what is
   this" + 2–3 example tasks + (enhancement, D-3) the live action space from
   `GET /api/v1/tools` as "Kelter can: read_url, write_file, …".
3. **Submit**:
   - `Enter` → submit.
   - `Shift+Enter` → newline.
   - `Cmd/Ctrl+Enter` → submit (parity for muscle memory).
4. **Validation** — empty/whitespace task → do not POST; inline "Type a task
   first" and keep focus (mirrors the API's 422 without a round-trip).
5. **Submitting** — button → spinner; field disabled. On 202 `{run_id}`: the
   composer **becomes the live timeline** (Flow 04) for that `run_id`, a new row
   appears at the top of the master list, and the field is cleared for next time.
6. **Error** — non-202 (e.g. 500) → inline error, field content preserved so the
   user doesn't lose their text.

## States (R-2)

| State | Rendering |
|---|---|
| empty | teaching copy + examples + action-space hint; Run disabled until non-empty |
| typing | Run enabled; char affordance minimal |
| submitting | field disabled, Run → spinner |
| validation-error | inline "Type a task first", focus retained |
| disabled (disconnected) | field + Run disabled, hint "Reconnect to run a task" |

## Copy

- Label: **"Task for Kelter"** (says this is input, and to whom).
- Placeholder: "e.g. Read https://example.com, extract its structure, and write a
  markdown report to out/report.md"
- Button: **"Run"** (`Enter`), with the shortcut hinted on hover/focus.
- Empty-state title: "Give Kelter a task." Subtitle: "It proposes each step, the
  kernel checks it, and you watch it run."

## Exit

Detail pane is now the live timeline for the new run (Flow 04). The composer is
one `Cmd+N` away.
