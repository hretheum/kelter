# ADR 0006: Governed microkernel and the extension model

Status: accepted (direction; not implemented in Phase 1) · Date: 2026-07-18

## Context

Kelter is positioned as a runtime, not a library. Against a library (you
`impl` a trait and compile it into your binary), a runtime's promise is:
extend it **without recompiling or forking**. MCP already establishes that
pattern for one axis — a tool is an out-of-process server, discovered at
runtime. The question is whether the other axes (memory, learning, skills,
agents) follow the same shape. They should.

Two of the project's threads converge here on a single structure: a
**microkernel**. The "deterministic kernel" thesis and the "runtime"
positioning meet where a small, trusted kernel holds only control-flow,
validation, authorization, journaling and an extension registry — and every
concrete capability lives in userland.

But open extensibility reintroduces exactly the distrust the kernel exists
to remove. A probabilistic component — or now a third-party extension —
must not be trusted to enforce its own constraints. So the decision is not
"add a plugin system"; it is "add a **governed** extension model".

## Decision

**Kelter is a governed microkernel.** `kernel-core` owns only: run flow,
action validation, authorization/allowlisting, the journal, and an extension
registry. It owns no concrete capability.

**Extensions are untrusted by construction.** Kernel validation,
authorization and journaling are non-bypassable. The Phase 1 invariant "the
kernel trusts no adapter" (`ValidatedAction` is constructible only via
`ActionSchema::validate`) generalizes to **"the kernel trusts no
extension"**: an extension may *propose* an action, *provide* a tool, or
*offer* memory — the kernel always validates it against a schema, enforces
the allowlist, and journals it. You can add a module; you cannot add a
module that bypasses the gate.

**Three extension mechanisms, chosen per axis** (not one plugin system):

1. **Protocol / process (MCP-style)** — maximum isolation, language-agnostic,
   zero recompile; cost is IPC + protocol versioning. For tools (shipped),
   memory backends, agents, and external model providers.
2. **WASM sandbox (in-process)** — the middle: fast, isolated, no recompile,
   still multi-language. For the hot path: validators, latitude policies,
   learning distillers. The distinctive but costlier bet (component-model
   maturity, debugging) — adopt deliberately, not by default.
3. **Compiled Rust trait (in-tree)** — trusted core only: the kernel and the
   constrained-decoding adapter. The "who builds Kelter" path, not the "who
   extends it" path.

Model adapters stay **compiled** on purpose: they are performance-critical,
hold API keys, and implement constrained decoding, and OpenRouter already
covers the "new models" demand via one config line. Not every axis must be
runtime-pluggable, and pretending it must is over-engineering.

**Extensions declare a manifest** — capabilities provided, permissions and
tools required, and the schemas their actions are subject to. The kernel
reads the manifest, scopes the extension, and enforces it.

**The public product surface is versioned extension protocols + the client
API contract**, not Rust types. The Rust internals may change freely; the
protocols are semver-stable, and a "breaking change" now means a protocol
change, not a type change.

**Dogfooding is a rule.** Kelter's own batteries (reference MCP tools, the
in-memory store; adapters excepted per above) are built through the same
public extension API, never privileged internals — which proves the
protocols are sufficient and keeps the kernel genuinely small.

## Consequences

- **Open like a runtime, governed like a kernel.** Most extensible agent
  platforms are open-but-ungoverned; the governed version is a
  differentiator, not hygiene, and it preserves the reliability/audit pitch.
- The moat becomes an **extension ecosystem on stable protocols** — the
  VS Code shape, where the value is the extension API + marketplace, not the
  host's implementation language. This is the foundation for a skills
  marketplace.
- Roadmap priority shifts toward **hardening and documenting the extension
  seams** over adding built-in features.
- Costs adopted with eyes open: WASM component-model maturity, protocol
  versioning overhead, IPC latency. Hence per-axis choice, not one uniform
  mechanism.

## Status of implementation

Phase 1 shipped the seams as compiled traits (`MemoryProvider`,
`PolicyOracle`, `RunStore`) plus MCP for tools. Staged path: formalize a
uniform capability registry in the kernel → memory as the first
runtime-registered backend (process or WASM) → WASM as the second mechanism
→ skills and agents as manifest-declared, catalog-watched artifacts →
extension SDK + a non-Rust example. The full extension platform is a
post-launch (Phase 3) track; the launch may demonstrate a single "extend
without Rust" example as proof.
