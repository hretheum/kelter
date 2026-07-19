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
an MCP server in any language. The manifest declares WHICH capability it
provides, and the registry routes it to the matching kernel seam
(ADR 0008):

| `provides` | plugs into | governance gate |
|---|---|---|
| `tools` | the model's action space | kernel-side action validation + run journal |
| `memory` | the kernel's `MemoryProvider` seam (never the action space) | response schema validation, kernel-tagged provenance |
| `agent` | a **remote** A2A agent the kernel dials as `delegate_to_<name>` | `network` permission required, egress origin-pinned (SSRF), response validated, durable task id |
| `model-provider` | reserved — **refuses to load** until implemented | — |

Extensions are untrusted by construction (ADR 0006); the gates are not
optional, and a manifest declaring anything the runtime cannot scope
(unknown capability, unknown permission) fails startup loudly.

Two working examples: a tool in ~25 lines of Python
([`wordstats/`](wordstats/), below) and a memory backend in ~30 lines
([`../examples/extensions/memstore/`](../examples/extensions/memstore/) —
see "Memory extensions").

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
provides = ["tools"]        # exactly one capability per manifest
trust = "subprocess-unsandboxed"   # required for a subprocess tool (see below)

[permissions]
workspace_write = true      # required to receive KELTER_WORKSPACE

[server]
command = "./run.sh"        # resolved relative to this directory
```

**The server** is a standard MCP stdio server. In Python with the official
SDK (`pip install "mcp>=1.28,<2"`, Python ≥ 3.10):

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
- **Permissions:** declare what you need in `[permissions]`
  (`workspace_write` gates the `KELTER_WORKSPACE` handover; `network` is
  recorded). Unknown permission keys are a manifest error.
- **Registration:** list the parent directory in the daemon config —
  `[tools] extension_dirs = ["./extensions"]` (the default). Every
  subdirectory with a `kelter-extension.toml` is loaded at startup. A broken
  manifest fails startup loudly; it is never skipped silently.
- **Only your tools:** set `[tools] bundled_tools = false` to serve
  exclusively your extensions.

## Trust: sandboxed by default, unsandboxed only when you say so

Kelter is secure by default: an extension runs sandboxed unless the operator
*explicitly* trusts a subprocess. A subprocess tool has OS-level network and
disk access the kernel cannot contain (ADR 0009), so running one is a
deliberate, attributable decision — never a silent default. It takes **two
independent parts** (ADR 0012):

1. **The manifest declares its trust mode.** A subprocess tool must set
   `[extension] trust = "subprocess-unsandboxed"`. A subprocess with no `trust`
   line — or one claiming `wasm-sandboxed` it can't back up, or an unknown
   value — is refused at startup, loudly. (A WASM tool is `wasm-sandboxed` by
   construction; you don't declare it, and declaring a subprocess trust on a
   WASM tool is an error.)
2. **The operator consents, in the daemon config.** Even a manifest that opts
   in loads only if the operator lists it by name:

   ```toml
   [trust]
   allow_unsandboxed_subprocess = ["wordstats"]
   ```

   The manifest is untrusted input and cannot authorise its own escape — so the
   operator gate is separate on purpose. Missing consent → the daemon refuses to
   start and tells you exactly which extension and which key to add.

**This is not a sandbox.** Allowing a subprocess-unsandboxed extension does not
contain it — it still has full OS egress. The name says so on purpose:
"subprocess-unsandboxed (operator-trusted)", never "sandbox" or "contained".
Want an actual sandbox for pure-compute logic? Use the WASM mechanism below.

The resolved trust posture is recorded per run — journaled and exposed on the
run object (`trust_posture`), so "what ran unsandboxed" is always answerable.
Proof: `scripts/smoke-trust.sh`.

## Memory extensions (bring your own backend)

A memory extension is an MCP server exposing three reserved tools that map
1:1 onto the provider contract:

```python
@mcp.tool()
def memory_write(episode: dict) -> dict: ...          # → {"written": true}

@mcp.tool()
def memory_retrieve(task: str, limit: int) -> dict:   # → {"results": [
    ...                                               #    {"episode_id", "summary",
                                                      #     "score" ∈ [0,1]}, …]}

@mcp.tool()
def memory_upsert(item: dict) -> dict: ...            # → {"upserted": true}
```

Manifest: `provides = ["memory"]`. The complete working example is
[`examples/extensions/memstore/`](../examples/extensions/memstore/) (~30
lines of Python; JSON-file store + overlap scoring). Smoke:
`scripts/smoke-memory-extension.sh` proves a real run writes its reflection
through the backend and retrieves it back — with no kernel recompile.

Things to know:

- **Registering a memory extension replaces the built-in durable memory** —
  that's why the example lives under `examples/extensions/`, not the
  auto-scanned `extensions/`: swapping storage should be an explicit act.
  Exactly one memory extension may be registered.
- The built-in memory itself is dogfooded through this same seam (the
  daemon spawns the bundled `memory-server` over the same protocol — no
  privileged path), so whatever the built-in can do, yours can.
- The kernel schema-validates every `memory_retrieve` response (shape,
  `score` within [0,1]) and rejects violations; a failing backend degrades
  context, never run integrity.
- `memory_upsert` is the knowledge-graph hook (`{"kind": "entity"|
  "relation", …}`) — a KG-backed memory implements ranking however it
  likes, as long as results come back in the contract shape.

## WASM extensions — bring your own logic, sandboxed by construction

Some extensions should run *inside* the kernel with **no ambient authority
at all** — no network, no disk, nothing you didn't grant. That's the WASM
mechanism (ADR 0011): compile your logic to WebAssembly in any language, and
the kernel runs it in-process through wasmtime with a capability sandbox.

This is a trust posture, not a detail: **you bring the logic; the kernel
decides what it can touch.** A pure-compute WASM module imports nothing, so
it *structurally cannot* reach the network or filesystem — not "we check and
block it", but "the capability isn't there to reach". A module that tries to
import a capability it wasn't granted simply fails to instantiate.

```toml
[extension]
name = "wordcount-wasm"
description = "Word statistics, compiled to WASM"
provides = ["tools"]
mechanism = "wasm"          # in-process, sandboxed — not a subprocess

