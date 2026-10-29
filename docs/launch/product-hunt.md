---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5-5
  session_date: 2026-10-29
  human_validated: false
  session_context: "Launch refresh (roadmap KR3.1): Product Hunt draft rewritten from the 2026-07-18 version to carry the governance thesis (ADR 0036-0040) next to the weak-model thesis. Every claim below points at a file or ADR in this repo. Final voice pass, scheduling and submission are human acts at the gate."
---

# Product Hunt launch — draft (human finalizes & submits)

> Status: **draft**. The maker does the final voice pass, records the demo
> (see [demo-storyboard.md](demo-storyboard.md)), schedules and submits.
> Nothing here is auto-published. Companion drafts:
> [show-hn.md](show-hn.md), [channels.md](channels.md),
> [repo-metadata.md](repo-metadata.md).

## The two theses (what the page has to say)

1. **Survives weak models.** The kernel, not the model, owns control flow;
   every proposal is schema-validated before anything runs, so a 0.5B local
   model and a frontier model produce the same step shape.
   Source: [ADR 0001](../adr/0001-kernel-as-runtime.md),
   [ADR 0003](../adr/0003-adapter-contract-constrained-vs-native.md),
   `scripts/demo-side-by-side.sh`, `examples/config-local.toml` vs
   `examples/config-openrouter.toml`.
2. **Governance lives in the kernel.** A human gate on mutating actions,
   cost budgets, a success contract, honest "unverified" episodes, and a
   journal that makes runs resumable and auditable:

| Claim | Where it is defined | Where it is proven |
|---|---|---|
| Mutating actions (those declaring `idempotency_key`) can be `allow` / `ask` / `deny` per tool; `ask` is a durable pause that survives a daemon restart and an `allow` runs that exact proposal once | [ADR 0038](../adr/0038-human-gate-on-mutating-actions.md) | `scripts/smoke-human-gate-budgets.sh`; BUILD-EVIDENCE "2026-10-05 — human gate on mutating actions + cost budgets" |
| Budgets in USD, tokens and wall time per run, and USD per principal per day, checked before the model call or action that would cross them; a stop is `failed` + `budget_stop` | [ADR 0039](../adr/0039-cost-budgets-enforced-before-crossing.md) | same smoke (token and wall legs); same BUILD-EVIDENCE entry |
| "Completed" means the caller's success contract was met; otherwise `incomplete`, naming the unmet condition. Runs without a contract carry `verification: none` and are not learned from until a human confirms them | [ADR 0033](../adr/0033-success-requires-an-effective-action.md), [ADR 0037](../adr/0037-success-contract-and-verification.md) | `scripts/smoke-success-contract.sh`; BUILD-EVIDENCE "2026-10-05 — success contract and `verification`" |
| Unverified episodes are stored but never enter model context; confirmation writes a new version, never an overwrite | [ADR 0040](../adr/0040-unverified-reflection-episodes.md) | `scripts/smoke-unverified-episodes.sh`; BUILD-EVIDENCE "2026-10-05 — unverified reflection episodes" |
| Every refusal and every resume is a journal entry with the refused proposal attached; a run killed after a side effect resumes without asking the model again and the effect happens once | [ADR 0036](../adr/0036-refusals-and-resumes-in-the-journal.md), [ADR 0007](../adr/0007-durable-execution-journal-replay.md) | `scripts/smoke-journal-history.sh`, `scripts/smoke-durability.sh`; `docs/evidence/adr-0036/` |

