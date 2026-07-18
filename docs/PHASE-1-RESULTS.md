---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-18
  human_validated: false
  session_context: "Independent validation of the Phase 1 walking skeleton (placeholder name: agentkernel; product name: Kelter) against the Phase 1 prompt's definition of done"
---

# Phase 1 — results & independent validation

The Phase 1 walking skeleton was produced on 2026-07-18. This document records
the outcome and an **independent** validation against the prompt's definition
of done — i.e. gates re-run directly on this machine, not taken on trust from
`BUILD-EVIDENCE.md`. Reviewer / human gate: Eryk.

## What shipped

- Rust workspace, 6 crates: `kernel-core` (deterministic kernel; a
  `ValidatedAction` is constructible only through `ActionSchema::validate`,
  encoding "the kernel trusts no adapter" in the type system), `policy` (two
  working adapters — `local` via llama.cpp + GBNF, `openrouter` via the unified
  API with the key read from env only — plus the latitude knob), `mcp` (tools
  as MCP servers, in-process + stdio transports), `memory` (`MemoryProvider`
  trait + in-memory impl), `api` (HTTP + WS, token auth), `daemon` (headless bin).
- Web UI (installable PWA) + Tauri desktop shell with both topologies
  (standalone embeds the daemon in-process; remote points at a URL).
- Demo wedge: read → extract structure → markdown report over 3 MCP tools, run
  identically by both adapters; adapter choice is one config line.
- OSS scaffolding: dual MIT/Apache-2.0, CONTRIBUTING/CODE_OF_CONDUCT/SECURITY,
  `architecture.md` + 5 ADRs, CI matrix, Dockerfile, Quadlet unit, compose,
  Product Hunt draft, and `BUILD-EVIDENCE.md` + raw logs under `docs/evidence/`.

## Independent validation (re-run on this machine)

| Gate | Status | How verified |
|---|---|---|
| `cargo fmt --all --check` | PASS | re-run here, exit 0 |
| `cargo build --workspace` | PASS | re-run here, exit 0 |
| `cargo test --workspace` | PASS | re-run here, exit 0, 0 failures |
| `cargo clippy … -D warnings` | PASS | re-run here, exit 0 |
| Naming: `git grep -i` old codename over tracked tree | 0 hits | re-run here |
| Internal briefs untracked / gitignored | PASS | `git ls-files` — none |
| No secrets committed | PASS | scan — only fake test keys / env passthrough / placeholders |
| No push / no remote / no public repo | PASS | `git remote -v` empty; 3 local commits on `main` |
| One-line adapter swap | PASS | `config-swap.diff` — only the `engine =` line differs |

## Accepted on raw-log evidence (not re-run here)

- **Local smoke, e2e** — a 0.5B model (Qwen2.5-0.5B) completed the 3-tool wedge
  under GBNF at temperature 0; raw run/report/event logs present. The
  constrained-decoding mechanism was empirically verified in `BUILD-EVIDENCE.md`
  §4. Not re-run here (needs a running llama-server).
- **Container image** — `docker buildx` for `linux/amd64,linux/arm64`, exit 0,
  manifest list with both platforms (`docker-buildx.log`).
- **Desktop build** — release build linking `kernel-core` + macOS `.app`
  (`desktop-build.log`).
- **README quickstart** — daemon start, `/healthz`, run submission
  (`quickstart-daemon.log`).

## Verdict

**PASS.** The definition of done is met, the walking skeleton walks (a weak
local model completed the wedge), the model-swap wedge is real (one config
line), and every guardrail held: the repo is private and unpushed, no secrets,
zero references to the old internal codename in the tracked tree. Fable stopped
correctly at the human gate. **Recommendation: approve.**

## Open items for the reviewer (none block the gate)

Verify-on-real-infra:
- **OpenRouter live leg not executed** (no key in the run). Run
  `OPENROUTER_API_KEY=… scripts/smoke-openrouter.sh docs/evidence`; re-check the
  example model id `anthropic/claude-sonnet-5` against the live models endpoint
  at run time.
- **Quadlet unit not machine-validated** (no podman on the Mac). Validate on the
  podman / K12 host before relying on it.
- **`grammar` on `/v1/chat/completions` is an undocumented llama.cpp extension** —
  works in b10050, but fragile across upgrades; documented fallback is
  `response_format.json_schema`.

Phase-1-scope deferrals (interfaces shipped, implementation later): in-memory
persistence only; naive token-overlap memory scoring; QR pairing deferred (URL +
token today); Tauri mobile unattempted; desktop expects a llama-server already
running; DMG bundling skipped headless (uses `app`).

## Naming

Placeholder `agentkernel` is used consistently across product / image / unit
names; crate names are functional. Rename to **Kelter** at Phase 2 launch
(secure the crate / npm / GitHub-org / domain first).
