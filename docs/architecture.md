# Architecture

The load-bearing inversion: **the kernel is the runtime; the model is a
constrained policy oracle.** Everything else follows from it.

## The anti-pattern this replaces

Contemporary agent stacks make the LLM the runtime — the model's tool calls
*are* the control flow. That couples orchestration reliability to model
tool-calling reliability: a weak engine breaks the whole system, and every
engine has to be puppeted through its own quirks. The same coupling shows up
when orchestrators drive coding agents by scraping terminal text — the
model's *prose* becomes a protocol.

## Layers

### Control plane — `crates/kernel-core`

A deterministic state machine owns flow, retries, persistence and error
handling. Model interface:

```rust
propose(context: &PolicyContext, schema: &ActionSchema) -> ProposedAction
```

Trust boundary, encoded in types: adapters return `ProposedAction`
(untrusted); the only constructor of `ValidatedAction` is
`ActionSchema::validate` (JSON-Schema validation, kernel-side). The executor
takes `ValidatedAction` only, so unvalidated model output cannot reach a
tool by construction.

Per-run loop (see `kernel_core::engine`):

1. Build the action space: MCP tool schemas + kernel-owned `finish`.
2. Under `constrained` latitude, drop tools that already succeeded
   (decomposition pressure; a weak model cannot stall on one tool).
3. `propose` → validate → on violation: reject with feedback, bounded retry.
4. Reject exact repeats of earlier steps (deterministic anti-stall).
5. Execute via MCP; tool failure becomes context feedback, not a crash.
6. After the terminal state: **gated** reflection writes one
   provenance-tagged episode to memory.

### Policy plane — `crates/policy`

One adapter contract: *return a proposal drawn from the schema, or fail
loudly.* Two working adapters:

- `local` — llama.cpp server over HTTP. The action schema is compiled to a
  **GBNF grammar** (`policy::gbnf`) and sent per-request; the decoder itself
  cannot emit a malformed envelope. Verified against llama.cpp b10050
  (`/v1/chat/completions` accepts the `grammar` extension and applies the
  chat template server-side).
- `openrouter` — OpenRouter unified chat-completions API with native
  tool-calling; one adapter reaches many frontier models via the model
  string. Key from `OPENROUTER_API_KEY` only.

The **latitude** knob (`constrained | standard | wide`) maps engine
capability to action-space treatment: guidance injection, retry budget,
space narrowing, duplicate policy. Selection is one config line:
`engine = "<adapter>:<model>"`.

### Tool substrate — `crates/mcp`

Zero custom plugin API (ADR 0002). The kernel's `ActionSource` seam is
implemented by an MCP *client* that spawns configured servers (stdio child
processes; in-process duplex for the standalone desktop), flattens their
tool JSON Schemas into the action space, and executes validated actions as
`call_tool`. The bundled reference tools (`read_source`,
`extract_structure`, `write_report`) ship as a normal MCP server binary
(`tools-server`) and get no special treatment.

### Executors

Long-running coding work is delegated to headless executors with a typed
contract — never by scraping an interactive terminal. Reference contract
(verified against current Claude Code docs): `claude -p --output-format
json` returns a structured result object (with `stream-json` for events and
`session_id` for resumption); the Agent SDK (`@anthropic-ai/claude-agent-sdk`,
`claude-agent-sdk` on PyPI) exposes the same as a typed `query()` iterator
with `allowed_tools` and permission modes for unattended runs. Claude Code
is itself a native MCP client (`claude mcp add`), so tools registered for
the kernel are reusable by the executor — one substrate, no seam. Executor
integration is an interface in v1, not an implementation.

### Memory — `crates/memory`

Provider protocol: `write(episode)` / `retrieve(task) → ranked_context` /
`upsert(entity|relation)`. Reference impl is in-memory with a scored,
unit-tested retrieval; graph/vector backends plug in behind the same trait
(ADR 0005). All writes are provenance-tagged and versioned; the reflection
stub is gated by config. No silent self-mutation.

### API — `crates/api`

The one stable contract every client uses (ADR 0004): HTTP for commands
(`POST /api/v1/runs`, `GET /api/v1/runs/{id}`), WebSocket for event
streaming (`/api/v1/events`), bearer-token auth, embedded web UI at `/`.
Designed to sit on localhost or a private network (tailnet); never exposed
publicly without extra hardening.

### Topologies — config, not code

- **daemon** (`crates/daemon`): headless binary = kernel + API + UI.
- **desktop standalone** (`apps/desktop`): the same crates linked into the
  Tauri process; daemon runs in-process on an ephemeral port; webview points
  at it.
- **remote client**: the same shell (or any browser/PWA) pointed at a
  daemon URL; no kernel code on the client.

The kernel owns state; clients are stateless views. CRDT offline sync is
explicitly v2+.

## Crate map

| Crate | Role | Key public items |
|---|---|---|
| `kernel-core` | state machine, validation, events | `Kernel`, `ActionSchema`, `ValidatedAction`, `PolicyOracle`, `ActionSource`, `Latitude` |
| `policy` | model adapters | `LocalAdapter`, `OpenRouterAdapter`, `PolicyConfig`, `build_oracle`, `gbnf::grammar_for` |
| `mcp` | tool substrate | `McpToolSource`, `ServerSpec`, `server::ToolServer`, bin `tools-server` |
| `memory` | memory providers | `MemoryProvider`, `InMemoryProvider`, `Episode`, `Provenance` |
| `api` | HTTP+WS contract | `router`, `serve`, `serve_listener` |
| `daemon` | headless assembly | bin `agentkernel` |
| `apps/desktop` | Tauri shell | commands `start_standalone`, `open_ui` |
