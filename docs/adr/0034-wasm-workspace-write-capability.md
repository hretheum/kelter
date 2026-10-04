---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-04
  human_validated: false
  session_context: "K12 acceptance (2026-10-04), finding N1: no plain 'write this content to a file' tool, so the model reached for append_ledger. Adds the zapisz_plik WASM extension, which needed three additive extensions to the ADR 0011 WASM tool ABI."
---

# ADR 0034: A workspace-write capability for WASM tools (`zapisz_plik`)

Status: accepted · Date: 2026-10-04 · Extends: ADR 0011 (WASM tool ABI), ADR 0012 (sandbox-by-default) · Trigger: K12 acceptance finding N1

## Context

On K12 the default tool set had nothing that simply writes given text to a
file. The bundled `write_report` renders a report *from a structure file*; the
model, asked to save content, picked `append_ledger`. The fix is a plain
"write this file" tool, shipped as an extension (ADR 0006: batteries go through
the public extension API) and as WASM, so it is contained by construction
rather than an operator-trusted subprocess (ADR 0012).

ADR 0011's WASM tools could not express it: a module has no filesystem, takes a
single `input` string, and has no way to report failure — any JSON it returns
is a successful step.

## Decision

Three additive extensions to the WASM tool ABI. Modules that use none of them
behave exactly as before.

### 1. `workspace_write` as a host capability (`kelter:fs`)

`[permissions] workspace_write = true` — the same key subprocess extensions
already use — on a WASM **tool** makes the host define one import,
`kelter:fs.kelter_ws_write(path_ptr, path_len, data_ptr, data_len, flags) -> i32`,
backed by the daemon's run workspace (`[tools] workspace`). Without the grant
the import is undefined and the module cannot be instantiated — the same
structural gate as `kelter:net` (ADR 0011). Memory backends never get it.

**The host is the enforcement point; the guest is not trusted**
(`crates/wasmhost/src/workspace.rs`):

- path must be relative and made only of normal components — no `..`, `.`,
  root, drive prefix, backslash or NUL; ≤ 512 bytes;
- no existing component on the way may be a symlink (a link planted in the
  workspace cannot redirect the write outside it);
- an existing file is replaced only with the overwrite flag; otherwise creation
  is atomic no-clobber (`create_new`);
- an overwrite writes a temp file and `rename`s it into place, which replaces
  the directory entry and never follows a link at the target;
- hard cap of 1 MiB per write.

Residual, stated not hidden: the symlink checks are check-then-act. A hostile
local process racing the daemon *inside its own workspace* could swap a
directory for a link between check and write. The workspace is the daemon's
own directory; a co-resident attacker with write access to it is outside this
capability's threat model.

### 2. Typed, flat arguments (`[wasm] parameters = "schema.json"`)

A tool may declare a JSON Schema for its arguments instead of the single
`input` string. The registry holds it to ADR 0011's shallow-grammar rule at
load: an object whose properties are all `string` / `boolean` / `integer` /
`number`, `required` naming only declared properties; `additionalProperties`
is forced to `false`; anything else refuses to load, loudly. The guest then
receives the validated arguments object as JSON. Named, described arguments
(`sciezka`, `tresc`, `nadpisz`) are far easier for a weak local model than JSON
smuggled inside one string.

### 3. Guest-reported failure (`{"ok": false, "error": "..."}`)

A top-level `"ok": false` result is now a **failed** step, with `error` as the
message. Before, a refused write would have been journaled as a successful
step — and, after ADR 0033, would have counted as the run's effective action.
No pre-existing module returns `"ok": false`, so none changes behaviour.

### The extension

`deploy/extensions/zapisz-plik/` — manifest, `schema.json`, and the compiled
module (built from `examples/wasm-tools/zapisz-plik/`, rebuilt by
`scripts/build-wasm-demo.sh`). It lives under `deploy/` rather than the
auto-scanned `extensions/` because it is a deployment artifact: the whole
directory is meant to be mounted as an `extension_dirs` entry (see
`deploy/extensions/README.md`). The guest adds its own checks and a tighter
256 KiB limit for clear Polish error messages; those are convenience, the host
checks are the boundary.

## Consequences

- K12 gets a contained file writer with no `[trust]` entry needed (it is not a
  subprocess) and no change to the container unit.
- The WASM ABI grows by one optional import, one optional manifest key and one
  result convention — all additive. No API or database change.
- Not done: reading files from WASM (no `kelter:fs` read import), a binary
  write variant, per-extension sub-directories of the workspace. Each is a
  separate grant decision, not a free extension of this one.

## Evidence

`crates/wasmhost/src/workspace.rs` (host checks, incl. symlink escape),
`crates/wasmhost/tests/zapisz_plik_test.rs` (the real module through the real
host, without the grant, and inside a kernel run), `crates/mcp/src/extensions.rs`
(schema flatness + the shipped manifest registers), guest unit tests in
`examples/wasm-tools/zapisz-plik`. A real daemon run against
`deploy/extensions` is recorded in `docs/BUILD-EVIDENCE.md`, 2026-10-04.
