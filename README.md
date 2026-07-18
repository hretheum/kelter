<!-- provenance: ai_authored | agent: claude-fable-5 | session_date: 2026-07-18
     human_validated: false | context: Phase 2 launch-grade README draft -->

# Kelter

**An agent runtime you deploy — with a deterministic kernel that stays
reliable when the model doesn't.**

Most agent stacks are frameworks: you compile your orchestration into an
app, and the LLM's tool calls *are* the control flow. When the model fumbles
a call, the workflow dies — so in practice you're locked into the few
biggest models, and "extending" means recompiling.

Kelter inverts both halves of that deal:

- **The kernel is the runtime; the model is a policy.** A typed, deterministic
  state machine owns control flow — retries, validation, persistence,
  journaling. The model only *proposes* the next action from a schema; the
  kernel validates and executes. Reliability is a property of the system,
  not of the model's mood.
- **You extend the runtime, not the source.** Every tool is an MCP server;
  new capabilities register through a manifest at startup — [~25 lines of
  Python](extensions/wordstats/server.py), zero Rust, zero recompilation —
  and still pass the same kernel-side validation as everything else.
- **Runs survive crashes — correctly.** Every decision and side effect is
  journaled (SQLite). Kill the daemon mid-run: it resumes to completion
  without re-querying the model for decisions already made and without
  re-firing side effects — exactly-once, receipt-gated, or an honest
  failure. Never a silent duplicate.

## Sixty seconds of proof

**1 · The engine is one line of config.** The same multi-step task — read a
document, extract its structure, write a report over three MCP tools — runs
identically on a 0.5 B local model and on a frontier model:

```diff
 [policy]
-engine = "local:qwen2.5-0.5b-instruct"
+engine = "openrouter:anthropic/claude-sonnet-5"
```

```
── local  (llama.cpp + GBNF)          →  completed in 2s
── frontier (via OpenRouter)          →  completed in 12s
   identical step sequence, same kernel, same tools
```

Reproduce: `scripts/demo-side-by-side.sh`

**2 · Extend it in Python over lunch.** Drop a directory with a manifest and
an MCP server next to the runtime; restart; the model can use your tool:

```
$ curl …/api/v1/tools
{"tools":[…, {"name":"word_stats", …}]}         # no kernel rebuild happened
```

Reproduce: `scripts/smoke-extension.sh` · guide: [`extensions/`](extensions/README.md)

**3 · Kill it mid-run. Watch it finish anyway.** A run is killed *after* a
ledger-append side effect fired but *before* its receipt landed — the worst
crash window. On restart the run resumes from the journal (the model is not
re-asked for decisions it already made) and the tool dedupes on the
journaled idempotency key:

```
[resume] replaying journaled decision — model NOT queried
[resume] dangling intent — re-dispatching with the journaled idempotency key
→ run completed · ledger has exactly ONE line
```

Reproduce: `scripts/smoke-durability.sh` · design: [ADR 0007](docs/adr/0007-durable-execution-journal-replay.md)

## Why the weak-model story matters

Small local models are cheap, private and fast — and terrible at free-form
tool calling. Kelter makes that irrelevant:

1. **Action space, not free text.** Each decision point offers a JSON-Schema
   action space built from MCP tool schemas plus a kernel-owned `finish`.
2. **Constrained proposal.** Locally, the space is compiled to a GBNF
   grammar — the *decoder* cannot emit a malformed call. Remotely, native
   tool-calling does the same job.
3. **Kernel validation on top, always.** Every proposal is re-validated
   in the kernel regardless of adapter; invalid → rejected with feedback →
   bounded retry. The kernel trusts no adapter and no extension.
4. **Deterministic anti-stall rules.** Exact repeats are rejected; under
   constrained latitude, tools that already succeeded leave the action
   space entirely — a weak model *cannot* loop.
5. **Everything is journaled.** Runs, steps, rejections and reflection
   episodes are typed, persisted, and streamed live over WebSocket.

A weak engine degrades decision *quality*; it cannot degrade flow
*integrity*. That's the graceful-degradation contract, and it's enforced by
code you can read.

```
        ┌───────────────────────────────────────────────────────────┐
        │                     clients (thin by default)             │
        │        web UI (PWA)  ·  desktop shell  ·  anything HTTP   │
        └───────────────▲───────────────────────────────────────────┘
                        │  one API contract: HTTP + WebSocket, bearer token
        ┌───────────────┴───────────────────────────────────────────┐
        │                        kernel  (crates/kernel-core)       │
        │   deterministic state machine · schema validation · retry │
        │   persistence · journaling · gated reflection             │
        └──────┬──────────────────────┬─────────────────────┬───────┘
               │ propose(context,     │ MCP (stdio /        │ write / retrieve
               │   action_schema)     │  in-process)        │ / upsert
        ┌──────▼───────┐      ┌───────▼────────┐    ┌───────▼────────┐
        │ policy plane │      │ tool substrate │    │ memory provider│
        │ local: GBNF  │      │ every tool is  │    │ swappable      │
        │ openrouter:  │      │ an MCP server; │    │ backends       │
        │ tool-calling │      │ extensions via │    │                │
        └──────────────┘      │ manifest       │    └────────────────┘
                              └────────────────┘
```

