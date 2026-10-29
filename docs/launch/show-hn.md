---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5-5
  session_date: 2026-10-29
  human_validated: false
  session_context: "Show HN draft (launch refresh). Technical tone, no marketing; every claim points at a file or ADR. Posting is a human act."
---

# Show HN — draft (human posts)

> Status: draft. Posting, timing and replies are the maintainer's. Rules to
> re-read before posting: <https://news.ycombinator.com/showhn.html> (a Show
> HN must be something people can run or try; no sign-up wall; do not ask
> for upvotes). Post as a link to the GitHub repo, then add the first
> comment below immediately.

## Title (pick one, ≤ 80 chars)

1. `Show HN: Kelter – an agent runtime where a Rust kernel gates, budgets and journals`
2. `Show HN: Kelter – agent runtime with a human gate, cost budgets and crash-resume`
3. `Show HN: Kelter – the model proposes, a deterministic kernel decides`

URL: `https://github.com/hretheum/kelter`

## First comment

Kelter is an agent runtime you deploy (daemon + HTTP/WS API + embedded web
UI), written in Rust. The design constraint is that the model is a policy,
not the runtime: at each decision point it proposes one action from a JSON
Schema, the kernel validates it, and only then is anything executed.
`ValidatedAction` has no public constructor other than the validator
(ADR 0001).

What I think is different, and where to look:

- **Human gate on mutating actions.** A tool that declares an
  `idempotency_key` can be set to `allow` / `ask` / `deny` in config. `ask`
  pauses the run; the pause is durable (it survives a daemon restart) and an
  `allow` runs the original proposal once. Decisions are journaled with the
  principal. ADR 0038, `scripts/smoke-human-gate-budgets.sh`. Known gap: a
  side-effecting tool that declares no key is not seen by the gate (recorded
  in the ADR).
- **Budgets checked before crossing.** USD, tokens and wall time per run,
  USD per principal per day. Before each model call the kernel compares usage
  plus the adapter's worst case (rendered prompt bytes + `max_tokens`) with
  the limit and refuses the call instead of noticing afterwards. This is
  conservative, not measured against a provider tokenizer. ADR 0039.
- **"Done" is checked.** A run can carry a success contract (for example
  `action_succeeded: write_report`). If the model says it finished but the
  condition is not met, the status is `incomplete`. Runs without a contract
  are marked unverified and the learning layer does not learn from them until
  a human confirms. ADR 0033, 0037, 0040.
- **Journal and resume.** Every decision, intent and receipt is journaled in
  SQLite (Postgres optional); refusals and resumes are entries too (ADR 0036).
  Kill the process after a side effect fired and before its receipt landed:
  on restart the decision is replayed from the journal without querying the
  model, and the tool dedupes on the journaled idempotency key.
  `scripts/smoke-durability.sh`, ADR 0007.
- **Weak models are a supported case.** Locally the action space is compiled
  to a GBNF grammar (llama.cpp); remotely native tool calling is used; the
  kernel validates either way. `scripts/demo-side-by-side.sh` runs the same
  task on Qwen2.5-0.5B and on a frontier model through OpenRouter and the
  engine differs by one config line. A 0.5B model decides worse, it just
  cannot break the flow.

What it is not: it is not an MCP server (it is an MCP client; tools are MCP
servers or WASM modules), there is no eval/replay tooling yet, and auth is
bearer tokens with principals, no OIDC. One maintainer.

Disclosure: most of the code and docs were written by AI agents under my
direction. Architecture and the decisions marked as mine in the ADRs are
human; documents an agent wrote carry `provenance` front matter. The receipts,
including failures and fixes, are in `docs/BUILD-EVIDENCE.md`, and there is an
adversarial audit with fixes in `docs/audit/`.

Quickstart is `scripts/bootstrap.sh` (needs Rust and llama.cpp). I would most
like criticism of the approval model (is "mutating = declares an
idempotency key" the right criterion?) and of the adapter contract (ADR 0003).
