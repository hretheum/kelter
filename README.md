# Kelter

**Agent orchestration that doesn't fall over when the model can't tool-call.**

Most agent frameworks make the LLM the runtime: the model's tool calls *are*
the control flow, so orchestration reliability equals the model's
tool-calling reliability. Kelter inverts that. A deterministic,
typed **kernel** owns the control flow — retries, persistence, validation,
error handling — and the model is a **constrained policy oracle** consulted
at explicit decision points. The model proposes; the kernel validates and
executes.

```
        ┌───────────────────────────────────────────────────────────┐
        │                     clients (thin by default)             │
        │        web UI (PWA)  ·  desktop shell  ·  anything HTTP   │
        └───────────────▲───────────────────────────────────────────┘
                        │  one API contract: HTTP + WebSocket, bearer token
        ┌───────────────┴───────────────────────────────────────────┐
        │                        kernel  (crates/kernel-core)       │
        │   deterministic state machine · schema validation · retry │
        │   persistence · gated reflection                          │
        └──────┬──────────────────────┬─────────────────────┬───────┘
               │ propose(context,     │ MCP (stdio /        │ write / retrieve
               │   action_schema)     │  in-process)        │ / upsert
        ┌──────▼───────┐      ┌───────▼────────┐    ┌───────▼────────┐
        │ policy plane │      │ tool substrate │    │ memory provider│
        │ (crates/     │      │ (crates/mcp)   │    │ (crates/memory)│
        │  policy)     │      │ every tool is  │    │ swappable      │
        │ local: GBNF  │      │ an MCP server  │    │ backends       │
        │ openrouter:  │      └────────────────┘    └────────────────┘
        │ tool-calling │
        └──────────────┘
```

## Why

- **Model-agnostic.** The same kernel drives a 0.5B local model and a
  frontier model. Swapping engines is **one line of config**, not a rewrite:

  ```toml
  [policy]
  engine = "local:qwen2.5-0.5b-instruct"
  # engine = "openrouter:anthropic/claude-sonnet-5"
  ```

- **Graceful degradation.** A weak engine degrades decision *quality*, never
  flow *reliability*. Locally, the action envelope is enforced by the
  decoder itself (GBNF constrained decoding in llama.cpp) — a malformed tool
  call is unrepresentable. Remotely, native tool-calling does the same job.
  Either way the kernel re-validates every proposal against JSON Schema
  before anything executes: it trusts no adapter.

- **Testable.** The kernel runs against a scripted mock oracle; policy
  adapters run against fixture HTTP servers; memory retrieval is a scored,
  unit-tested component. Orchestration stops being "hope the model
  complies".

## The demo slice

One real multi-step task runs **identically** on both engines — only the
config line above changes:

> *Read a document (URL or file) → extract its structure (title, headings,
> links) → write a markdown report.*

Three MCP tools, real decision points (the model picks each next action and
its arguments; the kernel narrows and validates), streamed live over
WebSocket into the UI. The bundled tool server is a normal MCP server —
swap in any other MCP server the same way.

```
step 0: read_source      {"input": "examples/sample.md", "save_as": "raw.md"}       ok
step 1: extract_structure{"source": "raw.md", "save_as": "structure.json"}          ok
step 2: write_report     {"source": "structure.json", "output": "report.md",
                          "format": "markdown"}                                     ok
finish: "report.md written"
```

Reproduce it: `scripts/smoke-local.sh` (local leg, needs llama.cpp) and
`scripts/smoke-openrouter.sh` (frontier leg, needs `OPENROUTER_API_KEY`).

## Features

| Area | v1 status |
|---|---|
| Deterministic kernel (state machine, retries, persistence) | ✅ trait + in-memory impl |
| Kernel-side schema validation of every model proposal | ✅ |
| `local` adapter — llama.cpp + GBNF constrained decoding | ✅ working |
| `openrouter` adapter — one adapter, many frontier models | ✅ working |
| Latitude knob (constrained / standard / wide per engine) | ✅ |
| Tools & plugins = MCP servers (no custom plugin API) | ✅ stdio + in-process |
| Memory provider interface (`write` / `retrieve` / `upsert`) | ✅ trait + in-memory impl, scored retrieval |
| Gated, provenance-tagged reflection (self-improvement stub) | ✅ episodic stub |
| One API contract: HTTP + WebSocket, bearer auth | ✅ |
| Web UI (PWA-installable, served by the daemon) | ✅ minimal |
| Desktop shell (standalone-embedded or remote-thin) | ✅ minimal |
| Container: Dockerfile, compose pod, Quadlet unit | ✅ |
| Graph/vector memory backends, CRDT sync, distillation | 🔜 interfaces only |

## Quickstart

### Server / headless daemon

```sh
# 1. local model sidecar (the "weak but reliable" engine)
llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096

# 2. the kernel daemon
cp config.example.toml config.toml
cargo run -p daemon -- --config config.toml
# ui:  http://127.0.0.1:7466/          (PWA-installable)
# api: http://127.0.0.1:7466/api/v1    (Authorization: Bearer <token>)

# 3. submit a run
curl -X POST http://127.0.0.1:7466/api/v1/runs \
  -H "Authorization: Bearer <token>" -H 'Content-Type: application/json' \
  -d '{"task": "Read the file ./examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish."}'
```

### Container (kernel + model as one pod)

```sh
cd deploy && docker compose up
# or, with Podman + systemd: copy deploy/kelter.container to
# ~/.config/containers/systemd/ (Quadlet)
```

### Desktop

```sh
cd apps/desktop/src-tauri && cargo tauri build
```

First run asks: **standalone** (kernel embedded in the app process) or
**remote** (thin client pointed at your daemon's URL — pairs well with a
tailnet). Same UI either way.

### Remote / PWA

Open `http://<your-daemon>:7466/` from any browser on the same network or
tailnet, paste the token, install as a PWA if you like. Clients are
stateless; the kernel owns all state.

## Where the reliability actually comes from

1. **Action space, not free text.** At each decision point the kernel offers
   a JSON-Schema action space (built from MCP tool schemas + `finish`).
2. **Constrained proposal.** The local adapter compiles that space to a GBNF
   grammar — the decoder cannot emit a malformed envelope. The remote
   adapter uses native tool-calling.
3. **Kernel validation on top.** Every proposal is re-validated against the
   schema in the kernel, whatever the adapter claims. Invalid → rejected
   with feedback → bounded retry.
4. **Deterministic anti-stall rules.** Exact repeats of earlier steps are
   rejected; under constrained latitude, already-succeeded tools leave the
   action space entirely.
5. **Everything is a step record.** Runs, steps, validation failures and
   reflection episodes are typed, persisted, and streamed.

## License

Dual-licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

---

*Status: walking skeleton with a working demo slice. Interfaces are the
product here; several implementations are deliberately minimal (in-memory
stores, naive retrieval scoring). See [docs/architecture.md](docs/architecture.md)
and the ADRs in [docs/adr/](docs/adr/).*
