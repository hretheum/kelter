---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5-5
  session_date: 2026-10-29
  human_validated: false
  session_context: "Launch channels (launch refresh). Entry conditions were checked on the web on 2026-10-29; Reddit pages could not be fetched directly, so Reddit rules come from search results and mirrors and MUST be re-read on reddit.com before posting. Nothing here is posted by an agent."
---

# Launch channels — conditions and ready text

Repo: <https://github.com/hretheum/kelter> (public, 0 stars on 2026-10-29).
Posting and submitting are the maintainer's acts. Companion drafts:
[show-hn.md](show-hn.md), [product-hunt.md](product-hunt.md).

| Channel | Entry condition (checked 2026-10-29) | Status for Kelter |
|---|---|---|
| r/rust | No showcase rule found in the wiki beyond the commercial-promotion bar ([rules](https://www.reddit.com/r/rust/wiki/rules/), read through a mirror); weekly "what are you working on" threads are the usual place for small projects. Open-source code under an OSI license clears the legitimacy bar. | Allowed; re-read rules first |
| r/LocalLLaMA | Self-promotion should stay at about 10% of your activity, affiliation must be disclosed, no engagement farming ([wiki](https://www.reddit.com/r/LocalLLaMA/wiki/index/), via a mirror). | Allowed if the post is about local models; lead with the 0.5B proof |
| r/selfhosted | Projects younger than 3 months go only in the weekly "New Project" megathread (Fridays); its template asks for AI involvement ([rules update thread](https://www.reddit.com/r/selfhosted/comments/1rmt39o/rules_update_new_project_friday_here_to_stay/)). Sources conflict on how strictly AI involvement is policed. | Kelter has been public since 2026-10-14 (under 3 months): use the Friday megathread, disclose AI involvement |
| awesome-rust | At least 50 GitHub stars or 2,000 crates.io downloads (or an equivalent metric stated in the PR); entry format `[ACCOUNT/REPO](url) [[CRATE](crates.io url)] - DESCRIPTION`; alphabetical; optional CI badge ([CONTRIBUTING](https://github.com/rust-unofficial/awesome-rust/blob/main/CONTRIBUTING.md)). | **Not yet eligible**: 0 stars, crate not published. Revisit at 50 stars. |
| awesome-mcp-servers | The entry must be a server users install and run, in a public repo; alphabetical within a category; one line per server ([CONTRIBUTING](https://github.com/punkpeye/awesome-mcp-servers/blob/main/CONTRIBUTING.md)). | **Does not fit today**: Kelter is an MCP client, not an MCP server. Roadmap item KR4.1 (Kelter as an MCP server) would change that. Do not submit before it exists. |

Also noted: r/opensource moderators banned AI-assisted projects on 2026-10-24
([report](https://winbuzzer.com/2026/10/27/reddits-open-source-forum-bans-ai-assisted-projects-xcxwbn/)),
so do not post there. Everywhere, state the AI involvement plainly; the README
already does.

## r/rust — title and text

Flair: whatever the sub currently offers for projects ("🛠️ project" at the
time of writing; check).

**Title:** `Kelter: an agent runtime in Rust where a deterministic kernel validates, gates and journals what the model proposes`

```
I've been building Kelter, an agent runtime (daemon + HTTP/WS API + embedded
React UI) in Rust, 12 crates, Apache-2.0/MIT.

The design constraint: the model is a policy, not the runtime. It proposes
one action from a JSON Schema; the kernel validates it and only then runs
it. ValidatedAction has no public constructor other than
ActionSchema::validate and derives Serialize but not Deserialize, so it
cannot be forged (kernel-core, ADR 0001).

Rust-specific things people may find interesting:
- crash-resume from a SQLite journal with exactly-once side effects via
  idempotency keys (ADR 0007); a crash-injection hook
  (KELTER_CRASH_POINT) drives the smoke tests
- capability-by-absence WASM sandbox for extensions (wasmhost crate,
  ADR 0011/0034)
- human approval gate and cost budgets as kernel state, not middleware
  (ADR 0038/0039)

Status: one maintainer, most of the code written by AI agents under my
direction (architecture and decisions are mine, ADRs mark which). ~430
hermetic tests plus Postgres integration tests in CI.

https://github.com/hretheum/kelter

Feedback on the ADRs and the crate boundaries welcome.
```

## r/LocalLLaMA — title and text

**Title:** `Same agent task on Qwen2.5-0.5B and a frontier model, identical step sequence: a runtime where the kernel, not the model, owns control flow`

```
Kelter is an open-source (Apache-2.0/MIT) agent runtime. With a local
llama.cpp model the action space is compiled to a GBNF grammar, so the
decoder cannot emit a malformed tool call; the kernel re-validates every
proposal anyway and rejects exact repeats so a small model cannot loop.

scripts/demo-side-by-side.sh runs one multi-step task on
Qwen2.5-0.5B-Instruct (q4_k_m) and on a frontier model via OpenRouter. The
configs differ by one line:

  -engine = "local:qwen2.5-0.5b-instruct"
  +engine = "openrouter:anthropic/claude-sonnet-5"

The 0.5B model decides worse; what it cannot do is break the workflow.
Fully local mode makes no external calls. The same kernel also adds a human
approval gate for mutating tools, cost budgets and crash-resume (details in
the repo). Disclosure: I'm the author; most of the code was written by AI
agents under my direction.

https://github.com/hretheum/kelter
```

## r/selfhosted — megathread comment

Post in the current "New Project" megathread on a Friday, using its template.

```
Name: Kelter
Link: https://github.com/hretheum/kelter
What it is: an agent runtime you run yourself: one daemon (container /
Podman Quadlet / binary), SQLite by default (Postgres optional), web UI
embedded, bearer-token auth with principals. Works with a local llama.cpp or
Ollama model, or your own OpenRouter key.
Why you may care: mutating tools can require your approval before they run
(pause survives restarts), runs have cost/time budgets, and a killed run
resumes without repeating side effects. Deployment files: deploy/ (compose.yaml,
kelter.container).
License: Apache-2.0 / MIT
AI involvement: most code and docs written by AI agents under my direction;
architecture and decisions are mine. Documents an agent wrote say so in front
matter. Receipts: docs/BUILD-EVIDENCE.md
Maturity: one maintainer, runs on my home server with a 26-scenario
acceptance suite; young project.
```

## awesome-rust — entry (use only when eligible)

Section: **Applications → Productivity** (where agent tools such as `aichat`
sit), alphabetical.

```
* [hretheum/kelter](https://github.com/hretheum/kelter) - Agent runtime where a deterministic kernel validates, gates and journals what the model proposes [![build badge](https://github.com/hretheum/kelter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/hretheum/kelter/actions)
```

If the crate is published, add `[[kelter](https://crates.io/crates/kelter)]`
after the repo link (see the metadata plan in the publishable-metadata PR).
The badge points at `.github/workflows/ci.yml`. PR title: `Add hretheum/kelter`.

## awesome-mcp-servers — hold

No entry text on purpose. When KR4.1 ships, category candidates are
"Coding Agents" or "Developer Tools"; the legend would be 🦀 (Rust),
🏠 (local service), 🍎 🐧 (OS). Re-read CONTRIBUTING at that time, and note
the repo also asks automated agents to follow a specific PR title rule.
