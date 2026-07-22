---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-22
  human_validated: true
  validation_note: "signed 2026-07-22 on Eryk's authorization; NOT independently re-run at sign-off"
  session_context: "Run-output UX pass: render the terminal result as prose instead of raw JSON, and surface the files a run produced in a side artifact panel (markdown-rendered, copy + download) backed by a new bearer-auth'd, path-traversal-safe files endpoint scoped by the run's own journal."
---

# ADR 0029: Prose run output + journal-scoped artifact files

Status: accepted · Date: 2026-07-22 · Builds on: ADR 0007 (journal — the
scope source), ADR 0018 (security surface) · Related: design/03 (result
rendering), the tool-side `Workspace` escape-refusal in `crates/mcp`

## Context

The run result panel rendered the terminal output as pretty-printed JSON —
`{"summary": "..."}` shown as a code block. And when a run's whole point was
a produced file (`write_report` → `report.md`), the UI never showed the file:
the user read a sentence *about* a report they could not open.

Two constraints shaped the fix:

1. **The tools workspace is shared.** One `[tools].workspace` directory
   serves every run of a daemon. "This run's files" therefore cannot be a
   directory listing — that would leak every other run's output to any
   token-holder, and there is no per-run subdirectory to point at.
2. **Serving files over HTTP is an LFI surface.** A `files/{name}` endpoint
   that resolves an arbitrary client-supplied path is a local-file-inclusion
   vulnerability by default. The brief's hard line: if a safe scope cannot be
   guaranteed, don't ship the endpoint.

## Decision

**Scope a run's files by its own journal, not by the filesystem.** A file is
served only if a successful `Receipt` in that run's journal names it under a
file-name key the bundled tools emit (`saved_as`, `output`, `ledger`). The
journal is durable, append-only, and written by the kernel — the client
cannot influence it after the fact. Two new bearer-gated routes:

- `GET /api/v1/runs/{id}/files` — journal-derived names + sizes (existing
  files only).
- `GET /api/v1/runs/{id}/files/{*name}` — one file's UTF-8 content as JSON.
  The name must be in the allowlist; anything else is a 404
  indistinguishable from a file that never existed (no workspace probing).

Path safety reuses the tool-side `Workspace::resolve` pattern on the way out
(reject `..` components and absolute paths outright → 400), then adds a
canonicalize-and-containment check so even a symlink planted inside the
workspace cannot serve content from outside it. A 2 MiB serving cap (413)
and a UTF-8-only rule (415) bound the surface; binary blobs are listed but
not served. The service is an optional router capability
(`api::RunFilesService`, wired via `router_full`/`build_app_full`): absent →
the routes 404, the same degrade convention as the skills/agents
controllers, which the frontend already understands.

**Render the result as prose, not a dump.** `outputView` (web-ui) extracts
the kernel's `finish` contract (`{ summary }`, and plan mode's
`{ summary, nodes }`) — or a `message`/`text`/`result` string for foreign
shapes — and shows it as text; any remaining payload moves behind a
collapsed "Raw output" disclosure. A shape with no prose degrades to
"Completed." + the disclosure; it never throws and never leads with raw
JSON.

**Surface the files as an artifact panel.** A side panel on the completed
face — open by default, first file preselected, markdown rendered, plain
`<pre>` for other text, Copy + Download (client-side Blob, so the bearer
fetch path stays the only network path), quiet empty state when the run
produced nothing, hidden entirely on a daemon without the endpoint.

## Markdown dependency

The web UI had no markdown renderer; `react-markdown` was added (the one new
dependency). Chosen because it renders to React elements — no
`innerHTML`/`dangerouslySetInnerHTML`, and raw HTML inside a produced file
is *not* executed, so a run's output cannot inject into the page — and it is
pure ESM, working under Vite and Vitest without config.

## Consequences

- A produced file is only visible while it still exists in the workspace —
  the listing checks the disk, so a pruned/cleaned workspace shrinks the
  panel rather than erroring.
- The allowlist keys are the bundled tools' receipt vocabulary. A future
  tool that writes files under a different receipt key won't surface its
  files until its key is added — a one-line, deliberate extension point
  (`FILE_NAME_KEYS`), preferred over guessing which receipt strings are
  paths.
- Extensions with `workspace_write` can write files no receipt names; those
  stay invisible to the panel. Same trade as above: invisible beats leaky.
- Proven by `crates/api/tests/run_files_test.rs`: anonymous → 401,
  percent-encoded `../` and absolute paths → 400, another run's co-resident
  workspace file → 404, produced file → 200 with exact content.
