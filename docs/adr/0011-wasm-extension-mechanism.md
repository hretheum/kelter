---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 5A: WASM as the second extension mechanism — in-process, capability-sandboxed via wasmtime; core WASM + minimal byte ABI"
---

# ADR 0011: WASM as the second extension mechanism (in-process, capability-sandboxed)

Status: accepted · Date: 2026-07-18 · Realizes: ADR 0006 (WASM was the announced second mechanism) · Registered via: ADR 0008 · Closes (for WASM): ADR 0009 egress deferral

## Context

ADR 0006 named two extension mechanisms: process/protocol (MCP — shipped in
Phases 2/4a/4b) and an in-process WASM sandbox. The subprocess mechanism has
an honest gap (ADR 0009): a spawned MCP server has OS-level egress the kernel
can't contain. WASM is the answer for the class of extensions that should run
*inside* the kernel with *no* ambient authority.

Two decisions had to be made against a fast-moving ecosystem: the wasmtime
version and, more importantly, the host↔guest interface.

## Decision

**Mechanism = `wasm` in the manifest, routed by the registry.** A manifest
may declare `[extension] mechanism = "wasm"` (default `"subprocess"`). For
the `tools` capability, the registry routes such an extension to the WASM
host instead of spawning a subprocess. A `[wasm]` section gives the module
path and the tool name. WASM is a **tools-only** mechanism in this slice;
`mechanism = "wasm"` on memory/agent/model-provider fails to load loudly
(deferred, not silently ignored — the registry's forward-compatible
posture).

**Interface = core WASM + a minimal byte ABI, NOT the component model /
WASI preview 2.** The guest exports three things over its linear memory:
`alloc(len) -> ptr`, `run(ptr, len) -> i64` (the result packed as
`ptr<<32 | len`), and `memory`. The host writes the input string in, calls
`run`, reads the JSON result out. This ABI is a handful of functions,
stable across wasmtime releases, and trivial to keep in the trust path. We
keep the component model / WASI preview 2 out of the kernel not because it
is unstable — wasmtime's own stability-tiers page now positions the
component model / WASIp2 as Tier 1 (production-supported), a shift from its
earlier "still stabilizing" reputation — but because it is **heavier**: it
needs the `wit`/`wasi-bindgen` toolchain and a larger interface surface for
no benefit to a single-string-in/JSON-out compute tool. Core WASM keeps the
trust path and the build minimal (dep-light, simplicity over richness — the
same spirit as keeping the A2A SDK out in Phase 4b). A pure-compute tool
needs **no WASI at all** — it imports nothing. (The Tier-1 claim is from a
WebFetch summary of the tiers page, not a verbatim read — worth a human
double-check before adopting the component model later.)

**Capability sandbox = the absence of host imports (structural, not
runtime).** A core WASM module has no syscalls and no ambient authority — it
can only do what the host functions it *imports* let it do. Kelter's WASM
host defines a capability's host function **only** when the manifest granted
it (`[permissions]`; default: nothing — no network, no disk). A module that
imports a capability it was not granted **cannot be instantiated**: wasmtime
rejects it with an unknown-import error. The capability is therefore
*structurally unreachable*, not "blocked at runtime by a check we might get
wrong". Proven by the sandbox smoke: a module importing `kelter:net` with no
`network` grant fails at instantiation; with the grant, the same module
instantiates and runs.

**Governance unchanged.** A WASM tool is an `ActionSource` like any other:
its action goes through kernel-side schema validation and journaling, and
(being a `tool` capability in the action space) it works as a plan-mode
`tool` node for free (ADR 0010). Execution is bounded by wasmtime fuel and a
fresh Store per call (stateless, deterministic).

**Versions (verified live):** wasmtime 46.0.1, core embedding only
(`cranelift` + `runtime` features; no `gc`, no `wasi`). Demo modules compiled
from Rust to `wasm32-unknown-unknown` and checked in.

## Consequences

- The egress gap from ADR 0009 is **closed for the WASM mechanism**: a WASM
  extension cannot reach the network/disk it wasn't granted.
- **Honest boundary (unchanged):** subprocess (MCP) extensions — tool,
  memory, agent — STILL have OS-level egress. WASM is a *sandboxed
  alternative* mechanism, not a fix for the subprocess one. Choosing WASM
  buys the sandbox; choosing subprocess keeps the flexibility (any
  language/binary, full OS) and the OS-level trust boundary. This ADR does
  **not** claim WASM closes subprocess egress.
- "Extend without Rust" now has a third shape: a sandboxed module in any
  language that compiles to WASM. The trust posture is a selling point — you
  bring the logic, the kernel decides what it can touch.
- Scoped out (deliberately): WASM for memory/agent/policy; the component
  model / WASI preview 2; richer host capabilities (a real network host
  function behind the gate); the byte ABI's typing (single string in, JSON
  out — a shallow schema, also required for fast constrained decoding on
  weak models per ADR 0010).