Overall evidence: about 430 hermetic Rust tests plus Postgres integration
tests in CI (CLAUDE.md "Testing"), and a 26-scenario acceptance suite run
against the live home-server deployment (README "Status";
BUILD-EVIDENCE entry "2026-10-13 — KRW.1 deployed on K12, first real runs"
and the roadmap's acceptance notes). The receipts, including what went
wrong, are in [BUILD-EVIDENCE.md](../BUILD-EVIDENCE.md); an adversarial audit
and its fixes are in [AUDIT-2026-07-21.md](../audit/AUDIT-2026-07-21.md).

## Name & tagline

**Kelter** — pick one:

1. **Agent runtime where the kernel decides and a human approves.** *(working
   favorite: carries both theses)*
2. The kernel is the runtime. The model is just the policy.
3. Swap a frontier model for a 0.5B local one — same agent, one config line.
4. Ask before it costs. Prove it finished. Resume after a crash.
5. Stop letting tool-call luck run your agents.

Gallery hook (under the hero GIF): *"Same task, same kernel — 0.5B local
model on the left, frontier model on the right, one config line apart. Then
the same kernel stops a bad action, and survives a kill -9."*

## Maker comment (draft — needs Eryk's voice pass)

> Every agent framework I tried had the same failure mode: the moment the
> model fumbled a tool call, the whole workflow fell over. The model *was*
> the runtime. And when it didn't fumble, I still had no good answer to
> three questions: who approved that action, what did it cost, and did it
> actually do the thing it said it did.
>
> Kelter moves those into a kernel you can read. A deterministic Rust kernel
> owns control flow, and the model only proposes the next action from a
> schema. The kernel validates it before anything runs, so a 0.5B local model
> and a frontier model end up with the same step sequence.
>
> Three things sit on top of that:
>
> - **A human gate.** Put a mutating tool under `ask` and the run pauses
>   before the action. The pause survives a daemon restart, and an `allow`
>   executes that exact proposal once.
> - **Budgets.** Dollars, tokens or wall time per run; a run stops before the
>   call that would cross the limit, and the call is never made.
> - **A success contract.** "Done" means a check you named passed, not that
>   the model said so. Without a contract the run is marked unverified and
>   the self-learning layer ignores it.
>
> And it all lives in a journal: kill the daemon mid-run and it resumes
> without re-asking the model for decisions it already made and without
> re-firing the side effect.
>
> Honest status: young project, one maintainer, and most of the code is
> written by AI agents under my direction. The architecture and the
> decisions are mine, and the ADRs mark which ones. Brutal feedback on the
> approval model and the adapter contract very welcome.
>
> Apache-2.0 / MIT, Rust, MCP-native for tools, a web UI embedded in the
> daemon.

## First-comment FAQ (prepared answers)

- **Why not just use <framework>?** We don't position against names. The
  difference is architectural: here the model never executes anything and
  the runtime never trusts the model. Approval, budgets and "done" are
  decided by the kernel (ADR 0038, 0039, 0037), not by prompt text.
- **Isn't a constrained 0.5B model still dumb?** Yes. It makes worse
  decisions and simpler plans. What it can't do is corrupt the workflow:
  locally the action space is compiled to a GBNF grammar, remotely native
  tool calling does the same job, and the kernel re-validates either way
  (README "Why the weak-model story matters"; ADR 0003).
- **What does the human gate cover?** Mutating actions only, i.e. tools that
  declare an `idempotency_key` (ADR 0031's criterion). A mutating tool that
  declares no key bypasses the gate today; this is a known gap recorded in
  ADR 0038 and tracked as a follow-up in `docs/ROADMAP.md`/CLAUDE.md open
  items.
- **Are budgets exact?** The check is conservative: before each model call
  the kernel compares usage plus the adapter's worst case (prompt bytes +
  `max_tokens`) with the limit, so it is pessimistic rather than measured
  against a provider tokenizer (BUILD-EVIDENCE, "Not verified here" in the
  ADR 0038/0039 entry). The daily USD per principal counter is derived from
  run records and can be off under parallel runs of one principal (ADR 0039).
- **What does "unverified" mean for learning?** A run that completed without
  a met contract is stored as an unverified episode and is kept out of model
  context until a human confirms it (ADR 0037, 0040). Consequence: without
  contracts or confirmations, the learning step learns nothing new.
- **Extensions in other languages?** Anything that speaks MCP over stdio, or
  a sandboxed WASM module (ADR 0002, 0011). Example:
  `extensions/wordstats/`.
- **Production-ready?** Honest status: it runs on one home server with a
  26-scenario acceptance suite (README "Status"), and the audit's findings
  are fixed. It is a one-maintainer project; it is not a finished platform.
  Not built yet: eval/replay tooling, OIDC/mTLS, OS-level process isolation
  (README feature table, `docs/ROADMAP.md`).
- **Is the code written by AI?** Mostly, under the maintainer's direction;
  README "Status" says so, and documents an agent wrote carry `provenance`
  front matter.
- **Privacy?** Fully local mode: llama.cpp sidecar + kernel on your machine,
  no external calls. Remote mode uses your own OpenRouter key from the
  environment, never committed or logged (README Quickstart).
- **Is Kelter an MCP server?** No: it is an MCP *client* (tools are MCP
  servers). Exposing Kelter as an MCP server is planned, not built.

## Gallery shot list

Matches [demo-storyboard.md](demo-storyboard.md):

1. **Hero GIF** — proof 1: 0.5B and frontier side by side, one config line
   apart (Scene 1).
2. **Rejected → retried** — proof 2: a bad proposal refused by the kernel and
   the retry (Scene 2).
3. **Kill → resume** — proof 3: daemon killed after the side effect, resume
   from the journal, one ledger line (Scene 3).
4. **Human gate** — the `awaiting_action_approval` pause, the decision and the
   journal entry with the principal (storyboard "Bonus").
5. Architecture card (README diagram, styled).
6. Extend-in-Python card — `extensions/wordstats/` code → manifest → registered.
7. Topology card — one kernel: container / desktop / PWA.

## Hero graphic

`docs/launch/hero.svg`, 1270×760 (PH size, 16:9-ish master): dark background
`#101418`, the kernel-as-ring motif at center, two engine glyphs (chip =
local, cloud = frontier) feeding one ring, one config line beneath:
`engine = "…"`. Wordmark **Kelter**; lowercase-k optional per final brand
pass. Readable on dark and light previews (the background is part of the
image, so it does not depend on the viewer's theme).

## Launch-day checklist (human)

- [x] Name decided: **Kelter** (2026-07-18)
- [x] Repo public, launch README merged (2026-10-14)
- [ ] Handles/domain/crate/npm secured *(gate prerequisite; see
      [repo-metadata.md](repo-metadata.md) for the crates.io status)*
- [ ] Record the three proofs (storyboard variant a or b), cut hero GIF ≤ 30 s
- [ ] Decide whether the demo is recorded on Glassbox (private fork,
      license undecided) or without it
- [ ] Maker-comment voice pass; positioning sign-off
- [ ] Re-verify the quickstart on a clean machine
- [ ] Apply the repo description/topics/social preview from `repo-metadata.md`
- [ ] Schedule (Tue–Thu, 00:01 PT) and submit
