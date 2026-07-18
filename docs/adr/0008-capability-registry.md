---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 4a: unified capability registry generalizing the tool-only extension slice; memory as the second registered capability"
---

# ADR 0008: The capability registry — one manifest, many kernel seams

Status: accepted · Date: 2026-07-18 · Realizes: ADR 0006 · Extends: Phase 2 tools slice

## Context

Phase 2 shipped extensions for exactly one axis: tools. ADR 0006 commits to
the same shape for every axis (memory, agents, model providers) with
governance that extensions cannot bypass. This ADR generalizes the loader
into a capability registry and proves it on the second axis — memory.

A key protocol decision was open: does a memory extension speak MCP, or a
dedicated memory protocol?

## Decision

**One manifest, routed by capability.** `kelter-extension.toml` declares
exactly one capability (`provides = ["tools"] | ["memory"]`; `agent` and
`model-provider` are reserved words that parse but **refuse to load** until
implemented — forward-compatible manifests, never silently ignored),
requested permissions (`[permissions]`, unknown keys are a manifest error),
and the server command. The registry routes each accepted extension to its
kernel seam:

- `tools` → the action space (`ActionSource`); governed by kernel-side
  action validation + the run journal (unchanged from Phase 2).
- `memory` → the `MemoryProvider` seam; **never** exposed in the model's
  action space.

**Memory reuses MCP** — no bespoke protocol. A memory extension is an MCP
server exposing three reserved tools mapping 1:1 onto the provider trait:
`memory_write(episode)`, `memory_retrieve(task, limit) → {results:
[RankedContext]}`, `memory_upsert(item)`. Rationale: MCP already gives
typed schemas, structured results, stdio transport, and SDKs in every
language — the contract fits without loss, so a dedicated protocol would
be cost without expressiveness. The registry (not the protocol) is what
keeps memory tools out of the policy's reach.

**Governance is seam-specific but always kernel-side.** For memory, the
kernel-side client validates every `memory_retrieve` response against the
interface schema (shape + score ∈ [0,1]) and rejects violations; episodes
are constructed and provenance-tagged by the kernel before they ever reach
the backend. An extension can store badly; it cannot feed the kernel a
malformed context or forge attribution. (A backend that fails or lies
degrades context quality, never run integrity — retrieval errors are
logged and treated as "no context".)

**Permission scoping in this slice:** `workspace_write` gates whether
`KELTER_WORKSPACE` is handed to the server; `network` is recorded and
logged (transport-level enforcement is a staged follow-up — declared
honestly, not simulated).

**Dogfooding (the ADR 0006 rule, now enforced in code):** the built-in
durable memory is the Phase 3 SQLite provider served by the bundled
`memory-server` binary over the same reserved tools, spawned by the daemon
through the same registry path as any third-party backend. There is no
privileged in-process memory in the daemon. An external memory extension
overrides the built-in one; more than one is a startup error.

## Consequences

- "Extend without Rust" now covers two axes: a tool in ~25 lines of Python
  (Phase 2) and a memory backend in ~30 lines
  (`examples/extensions/memstore/`), both manifest-registered, both used by
  real runs with zero recompilation.
- BYO knowledge-graph memory is now a userland exercise: implement three
  MCP tools in any language (the `upsert` leg is the graph hook).
- The registry is the seam 4b (agents) and 4c plug into: their capability
  words are already reserved and refuse to load until real.
- Cost accepted: memory calls now cross a process boundary (stdio MCP).
  At Kelter's call rates (one retrieve per run start, one write per
  reflection) this is noise; a hot-path memory would revisit with the WASM
  mechanism from ADR 0006.
- The example memory backend lives under `examples/extensions/` — NOT the
  auto-scanned `extensions/` — because registering a memory extension
  *replaces* the built-in durable backend; replacing storage should be an
  explicit act, while tools are additive and safe to auto-load.
