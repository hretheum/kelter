<!-- provenance: ai_authored | agent: claude-opus-5-5 | session_date: 2026-10-14
     human_validated: false | context: launch README (roadmap KR3.1), rewritten
     from the Phase 2 draft to cover governance (ADR 0033-0040) and the
     Claude Code executor; every claim links to code, an ADR or a script. -->

# Kelter

**An agent runtime you deploy. A deterministic kernel decides what the model
may do, asks a human before anything expensive, and keeps a journal that
proves what happened.**

Most agent stacks are frameworks: you compile your orchestration into an
app, and the model's tool calls *are* the control flow. When the model
fumbles a call, the workflow dies; when it calls the wrong thing, nothing
stops it; when it says "done", you take its word for it.

Kelter moves all three into a kernel you can read:

- **The kernel is the runtime; the model is a policy.** A typed,
  deterministic state machine owns control flow: retries, validation,
  persistence, journaling. The model only *proposes* the next action from a
  schema, and the kernel validates it before anything runs. A validated
  action has no public constructor except the validator.
- **Governance is in the kernel, not in your prompt.** Mutating actions can
  require a human's approval, budgets stop a run *before* it crosses a limit,
  and "completed" means your success contract was met, not that the model
  said so. Every refusal and every human decision is journaled.
- **Runs survive crashes, correctly.** Kill the daemon mid-run: it resumes
  without re-asking the model for decisions it already made and without
  re-firing side effects. Exactly once, or an honest failure.
- **You extend the runtime, not the source.** Every tool is an MCP server or
  a sandboxed WASM module registered through a manifest at startup.

## Four proofs you can run

**1 · The engine is one line of config.** The same multi-step task runs
identically on a 0.5B local model and on a frontier model:

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

**2 · Kill it mid-run. Watch it finish anyway.** The run is killed *after* a
side effect fired but *before* its receipt landed, the worst crash window.
On restart it resumes from the journal and the tool dedupes on the
journaled idempotency key:

```
[resume] replaying journaled decision — model NOT queried
[resume] dangling intent — re-dispatching with the journaled idempotency key
→ run completed · ledger has exactly ONE line
```

Reproduce: `scripts/smoke-durability.sh` · design: [ADR 0007](docs/adr/0007-durable-execution-journal-replay.md)

**3 · Ask before it costs.** Put a mutating tool under `ask` and give the run
a budget:

```toml
[approval.actions]
deploy_branch = "ask"        # pause, wait for a human; deny after a timeout

[budget]
max_usd = 0.50
max_wall_secs = 1800
```

The run pauses before the action, the pause survives a daemon restart, and
an `allow` executes that exact proposal once. A token or wall-clock budget
stops the run *before* the call that would cross it; the call is never made.

Reproduce: `scripts/smoke-human-gate-budgets.sh` · design: [ADR 0038](docs/adr/0038-human-gate-on-mutating-actions.md), [ADR 0039](docs/adr/0039-cost-budgets-enforced-before-crossing.md)

**4 · "Done" means checked.** Give a run a success contract:

```json
{"task": "write the weekly report",
 "success_contract": {"require": [{"kind": "action_succeeded", "action": "write_report"}]}}
```

If the model says it finished but `write_report` never succeeded, the run ends
`incomplete`, naming the unmet condition. Runs without a contract are marked
unverified, and the self-learning layer learns only from verified runs or
ones a human confirmed.

Reproduce: `scripts/smoke-success-contract.sh` · design: [ADR 0033](docs/adr/0033-success-requires-an-effective-action.md), [ADR 0037](docs/adr/0037-success-contract-and-verification.md)

## Why the weak-model story matters

Small local models are cheap and private, and terrible at free-form tool
calling. Kelter makes that matter less:

1. **Action space, not free text.** Each decision point offers a JSON-Schema
   action space built from tool schemas plus a kernel-owned `finish`.
2. **Constrained proposal.** Locally, the space is compiled to a GBNF
   grammar, so the decoder cannot emit a malformed call. Remotely, native
   tool calling does the same job.
3. **Kernel validation on top, always.** Every proposal is re-validated in
   the kernel regardless of adapter; invalid means rejected with feedback
   and a bounded retry.
4. **Deterministic anti-stall rules.** Exact repeats are rejected; a weak
   model cannot loop.

A weak engine degrades decision *quality*. It cannot degrade flow
*integrity*, and that contract is enforced by code you can read.

