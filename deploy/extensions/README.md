---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-04
  human_validated: false
  session_context: "K12 acceptance finding N1: a deployable extension directory with the zapisz_plik WASM tool, and how to mount it into the container without touching kelter.container."
---

# Deployable extensions

This directory is an **extension root**: point `[tools] extension_dirs` at it
(or at a copy of it) and every subdirectory with a `kelter-extension.toml` is
registered at daemon start. Unlike the repo's `extensions/` (examples, incl. a
subprocess tool that needs operator trust), everything here is meant to be
copied onto a server as-is.

| extension | what | mechanism | grants |
|---|---|---|---|
| [`zapisz-plik/`](zapisz-plik/) | `zapisz_plik(sciezka, tresc, nadpisz?)` — write a text file into the run workspace | WASM, in-process, sandboxed | `workspace_write` only |

## `zapisz_plik`

Writes `tresc` to `sciezka` (relative to `[tools] workspace`). The host — not
the module — enforces: relative path only, no `..` / `.` / absolute / drive /
backslash, no symlink anywhere on the way, **no overwrite unless
`nadpisz = true`**, 1 MiB hard cap (the module itself stops at 256 KiB). A
refused write is a *failed* step, so it never counts as the run's effective
action (ADR 0033). Design: [ADR 0034](../../docs/adr/0034-wasm-workspace-write-capability.md).

Files: `kelter-extension.toml` (manifest), `schema.json` (the typed arguments
the model sees), `zapisz_plik.wasm` (built from
`examples/wasm-tools/zapisz-plik/`; rebuild with `scripts/build-wasm-demo.sh`).

No `[trust]` entry is needed: it is not a subprocess, so the operator gate of
ADR 0012 does not apply. It has no network and cannot read files.

## Mounting it on K12 (rootless Podman + Quadlet)

`deploy/kelter.container` already mounts `~/.config/kelter` read-only at
`/data/config`. Put the extensions **inside that directory** and no change to
the unit is needed.

On the K12 host, as the user that runs the `kelter` unit:

```sh
# 1. copy the extension directory (manifest + schema + .wasm — all three)
mkdir -p ~/.config/kelter/extensions
cp -r deploy/extensions/zapisz-plik ~/.config/kelter/extensions/

# 2. register the directory in ~/.config/kelter/config.toml
#    [tools]
#    workspace = "/data/work"
#    extension_dirs = ["/data/config/extensions"]

# 3. restart and check
systemctl --user restart kelter
journalctl --user -u kelter | grep -E "zapisz|wasm tools online"
#   → registry: WASM extension 'zapisz-plik' (… grants: Grants { network: false, workspace_write: true })
#   → wasm tools online: zapisz_plik
curl -s -H "Authorization: Bearer $KELTER_TOKEN" http://127.0.0.1:7466/api/v1/tools
#   → […, "zapisz_plik", …]
```

Notes:

- `extension_dirs` **replaces** the default (`["./extensions"]`, which inside
  the image resolves to `/data/extensions` and does not exist — a missing
  directory is not an error). List both if you add another root later.
- The paths are **container** paths (`/data/config/…`), not host paths.
- Files written by the tool land in the `kelter-work` volume
  (`/data/work`), the same workspace the bundled tools use.
- The image does not need rebuilding: the module is loaded from the mounted
  directory at start. It does need a daemon built from a commit that includes
  ADR 0034 (the `kelter:fs` host import and typed `[wasm] parameters`) — an
  older daemon refuses this manifest loudly (`unknown field "parameters"`)
  rather than half-loading it.
- A broken manifest fails daemon start loudly; it is never skipped silently.
