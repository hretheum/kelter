# AGENTS.md — working conventions for this repo

**Kelter** — an agent-orchestration runtime: a deterministic kernel that
validates what a model proposes, model-agnostic via swappable policy adapters.
This file orients any Codex/agent working in the repo. Read it first.

## Current state (orientation, not a changelog)

A working, durable agent-orchestration runtime. The deterministic kernel
(`kernel-core`) owns control flow; a model is consulted only at decision points
and every proposed action is schema-validated before it can execute
(`ValidatedAction` has no public constructor but `ActionSchema::validate`, and
derives `Serialize` but **not** `Deserialize` — it is unforgeable). Around that
core: durable journal + crash-resume (ADR 0007), MCP tool substrate + a
capability registry (ADR 0002/0008), governed A2A delegation out+in with
trust/policy/ask + retry/circuit-breaker + streamed progress (ADR
0009/0013/0022/0023/0026/0027), plan mode (ADR 0010), WASM-sandboxed extensions
incl. a memory backend (ADR 0011/0024), advisory skills with model
self-activation + human-in-the-loop ask (ADR 0019/0020/0021), governed +
reversible self-learning (ADR 0016), retention/log hardening (ADR 0017),
security surface — bearer/principals/SSRF-pin/TLS (ADR 0018), and an optional
**shared** Postgres+pgvector memory backend so two independent kernels share
memory + learned facts, governance preserved (ADR 0028). React 19 + Tailwind
web UI is embedded in the daemon.

**Crate map.** `kernel-core` (kernel, journal, learned store, plan) ·
`policy` (model adapters: OpenRouter/local) · `mcp` (tool substrate + registry +
`memory-server`) · `memory` (provider trait, BM25/embeddings retrieval) ·
`storage` (SQLite `RunStore`/memory/learned — the default) · `pgstore`
(opt-in shared Postgres backend, ADR 0028; behind the daemon `postgres`
feature) · `a2a` (governed delegation client + inbound server) · `wasmhost`
(capability-by-absence WASM sandbox) · `skills` · `model-config` · `api`
(HTTP+WS contract, auth) · `daemon` (assembles everything, the `kelter` binary).
`apps/web-ui` (embedded UI) · `apps/desktop` (Tauri; excluded from the default
workspace).

**Where the detail lives.** Ongoing status/roadmap → `docs/ROADMAP.md`
(Eryk-owned — leave unstaged). Per-phase evidence + decisions →
`docs/BUILD-EVIDENCE.md`. Architecture → `docs/architecture.md` /
`architecture-overview.md`. Decisions → `docs/adr/` (0001–0028). The 2026-07-21
adversarial audit + its fix passes → `docs/audit/AUDIT-2026-07-21.md`.

**Testing.** `cargo test --workspace` is hermetic (235 tests; the `pgstore`
Postgres integration tests skip unless `KELTER_TEST_POSTGRES_URL` is set — use a
Docker/podman `pgvector/pgvector` container). `cargo clippy --workspace
--all-targets -D warnings` and `cargo fmt --check` are the bar. web-ui:
`pnpm --dir apps/web-ui test` (vitest) + `pnpm build`.

## Where things live (keep the root clean)

- **`prompts/`** — internal working briefs and prompt inputs: the phase prompts
  for the autonomous build agent, and the assumptions / founding doc. **These
  live here, never at the repo root**, so the root does not fill up with loose
  `.md` files. The folder is **gitignored** — briefs are inputs, not artifacts,
  and are never committed. Drop any new brief or prompt here.
- **`docs/`** — committed project documentation: `architecture.md`, `architecture-overview.md` (diagrammed synthesis), ADRs
  (`docs/adr/`), `BUILD-EVIDENCE.md`, `PHASE-1-RESULTS.md`, launch assets
  (`docs/launch/`), and raw run logs (`docs/evidence/`).
- **`crates/` · `apps/` · `deploy/`** — code and deployment artifacts.

## Hard rules

- **Naming.** The product is **Kelter**. The pre-rename working codename — the
  one this repo's own directory is named after — must never appear inside any
  tracked file; `git grep -i` for it stays at 0. The interim placeholder
  `agentkernel` is fully renamed to `kelter` and now survives ONLY inside the
  pre-rename run logs under `docs/evidence/` and the notes that explain them.
  Those are historical audit records — **do not scrub them.**
- **Human gate.** No `git push`, no making the repo public, no Product Hunt
  submission, no crate / npm / GitHub-org / domain registration. Those are the
  maintainer's acts, not an agent's.
- **Secrets** live in the environment only (`OPENROUTER_API_KEY`), never in the
  tree; mask them in logs.
- **Provenance.** `.md` files an agent authors carry `provenance: ai_authored`
  frontmatter (with `provenance_metadata`); untagged files are human-authored.
  This convention was adopted starting with ADR 0007 — the founding docs
  (ADRs 0001-0006, `architecture.md`) predate it and are untagged regardless
  of actual authorship; don't read their absence of frontmatter as a claim
  (audit finding 6-a, `docs/audit/AUDIT-2026-07-21.md`).
