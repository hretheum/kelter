---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5-5
  session_date: 2026-10-29
  human_validated: false
  session_context: "Proposal for GitHub repository metadata. Nothing was applied: the settings are the maintainer's act (CLAUDE.md human gate)."
---

# Repository metadata — proposal (not applied)

State on 2026-10-29 (`gh repo view`): description empty, no topics, no
homepage, 0 stars, license detected as Apache-2.0 (the repo is dual
MIT OR Apache-2.0; GitHub shows the first it detects).

## Description (≤ 350 characters)

```
Agent runtime in Rust: a deterministic kernel validates every model proposal, asks a human before mutating actions, enforces cost budgets, checks a success contract, and journals everything so killed runs resume exactly once. Works with a 0.5B local model or a frontier one.
```

(about 270 characters; every clause maps to ADR 0001, 0038, 0039, 0037, 0007
and the side-by-side demo.)

Shorter alternative (≈ 120): `Agent runtime in Rust: the model proposes, a deterministic kernel validates, gates, budgets and journals.`

## Topics

`ai-agents`, `agent-runtime`, `llm`, `rust`, `mcp`, `human-in-the-loop`,
`governance`, `durable-execution`, `local-llm`, `llama-cpp`, `openrouter`,
`wasm`, `self-hosted`

(GitHub allows 20. Not proposed: `claude-code` — the integration exists in
`deploy/claude-code/` but it is not the project's identity; add if wanted.)

## Homepage

Leave empty until a project page exists. Do not point it at a personal
address by default; `https://github.com/hretheum/kelter#readme` is the
fallback if a value is required.

## Social preview (1280×640, PNG)

Same visual language as `docs/launch/hero.svg`: dark `#101418`, ring,
two engine glyphs, one config line. Text on the card:

- Headline: **Kelter**
- Subline: **The model proposes. The kernel decides.**
- Three short chips: `human gate` · `cost budgets` · `resume after kill`
- Bottom line (monospace): `engine = "local:qwen2.5-0.5b-instruct"`

Keep important content inside the central 1000×500 area (GitHub crops
edges on some surfaces). The SVG can be exported at 1280×640 by changing its
`viewBox`/canvas, or recomposed from the same elements.

## Commands the maintainer would run (do not run by agent)

```sh
gh repo edit hretheum/kelter \
  --description "Agent runtime in Rust: a deterministic kernel validates every model proposal, asks a human before mutating actions, enforces cost budgets, checks a success contract, and journals everything so killed runs resume exactly once. Works with a 0.5B local model or a frontier one." \
  --add-topic ai-agents --add-topic agent-runtime --add-topic llm --add-topic rust \
  --add-topic mcp --add-topic human-in-the-loop --add-topic governance \
  --add-topic durable-execution --add-topic local-llm --add-topic llama-cpp \
  --add-topic openrouter --add-topic wasm --add-topic self-hosted
```

The social preview image can only be uploaded in the web UI
(Settings → General → Social preview).

## crates.io

The name `kelter` is reserved with a 0.0.1 placeholder (2026-07-18). See the
publishable-metadata PR for what a real publication would need.
