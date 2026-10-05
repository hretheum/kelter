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
core: durable journal + crash-resume (ADR 0007) whose run history also
carries refusals and resumes as informational entries (ADR 0036), MCP tool
substrate + a capability registry (ADR 0002/0008), governed A2A delegation out+in with
trust/policy/ask + retry/circuit-breaker + streamed progress (ADR
0009/0013/0022/0023/0026/0027), plan mode (ADR 0010), WASM-sandboxed extensions
incl. a memory backend (ADR 0011/0024), advisory skills with model
self-activation + human-in-the-loop ask (ADR 0019/0020/0021), governed +
reversible self-learning in both slices — facts and plans (ADR 0016/0032),
retention/log hardening (ADR 0017),
security surface — bearer/principals/SSRF-pin/TLS (ADR 0018), and an optional
**shared** Postgres+pgvector memory backend so two independent kernels share
memory + learned facts, governance preserved (ADR 0028), and a
caller-supplied **success contract** the kernel checks before `completed`, with
unverified successes kept out of learning until a human confirms them (ADR
0037), an operator **allow/ask/deny gate on mutating actions** on the same
durable human pause (ADR 0038), and **cost budgets** — $, tokens, wall time
per run, $ per principal per day — enforced before a model call or action
would cross them (ADR 0039). React 19 + Tailwind web UI is embedded in the
daemon.

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
`architecture-overview.md`. Decisions → `docs/adr/` (0001–0039). The 2026-07-21
adversarial audit + its fix passes → `docs/audit/AUDIT-2026-07-21.md`.

**Testing.** `cargo test --workspace` is hermetic — 409 tests, plus 9
`pgstore` tests that are `#[ignore]`d because they need a real Postgres. To run
those too: start a `pgvector/pgvector:pg16` container, export
`KELTER_TEST_POSTGRES_URL`, and add `-- --include-ignored` (418 total — the
figure CI reports). CI does this via a `services:` block. Bare `cargo test`
reports them as `9 ignored`, never as a green pass. `cargo clippy --workspace
--all-targets -D warnings` and `cargo fmt --check` are the bar. web-ui:
`pnpm --dir apps/web-ui test` (vitest — 2 files, 22 tests) + `pnpm build`;
both run in CI since 2026-08-03.

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

## Where work stands (2026-08-04)

Chronology lives in `docs/ROADMAP.md` ("Where we are", current through Phase
23) and `docs/BUILD-EVIDENCE.md` (dated entries). This section is only what a
fresh session needs that isn't derivable from those.

**Latest: Phase 23, plan distillation (ADR 0032).** Self-learning's second
slice; ADR 0016's deferral is closed. Design spec:
`docs/superpowers/specs/2026-08-03-plan-distillation-design.md`. CI has been
green end to end since 2026-08-03, when it was repaired after 51 dead commits.

**2026-10-04: K12 acceptance fixes.** ADR 0033 — a run with no effective
action (no dispatched step that came back ok) ends in the new terminal status
`incomplete`, never `completed`; the kernel decides, in both step and plan
mode. ADR 0034 — WASM tools can get a host-validated `workspace_write`
capability, typed flat `[wasm] parameters`, and an `{"ok": false}` failure
convention; `deploy/extensions/zapisz-plik/` is the first user (a deployable
extension root, mounted on K12 via the existing config volume). ADR 0035 — a
refused extension (invalid manifest, or `subprocess-unsandboxed` without a
`[trust]` entry) is skipped with an ERROR log and listed in
`skipped_extensions` on `GET /api/v1/tools`; the daemon starts. What is
refused is unchanged.

**2026-10-05: refusals and resumes in the journal (ADR 0036, KR0.5).** Every
`RunEvent::Rejected` site (17, inventoried in the ADR) goes through
`Kernel::reject`, which also journals `kind: "rejected"` with an explicit
`subject` (`proposal` inline / this step's `decision` / `plan_node` /
`none`) — no `seq + 1` pairing. A resumed execution journals
`kind: "resumed"` (process `session`, `at_ms`, `cause`, `replayed_steps`).
Both are **informational**: `fold_step` and every replay/exactly-once scan
ignore them (`JournalEntry::is_informational`), proven by with/without
invariance tests. WS `rejected` gained only `proposal_seq`. If you add a
journal scan, keep it kind-specific so it cannot start reading them.
`scripts/smoke-journal-history.sh` checks both against the real daemon with a
scripted model stub.

