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

**Extensions — the capability registry (ADR 0006 → ADR 0008):** a
directory with a `kelter-extension.toml` manifest declares exactly one
capability, requested permissions, and a start command; the registry routes
it to the matching kernel seam. `tools` extensions join the action space
(kernel validation + journal); `memory` extensions serve the reserved
`memory_write`/`memory_retrieve`/`memory_upsert` MCP tools behind the
`MemoryProvider` seam with kernel-side response-schema validation; `agent`
extensions declare a **remote** A2A endpoint (Agent Card URL + required
`network` permission) and join the action space as `delegate_to_<name>`;
the kernel dials them as a governed A2A client — origin-pinned egress
(SSRF allowlist), validated responses, and durable delegation (the remote
task id is journaled at creation so a crash-resume reconnects via `GetTask`
instead of re-delegating). `model-provider` is reserved and refuses to load
until implemented. **WASM tool extensions** (`mechanism = "wasm"`, ADR 0011) run in-process through wasmtime with a capability sandbox — the host grants only the permissions the manifest declares (default: none), so an ungranted network/disk capability is structurally unreachable; a WASM tool is an action-space `tool` like any other (works as a plan node too).
Unknown capabilities or permission keys fail startup loudly. The built-in
durable memory is dogfooded through this same seam (spawned
`memory-server`, no in-process shortcut); an external memory extension
replaces it. Reference examples: a Python tool in ~25 lines
(`extensions/wordstats/`) and a Python memory backend in ~30 lines
(`examples/extensions/memstore/`).

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

### Durability — `crates/storage` + the journal (ADR 0007)

Runs, their journal and memory persist in SQLite (WAL; one file, migrations
via `user_version`; in-memory backend remains for tests). Every run appends
`Decision → Intent → Receipt → Terminal` journal entries as it progresses.
On restart the daemon resumes non-terminal runs: journaled decisions replay
**without re-querying the model**, receipts skip re-execution, and a
dangling intent (crash mid-effect) is re-dispatched with the journaled
idempotency key when the tool declares support — or fails loudly when it
doesn't. Exactly-once side effects under kill-and-resume are proven by
`scripts/smoke-durability.sh`. The journal also carries per-model-call
token usage, aggregated on the run record (`run.usage`).

### Plan mode (ADR 0010)

With `[kernel] mode = "plan"` the model proposes a whole DAG up front
(`propose_plan`, schema-constrained) instead of one action at a time. The
kernel **validates** it before executing anything — unique ids, no cycles
(topological sort), allowed node types, referenced actions exist in the
registry — and rejects an invalid plan pre-execution. Nodes are typed
`tool` / `a2a` / `review` and each executes through the *existing* seam
(the action space, the A2A delegation source, or a kernel-owned review
decision), so per-node validation, journaling and A2A egress governance
still apply. The plan is a memoized decision: journaled once as
`PlanProposed`, replayed on resume without re-querying the model, with node
receipts skipping completed nodes (durable DAG execution generalizing
ADR 0007). Node failure fails the plan explicitly; adaptive re-planning is
a deferred follow-up.

### Observability

`tracing` spans — `kelter.run` → `kelter.step` → `kelter.model_call` /
`kelter.validate` / `kelter.tool_call` — bridge to OpenTelemetry when
`[otel] endpoint` is configured (OTLP/HTTP; see `docs/observability.md`).

### Memory — `crates/memory`

Provider protocol: `write(episode)` / `retrieve(task) → ranked_context` /
`upsert(entity|relation)`. Retrieval is BM25 (dependency-free, tested on a
query contract in `memory::retrieval`); the durable backend stores episodes
and graph upserts in the shared SQLite file. Bring-your-own backends
(knowledge graph, vector store) plug in behind the same trait (ADR 0005) —
implement `MemoryProvider`, wire it where the daemon selects the storage
backend. All writes are provenance-tagged and versioned; the reflection
stub is gated by config. No silent self-mutation.

### API — `crates/api`

The one stable contract every client uses (ADR 0004): HTTP for commands
(`POST /api/v1/runs`, `GET /api/v1/runs/{id}`, `GET /api/v1/tools`), WebSocket for event
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
| `mcp` | tool substrate + capability registry | `McpToolSource`, `McpMemoryProvider`, `extensions::{discover, Capability}`, `server::ToolServer`, `memory_server::MemoryToolServer`, bins `tools-server`/`ledger-server`/`memory-server` |
| `memory` | memory providers | `MemoryProvider`, `InMemoryProvider`, `Episode`, `Provenance` |
| `api` | HTTP+WS contract | `router`, `serve`, `serve_listener` |
| `storage` | durable persistence | `Db`, `SqliteRunStore`, `SqliteMemoryProvider` |
| `a2a` | outbound A2A client (ADR 0009) | `A2aActionSource`, `A2aClient`, `RemoteAgent` |
| `wasmhost` | in-process WASM tool mechanism (ADR 0011) | `WasmToolSource`, `Grants` |
| `kernel-core::plan` | plan validation (DAG/cycles/types) + durable DAG execution | `Plan`, `PlanNode`, `validate`, `ValidatedPlan` |
| `daemon` | headless assembly, resume-on-startup, OTel init, composite action source | bin `kelter` |
| `apps/desktop` | Tauri shell | commands `start_standalone`, `open_ui` |