```
        ┌───────────────────────────────────────────────────────────┐
        │   clients: web UI (PWA) · desktop shell · anything HTTP   │
        └───────────────▲───────────────────────────────────────────┘
                        │  HTTP + WebSocket, bearer token, principals
        ┌───────────────┴───────────────────────────────────────────┐
        │                  kernel  (crates/kernel-core)             │
        │  state machine · schema validation · retry · journal      │
        │  approval gate · budgets · success contract · plan mode   │
        └──────┬──────────────────┬──────────────────┬──────────────┘
               │ propose(context, │ MCP / WASM /     │ memory provider
               │   action_schema) │ A2A              │ (SQLite, Postgres)
        ┌──────▼───────┐  ┌───────▼────────┐  ┌──────▼───────────┐
        │ policy plane │  │ tool substrate │  │ governed,        │
        │ local: GBNF  │  │ extensions via │  │ reversible       │
        │ remote: tool │  │ manifest, trust│  │ self-learning    │
        │ calling      │  │ by absence     │  │ (verified runs)  │
        └──────────────┘  └────────────────┘  └──────────────────┘
```

## Quickstart

One command from clone to a first completed agent run (macOS/Linux; needs
Rust, Node 22 + pnpm, and [llama.cpp](https://github.com/ggml-org/llama.cpp)):

```sh
scripts/bootstrap.sh
# pulls a small model, starts the sidecar + daemon, runs the demo task,
# opens the UI. First run: expect several minutes for the build (a clean
# machine needed about 7 for build + full test run) plus the model download.
```

**Choose your model in the UI.** Open the UI, click ⚙ Model, pick a local
model (llama.cpp `:8080` or Ollama `:11434`) or an API model (OpenRouter,
paste a key), test the connection and save. The daemon swaps the model live.
API keys are stored server-side in a `0600` file or read from
`OPENROUTER_API_KEY`, never committed or logged.

<details>
<summary>Manual path / other topologies</summary>

**Headless daemon**

```sh
llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096
cp config.example.toml config.toml
# the daemon embeds the web UI, so build it first (once):
pnpm --dir apps/web-ui install --frozen-lockfile && pnpm --dir apps/web-ui build
cargo run -p daemon -- --config config.toml
# ui:  http://127.0.0.1:7466/   ·   api: /api/v1 (Bearer token)
```

**Container**

```sh
cd deploy && docker compose up
# Podman/systemd: deploy/kelter.container as a Quadlet unit,
# deployment specifics as drop-ins (example: deploy/k12/README.md)
```

**Desktop**

```sh
cd apps/desktop/src-tauri && cargo tauri build
```

First run asks: **standalone** (kernel embedded) or **remote** (thin client
at your daemon's URL). Clients are stateless; the kernel owns all state.

</details>

No model needed for two of the proofs: `scripts/smoke-human-gate-budgets.sh`
and `scripts/smoke-success-contract.sh` drive the real daemon with a scripted
model stub (about 2 minutes and 10 seconds respectively on a warm build).

## Using it with Claude Code

[`deploy/claude-code/`](deploy/claude-code/README.md) runs Claude Code as a
headless executor under the kernel. Kelter decides the path for a coding
task (no coding agent, a cheap model, a mid-priced one, or Opus), the most
expensive path always waits for a human, and the task is done only when a
named check (your tests) passes on the branch Claude Code produced. Claude
Code runs in its own container, never pushes, and hands results back as a
git bundle.

## Features

| Area | Status |
|---|---|
| Deterministic kernel: state machine, retries, schema validation of every proposal | ✅ [ADR 0001](docs/adr/0001-kernel-as-runtime.md) |
| Durable execution: SQLite journal, crash-resume, exactly-once keyed side effects | ✅ [ADR 0007](docs/adr/0007-durable-execution-journal-replay.md) |
| Refusals and resumes in the run's journal history | ✅ [ADR 0036](docs/adr/0036-refusals-and-resumes-in-the-journal.md) |
| Human gate on mutating actions (`allow` / `ask` / `deny`, pause survives restart) | ✅ [ADR 0038](docs/adr/0038-human-gate-on-mutating-actions.md) |
| Cost budgets (USD, tokens, wall clock, daily per principal), enforced before crossing | ✅ [ADR 0039](docs/adr/0039-cost-budgets-enforced-before-crossing.md) |
| Success contract, `incomplete` status, `verification` on every success | ✅ [ADR 0033](docs/adr/0033-success-requires-an-effective-action.md), [0037](docs/adr/0037-success-contract-and-verification.md) |
| Policy adapters: local (llama.cpp + GBNF) and OpenRouter; live model swap from the UI | ✅ [ADR 0003](docs/adr/0003-adapter-contract-constrained-vs-native.md), [0015](docs/adr/0015-runtime-model-config-and-ui.md) |
| Tools as MCP servers; manifest-registered extensions, no recompile | ✅ [ADR 0002](docs/adr/0002-mcp-as-tool-substrate.md), [0008](docs/adr/0008-capability-registry.md) |
| WASM extensions, capability-sandboxed (incl. a host-validated workspace write) | ✅ [ADR 0011](docs/adr/0011-wasm-extension-mechanism.md), [0034](docs/adr/0034-wasm-workspace-write-capability.md) |
| Extension trust: sandbox by default, unsandboxed only with operator opt-in; a refused extension is skipped, not fatal | ✅ [ADR 0012](docs/adr/0012-extension-trust-and-sandbox-by-default.md), [0035](docs/adr/0035-refused-extensions-are-skipped.md) |
| Plan mode: validated, durable DAG, parallel fan-out, bounded retry and re-planning | ✅ [ADR 0010](docs/adr/0010-plan-mode.md), [0022](docs/adr/0022-multiagent-parallel-fanout-governed-delegation.md), [0030](docs/adr/0030-plan-mode-resilience.md) |
| Governed A2A delegation, outbound and inbound, with trust, ask, retry and circuit breaker | ✅ [ADR 0009](docs/adr/0009-governed-durable-a2a.md), [0013](docs/adr/0013-a2a-inbound-kelter-as-server.md), [0023](docs/adr/0023-agent-trust-policy-resilience.md) |
| Skills: advisory knowledge, model self-activation, human-in-the-loop ask | ✅ [ADR 0019](docs/adr/0019-skills-advisory-knowledge.md)–[0021](docs/adr/0021-skills-interactive-activation.md) |
| Memory provider with BM25 / embeddings retrieval; shared Postgres + pgvector backend | ✅ [ADR 0005](docs/adr/0005-memory-provider-interface.md), [0028](docs/adr/0028-shared-postgres-memory-backend.md) |
| Governed, reversible self-learning (facts and plans), only from verified runs | ✅ [ADR 0016](docs/adr/0016-governed-self-learning.md), [0032](docs/adr/0032-plan-distillation.md), [0040](docs/adr/0040-unverified-reflection-episodes.md) |
| Security surface: bearer tokens, principals, SSRF pinning, TLS; retention and log hardening | ✅ [ADR 0017](docs/adr/0017-data-lifecycle-and-log-hardening.md), [0018](docs/adr/0018-security-hardening.md) |
| OpenTelemetry spans over OTLP | ✅ [observability](docs/observability.md) |
| Web UI (PWA) · desktop shell (standalone / remote) · container / Quadlet | ✅ |
| Claude Code as a headless executor with a cost path | ✅ [deploy/claude-code](deploy/claude-code/README.md) |
| Packaging (`.dmg`, signing), OIDC / mTLS, OS-level process isolation, eval and replay tooling | 🔜 [roadmap](docs/ROADMAP.md) |

## Status

Young project, one maintainer, honest scope. What exists is tested: about
430 Rust tests run hermetically, plus Postgres integration tests in CI,
plus scripted smoke tests that drive the real daemon (`scripts/smoke-*.sh`).
Kelter runs in production on a home server, where a 26-scenario acceptance
suite drives the live deployment end to end. The receipts, including what
went wrong and how it was fixed, are in
[docs/BUILD-EVIDENCE.md](docs/BUILD-EVIDENCE.md); an adversarial audit and
its fixes are in [docs/audit](docs/audit/AUDIT-2026-07-21.md).

Most of the code and documentation is written by AI agents under the
maintainer's direction: the architecture, the rules and every decision
marked as the maintainer's in the ADRs are human. Documents an agent wrote
say so in their `provenance` front matter.

## Design notes

[Architecture](docs/architecture.md) ·
[overview with diagrams](docs/architecture-overview.md) ·
[all decisions (ADRs 0001–0040)](docs/adr/) ·
[roadmap](docs/ROADMAP.md) ·
[observability](docs/observability.md)

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