**2026-10-05: success contract (ADR 0037, KR1.1).** `POST /runs` takes an
optional `success_contract` (`action_succeeded` / `result_equals` /
`result_contains` over the recorded trace — nothing richer, no model), 422 at
creation if it names an unknown action. `outcome::verdict` is the one success
predicate (ADR 0033 floor, then the contract); unmet → `incomplete` naming the
failed conditions. Every judged run carries `verification: contract|none`.
`distill` takes only `Run::learnable()` runs — contract-verified, or confirmed
via `POST/DELETE /runs/{id}/confirmation` (journaled, informational
`success_confirmation_decided`; revoke forgets what that run taught). Runs
that ended before ADR 0037 count as unverified too (Eryk, 2026-10-05) —
confirmable, not learnable until confirmed; already-learned artifacts stay.
**Consequence:** without contracts or confirmations, `distill` learns nothing
new. "Human" = holder of an API token (named in the ADR). `scripts/smoke-success-contract.sh` drives it on the real
daemon. If you add a learning path, gate it on `Run::learnable()`.

**2026-10-05: human gate + budgets (ADR 0038 KR1.2, ADR 0039 KR1.3).**
`[approval]` puts *mutating* actions (those declaring `idempotency_key` —
ADR 0031's criterion; read-only tools and `delegate_to_*` are never gated)
under `allow`/`ask`/`deny`; `review_verdict = "ask"` makes plan review nodes
human. `ask` is the fourth use of the ADR 0021 pause (`awaiting_action_approval`,
`POST /runs/{id}/action-approval`, `approval_sweep` = timeout deny, allow →
the original proposal journaled as the `Decision`); `GET /api/v1/approvals`
lists every run waiting on a human. `[budget]` + a lower-only `budget`
override on `POST /runs`: checked before every model call against
`PolicyOracle::call_bound` (rendered prompt bytes + `max_tokens` — a new
trait method; **a wrapper oracle must delegate it**, as `SwappableOracle`
and `SkillContextOracle` do) and before every action; a stop is
`rejected/budget` + `budget_stop` + `failed`. Unpriced model → dollar limits
inactive (`usd_enforced: false`). `run.usage` now counts every model call,
refused ones included. `scripts/smoke-human-gate-budgets.sh` covers ask →
restart → allow-once, deny, token and wall stops. If you add a model call,
put a `budget_check` before it; if you add a side effect, declare
`idempotency_key` or the gate cannot see it.

**Open items, in the order they'd bite.**
1. **18 ADRs carry `human_validated: false`** — 0007–0023 plus 0032. Eryk's
   job, not an agent's; signing ADR 0031 honestly took a real verification
   pass, so this is not a batch operation.
2. **The `git push` rule below is stale** — it says pushes are the maintainer's
   act, yet Eryk directed ~12 across 2026-08-03/04. It has NOT been amended, so
   it stands as written: never push on your own initiative; ask.
3. **Downgrade past ADR 0036 breaks per run** — a binary at/before `a143922`
   cannot deserialize a journal holding `rejected`/`resumed` rows (500 on
   `/events`, no resume, `distill` errors). The ADR lists the options
   (documented `DELETE` rollback vs a catch-all variant); the choice is
   Eryk's.
4. **ADR 0037 follow-ups** — reflection episodes still record an unverified
   `completed` as `Success`; Eryk wants a separate PR (an `Unverified`
   outcome that reflection/retrieval do not treat as success). The web UI
   shows/confirms verification but cannot submit a contract yet.
5. **ADR 0038/0039 open choices** — terminal state on a budget stop (`failed`
   + `budget_stop` built, vs a new status) and the daily counter (derived
   from run records, built, vs a ledger table with a migration). Eryk's call.
6. **Phase 23 loose ends** — no plan leg in `scripts/smoke-selflearn.sh`, no
   web-UI surface for learned plans, and the `adopt_learned_plan` branch in
   `execute_plan` is unreviewed (it sits inside ADR 0030 generation /
   exactly-once semantics, where a mistake is silent rather than loud).

**Operational fact worth knowing before you push.** A push touching `crates/`
costs ~14 minutes in the `container` job. The `cargo-chef` split does not help
there: that job sets `cache-to: type=gha,mode=min`, which never exports
intermediate stages, so the cook layer cannot survive between pushes. This is a
deliberate trade-off protecting `publish-image.yml`'s `mode=max` entries in the
shared 10 GB cache (see the comment at `ci.yml:95`) — a priced cost, not a bug.
Sub-30s container runs are whole-image cache hits, not the split paying off.
Measurement and the one untested alternative: BUILD-EVIDENCE, last entry.

**Smoke scripts stay out of CI, deliberately.** `scripts/smoke-*.sh` bind
ports, spawn daemons and need cleanup; wiring them in would trade pipeline
flakiness for a regression class nothing has actually hit. Wire one in when it
catches something by hand — not before.

**Product backlog** → `docs/ROADMAP.md` "Deferred / backlog": WASM for the
agent/policy capabilities plus the component model and a network host-fn ·
extension SDK + gallery · packaging (`.dmg`, signing, notarization —
human/hardware blocked) · security (mTLS, OAuth/OIDC, OS-level containment).

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
