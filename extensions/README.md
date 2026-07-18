---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 2: extension model first slice (ADR 0006) — write-your-own guide based on the working wordstats example"
---

# Extending Kelter — no Rust required

Kelter is a runtime: you add capabilities by **registering extensions**, not
by recompiling the kernel. An extension is a directory with a manifest and
an MCP server in any language. The kernel discovers it at startup, adds its
tools to the action space, and — like everything else — validates every
proposed call against the tool's schema before executing it. Extensions are
untrusted by construction (ADR 0006); the gate is not optional.

The working example in [`wordstats/`](wordstats/) is a complete extension in
~25 lines of Python. This guide is based on it.

## Anatomy of an extension

```
extensions/
  wordstats/
    kelter-extension.toml   # manifest: what you provide, how to start you
    server.py               # an ordinary MCP server (official `mcp` SDK)
    run.sh                  # launcher (creates a private venv on first run)
```

**The manifest** declares capabilities and the start command:

```toml
[extension]
name = "wordstats"
description = "Word statistics tool in ~25 lines of Python"
provides = ["tools"]        # this slice supports "tools"; more axes later

[server]
command = "./run.sh"        # resolved relative to this directory
```

**The server** is a standard MCP stdio server. In Python with the official
SDK (`pip install "mcp[cli]"`, Python ≥ 3.10):

```python
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("wordstats")

@mcp.tool()
def word_stats(source: str, save_as: str = "stats.json") -> dict:
    """Compute word statistics for a text file …"""
    ...

if __name__ == "__main__":
    mcp.run()   # stdio transport by default
```

Type hints become the tool's JSON Schema — the same schema the kernel uses
to validate every call the model proposes.

## The runtime contract

- **Transport:** MCP over stdio. `stdout` belongs to the protocol — send
  any setup/log output to `stderr` (see `run.sh`).
- **Working directory:** your extension's own directory.
- **`KELTER_WORKSPACE`** (env): the absolute path of the run workspace.
  Resolve relative file arguments against it so your tool composes with the
  built-in ones; write outputs inside it.
- **Registration:** list the parent directory in the daemon config —
  `[tools] extension_dirs = ["./extensions"]` (the default). Every
  subdirectory with a `kelter-extension.toml` is loaded at startup. A broken
  manifest fails startup loudly; it is never skipped silently.
- **Only your tools:** set `[tools] bundled_tools = false` to serve
  exclusively your extensions.

## Try it

```sh
scripts/smoke-extension.sh      # registers wordstats, then has a local
                                # 0.5B model actually use it in a run
```

or manually:

```sh
cargo run -p daemon -- --config config.toml     # extension_dirs picks it up
curl -H "Authorization: Bearer <token>" localhost:7466/api/v1/tools
# → …, "word_stats", …
```

## What this is (and isn't) yet

This is the first slice of the governed extension model: manifest-declared
**tool** extensions over MCP/stdio. Memory backends, WASM in-process
extensions, and manifest-scoped permissions are staged next — see
[ADR 0006](../docs/adr/0006-governed-microkernel-extension-model.md) for the
full model and sequencing.
