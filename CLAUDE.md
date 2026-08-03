# CLAUDE.md — working conventions for this repo

**Kelter** — an agent-orchestration runtime: a deterministic kernel that
validates what a model proposes, model-agnostic via swappable policy adapters.
This file orients any Claude/agent working in the repo. Read it first.

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
`architecture-overview.md`. Decisions → `docs/adr/` (0001–0031). The 2026-07-21
adversarial audit + its fix passes → `docs/audit/AUDIT-2026-07-21.md`.

**Testing.** `cargo test --workspace` is hermetic — 266 tests, plus 7
`pgstore` tests that are `#[ignore]`d because they need a real Postgres. To run
those too: start a `pgvector/pgvector:pg16` container, export
`KELTER_TEST_POSTGRES_URL`, and add `-- --include-ignored` (273 total). CI does
this via a `services:` block. Bare `cargo test` reports them as `7 ignored`,
never as a green pass. `cargo clippy --workspace
--all-targets -D warnings` and `cargo fmt --check` are the bar. web-ui:
`pnpm --dir apps/web-ui test` (vitest) + `pnpm build`.

**The excluded-workspace hazard — read this before touching shared types.**
`Cargo.toml` `exclude`s three crates from the default workspace:
`apps/desktop/src-tauri`, `examples/wasm-tools/wordcount`,
`examples/wasm-tools/netpeek`. `cargo test --workspace` does NOT compile them.
On 2026-08-03 this bit hard: `PolicyConfig` gained three fields and `Run` gained
`max_steps`, the desktop shell's struct literals were never updated, and the
desktop app was unbuildable for 12 days without a single local signal. If you
add a field to a type the desktop links (`PolicyConfig`, `Run`, anything from
`kernel-core`/`policy`/`api`), you MUST also run, from
`apps/desktop/src-tauri`: `cargo fmt --all --check`, `cargo clippy
--all-targets -- -D warnings`, `cargo test --all-targets`. CI now does this too,
but locally the root workspace will happily go green while the desktop is
broken. The two `wasm-tools` crates are lower risk (no workspace deps) and
currently compile clean.

## Where work stands (2026-08-03) and what comes next

**What just happened.** A full day went into CI and evidence hygiene, not
features. In short: CI had been dead since 2026-07-19 — `pnpm/action-setup@v4`
failed on step one because `apps/web-ui/package.json` had no `packageManager`
key, so the `check` job never ran for 51 commits and `container`/`desktop`
(both `needs: check`) never ran at all. Fixing that surfaced the desktop
breakage above, plus a second one in `tests/durable_store.rs` that even the old
CI would have missed (`cargo tauri build` compiles the lib, not test targets).
Separately, the 7 `pgstore` tests had been returning a green `ok` without
touching a database — the env guard was an early `return`, and the variable was
set nowhere in CI. All fixed; the pipeline is green end to end for the first
time since July 19. Full detail in `docs/BUILD-EVIDENCE.md` (entries dated
2026-08-03). ADR 0031 was also signed off, and `docs/ROADMAP.md` "Where we are"
was brought current through Phase 22.

**Recommended next step: add vitest to CI, then go back to product work.**
`ci.yml` runs `pnpm build` for the web-ui but never `pnpm test`, so the vitest
suite is outside CI. That is a small, well-understood gap — close it and stop
there. The infrastructure work above had a real payoff, but it has hit
diminishing returns.

**Deliberately NOT recommended yet: the smoke scripts.** `scripts/smoke-*.sh`
(wasm sandbox, trust posture, durability) are real end-to-end proofs against a
live daemon, and none of them run in CI. They stay manual on purpose: they bind
ports, spawn daemons and need cleanup, so they would make the pipeline flaky in
exchange for a regression class nothing has actually hit yet. Wire them in when
one of them catches something by hand — not before.

**Product backlog, from `docs/ROADMAP.md` "Deferred / backlog".** Plan
distillation (the second self-learning slice — a learned plan re-validated like
any plan, reversible per ADR 0016) · WASM for the agent / policy capabilities
plus the component model and a real network host-fn · extension SDK + example
gallery · packaging: `.dmg`, Developer ID signing, notarization (human/hardware
blocked) · security: mTLS, OAuth/OIDC, OS-level subprocess containment.

**Two things that need Eryk, not an agent.**
1. **17 ADRs still carry `human_validated: false`**, including 0007–0023 — most
   of the core. Signing one honestly (ADR 0031) took a real verification pass;
   this is not a batch job.
2. **The `git push` rule below is stale.** It says pushes are the maintainer's
   act, but on 2026-08-03 Eryk explicitly directed ~8 pushes to `main` during
   the CI repair. Either the rule needs amending or that was a one-off — a
   fresh session should ASK rather than assume, and must not push on its own
   initiative either way.

**One measurement is pending and free.** The Docker build was split with
`cargo-chef` so dependencies are their own cached layer; locally a one-line
source edit rebuilt in 51s against a 254s cold cook. The next CI push that
touches `crates/` WITHOUT touching `Cargo.lock` is the first real test of
whether the GHA layer cache actually hits. Worth a glance at the `container`
job's duration whenever that happens.

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