# No [permissions] → no network, no disk. Pure compute.

[wasm]
module = "wordcount_wasm.wasm"   # relative to this directory
tool = "word_count"
```

The guest ABI is a minimal byte protocol over linear memory (core WASM, no
WASI, no component model): export `alloc(len) -> ptr`, `run(ptr, len) -> i64`
(result packed as `ptr<<32 | len`), and `memory`. The host writes the input
string in, calls `run`, reads a JSON result out. A WASM tool takes one
string argument `input` and returns a JSON object.

Complete working example: [`examples/wasm-tools/wordcount/`](../examples/wasm-tools/wordcount/)
(the Rust source) → compiled to [`examples/extensions/wordcount-wasm/`](../examples/extensions/wordcount-wasm/)
(manifest + `.wasm`). Rebuild the modules with `scripts/build-wasm-demo.sh`.
Smoke: `scripts/smoke-wasm.sh` proves the tool runs in-process AND that an
ungranted network capability is structurally blocked.

Because a WASM tool is a `tools`-capability action like any other, it also
works as a `tool` node in [plan mode](../docs/adr/0010-plan-mode.md) with no
extra work.

**Honest boundary:** WASM sandboxes the *WASM* mechanism. Subprocess
extensions (the MCP tool/memory/agent examples above) still run as OS
processes with OS-level network/disk access — WASM is a sandboxed
*alternative*, not a fix for the subprocess mechanism. Pick WASM when you
want the sandbox; pick subprocess when you want any language/binary and full
OS access.

## Remote agents (outbound A2A)

An `agent` extension is a **remote** agent Kelter delegates sub-tasks to
over the A2A protocol — nothing is spawned. Its manifest points at the
agent's Agent Card and MUST request `network`:

```toml
[extension]
name = "toy"
provides = ["agent"]

[permissions]
network = true              # REQUIRED — an agent without it is refused

[agent]
card_url = "http://127.0.0.1:7590"   # …/.well-known/agent-card.json
```

The kernel adds `delegate_to_toy(task)` to the action space; the model
delegates, the kernel dials the remote agent (A2A v1.0.0 JSON-RPC:
`SendMessage` → poll `GetTask` → collect the artifact text), and the answer
returns as a validated, journaled observation attributed to the agent.

Governance you get for free (ADR 0009):

- **Egress is the kernel's** — the dial is pinned to the `card_url` origin
  (SSRF allowlist); a card advertising an endpoint on another host is
  refused, and redirects are disabled.
- **Responses are validated** — an unknown A2A task state is rejected, like
  the memory schema gate.
- **Delegation is durable** — the remote task id is journaled at creation;
  a crash-resume reconnects (`GetTask`) instead of delegating twice.
- **Per-agent accounting** — `run.agent_usage` records delegations + remote
  time per agent.

Complete working target: [`examples/a2a-agent/`](../examples/a2a-agent/)
(a ~90-line Starlette A2A agent) with its manifest at
[`examples/extensions/toy-agent/`](../examples/extensions/toy-agent/). Smoke:
`scripts/smoke-a2a.sh` (delegate → validate → kill-mid-poll → reconnect,
plus the network-refusal check).

**Delegating to an authenticated agent.** If the remote endpoint requires a
bearer token (e.g. another Kelter's inbound endpoint, below), name an env var
holding it in the manifest — the token itself never goes in the tree:

```toml
[agent]
card_url  = "http://127.0.0.1:7601"
token_env = "KELTER_B_TOKEN"   # daemon reads the token from this env var
```

## Kelter as an A2A server (inbound)

The `agent` seam runs both ways. A Kelter daemon is itself an **A2A agent**:
it publishes an Agent Card and accepts delegated tasks (ADR 0013) — no config
needed beyond the API bearer token it already has.

- **Discover:** `GET /.well-known/agent-card.json` (public) returns the card —
  it names Kelter, lists the JSON-RPC interface (`…/a2a/rpc`), and **declares
  the bearer security scheme** so a client knows auth is required.
- **Delegate:** `POST /a2a/rpc` (JSON-RPC 2.0, **bearer required**) with
  `message/send` (or the v1.0 name `SendMessage`) starts a task; `tasks/get`
  polls it; `tasks/cancel` cancels it. An anonymous call is refused (`401`).
- **The task is a durable, governed run.** The A2A `task.id` *is* the kernel
  `run_id`: the delegated text runs through the full kernel (every action
  validated + journaled), and because the run is durable, the task **survives a
  server crash** — restart resumes it and `tasks/get` reconnects to the same
  id, with side effects applied exactly once. The caller is attributed in the
  run journal.

Because this is real A2A, any conformant client interops — a generic client
resolving the well-known card, or another Kelter delegating to it. Smoke:
`scripts/smoke-a2a-inbound.sh` (card + auth-refusal, `message/send`→durable
run, `tasks/get`/`tasks/cancel`, crash-mid-task reconnect, a **Kelter↔Kelter
loop** on localhost, and a generic A2A client — [`examples/a2a-generic-client.py`](../examples/a2a-generic-client.py),
urllib only, no SDK). Deferred: SSE streaming, push/webhooks, per-tool A2A
skills, plan-mode over A2A.

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