## Quickstart

One command from clone to a first completed agent run (macOS/Linux;
needs Rust + [llama.cpp](https://github.com/ggml-org/llama.cpp)):

```sh
scripts/bootstrap.sh
# → pulls a small model, starts the sidecar + daemon, runs the demo task,
#   opens the UI. First run ≈ a few minutes (model download + build);
#   warm runs ≈ under a minute.
```

<details>
<summary>Manual path / other topologies</summary>

**Headless daemon**

```sh
llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096
cp config.example.toml config.toml
cargo run -p daemon -- --config config.toml
# ui:  http://127.0.0.1:7466/   ·   api: /api/v1 (Bearer token)
```

**Container (kernel + model as one pod)**

```sh
cd deploy && docker compose up
# Podman/systemd: copy deploy/kelter.container to ~/.config/containers/systemd/
```

**Desktop**

```sh
cd apps/desktop/src-tauri && cargo tauri build
```

First run asks: **standalone** (kernel embedded in-process) or **remote**
(thin client at your daemon's URL — pairs well with a tailnet).

**Any browser / PWA**

Open `http://<your-daemon>:7466/`, paste the token, install as a PWA.
Clients are stateless; the kernel owns all state.

</details>

## Features

| Area | Status |
|---|---|
| Deterministic kernel: state machine, retries, journaling | ✅ |
| **Durable execution**: SQLite journal, crash-resume, exactly-once keyed side effects | ✅ |
| Per-run token accounting (`run.usage` via the API) | ✅ |
| OpenTelemetry spans (run/step/model/validate/tool) over OTLP | ✅ |
| Kernel-side schema validation of every proposal (typed trust boundary) | ✅ |
| `local` adapter — llama.cpp + GBNF constrained decoding | ✅ |
| `openrouter` adapter — one adapter, many frontier models | ✅ |
| Latitude knob (constrained / standard / wide per engine capability) | ✅ |
| Tools = MCP servers; **manifest-registered extensions, no recompile** | ✅ first slice |
| Extension example in ~25 lines of Python (official MCP SDK) | ✅ |
| Memory provider interface (`write` / `retrieve` / `upsert`), BM25 retrieval | ✅ persistent (SQLite) |
| Gated, provenance-tagged reflection (self-improvement stub) | ✅ |
| One API contract: HTTP + WebSocket, bearer auth | ✅ |
| Web UI (PWA) · desktop shell (standalone/remote) · container/Quadlet | ✅ |
| **WASM in-process, capability-sandboxed extensions** | ✅ [ADR 0011](docs/adr/0011-wasm-extension-mechanism.md) |
| **Memory & agents as registered extensions** (capability registry) | ✅ [ADR 0008](docs/adr/0008-capability-registry.md) |
| **Governed durable A2A** — delegate to remote agents, resumable | ✅ [ADR 0009](docs/adr/0009-governed-durable-a2a.md) |
| **Plan mode** — validated, durable DAG of tool / A2A / review nodes | ✅ [ADR 0010](docs/adr/0010-plan-mode.md) |
| Sandbox-by-default extension trust · marketplace · CRDT sync | 🔜 [ADR 0012](docs/adr/0012-extension-trust-and-sandbox-by-default.md) |

## Design notes

The architecture and each load-bearing decision are documented:
[architecture](docs/architecture.md) ·
[kernel-as-runtime](docs/adr/0001-kernel-as-runtime.md) ·
[MCP as substrate](docs/adr/0002-mcp-as-tool-substrate.md) ·
[adapter contract](docs/adr/0003-adapter-contract-constrained-vs-native.md) ·
[topology-as-config](docs/adr/0004-topology-as-config.md) ·
[memory provider](docs/adr/0005-memory-provider-interface.md) ·
[governed extensions](docs/adr/0006-governed-microkernel-extension-model.md) ·
[durable execution](docs/adr/0007-durable-execution-journal-replay.md) ·
[observability](docs/observability.md)

Status: young project, honest scope — the kernel, both adapters, the demo
slice, the extension slice and crash-resume all work end-to-end (see
[docs/BUILD-EVIDENCE.md](docs/BUILD-EVIDENCE.md) for receipts); several
backends are deliberately in-memory reference implementations behind stable
traits.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
