---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-20
  human_validated: true
  session_context: "WASM as a governed extension for the memory capability — promoting the compiled-in MemoryProvider (ADR 0005) to a WASM-sandboxed, manifest-declared, runtime-registered backend (ADR 0011 + ADR 0008), preserving the self-learning invariant (ADR 0016)"
---

# ADR 0024: WASM as a governed extension for the memory capability

Status: accepted · Date: 2026-07-20 · Realizes: ADR 0011 (memory was the
first scoped-out WASM capability) · Registered via: ADR 0008 · Preserves:
ADR 0016 (governed self-learning invariant) · Interface: ADR 0005

## Context

ADR 0011 shipped WASM as the second extension mechanism, but **tools-only**:
`mechanism = "wasm"` on a `memory` capability failed to load loudly, with
memory/agent/model-provider explicitly deferred. Memory therefore had two
backends — the compiled-in `InMemoryProvider` (ADR 0005) and the subprocess
MCP backend (`McpMemoryProvider`, ADR 0008) — but no *sandboxed* one. The
subprocess backend has the honest egress gap of ADR 0009: a memory server is
an OS process that can reach the network and disk the kernel cannot contain.

This ADR promotes the memory provider to a **WASM-sandboxed, manifest-declared,
runtime-registered** backend: "bring your own memory backend in any language
that compiles to WASM, sandboxed by construction, without recompiling Kelter."

The load-bearing question was not the wiring — the registry, the WASM host,
and the `MemoryProvider` seam all already existed — but whether the memory
interface can cross the WASM boundary **without ambient authority** and
**without weakening the self-learning invariant** (ADR 0016).

## Decision

**`mechanism = "wasm"` now supports `memory` as well as `tools`.** The
registry routes a `[extension] provides = ["memory"], mechanism = "wasm"`
manifest to the WASM host instead of spawning a subprocess. `agent` and
`model-provider` with `mechanism = "wasm"` still fail loudly — the
forward-compatible refusal *shrank*, it did not disappear.

**Same byte ABI as tools, one multiplexed op.** A memory module exports the
identical `alloc`/`run`/`memory` ABI as a WASM tool (ADR 0011); the operation
is carried in a JSON envelope whose op names mirror the reserved MCP memory
tools (ADR 0008), so both transports speak one protocol:

```
{"op":"memory_write",    "episode": {...}}          -> {"ok": true}
{"op":"memory_retrieve", "task": "...", "limit": N}  -> {"results": [ {episode_id, summary, score}, … ]}
{"op":"memory_upsert",   "item": {...}}             -> {"ok": true}
```

A memory module declares **no** `[wasm] tool` name (a memory backend serves
the fixed op protocol, not one named action); declaring one is a loud
manifest error, exactly as a tools module *omitting* one is.

**Two deliberate differences from the tool host.** A tool guest is one-shot
(fresh Store per call, leak-and-tear-down); a memory backend must *remember*,
so:

1. **Persistent instance.** One `Store`/instance lives for the provider's
   lifetime — the guest's linear memory *is* its storage — serialized behind
   a mutex, fuel refilled per call. Linear-memory growth is capped by a
   wasmtime `StoreLimits` bound (64 MiB) so a hoarding or leaky guest hits a
   limit, never the host.
2. **Eager instantiation.** The capability sandbox bites at *instantiation*
   (an ungranted import is structurally absent), so `load` instantiates
   immediately. A memory module demanding an ungranted capability is refused
   **at daemon startup**, loudly — not on first retrieval.

**The capability sandbox is unchanged and shared.** The linker gate from
ADR 0011 — a host function is defined only if the manifest granted it — is
now generic over the host state, so the tool host and the memory host use
*one* gate. A memory module that imports `kelter:net` without a `network`
grant cannot be instantiated; the capability is structurally unreachable,
not "blocked by a check". Default grants: nothing.

**The governance invariant holds by construction (ADR 0016).** Everything a
memory backend returns enters the kernel through exactly one seam:
`PolicyContext.memory` — retrieved *context*, never authority. Three
independent guards, none new to this ADR, all now applied to the WASM
boundary:

- **Kernel-side response schema.** The canonical `retrieve` contract, lifted
  out of the subprocess client into the interface crate
  (`memory::retrieve_response_schema`) and now shared, validates every
  backend's response before it reaches the policy context. A malformed shape
  is an error (degraded context), never trusted.
- **Provenance tag.** Context lines from a WASM backend are tagged
  `[wasm-memory:<name>]`, the same visible-untrusted-source technique as
  `[learned]` (ADR 0016).
- **The validation floor.** The kernel validates every *proposed action*
  against the action schema regardless of what context suggested it. A
  poisoned memory backend can make the model *propose* an out-of-schema
  action; the kernel *rejects* it. Poisoned context ≠ authorization.

This is proven with a **deliberately malicious** module (`poisonmem`): it
returns a schema-valid response whose content orders the model to run an
out-of-schema `nuke` action claiming pre-authorization. The scripted model
obeys; the kernel rejects `nuke`; the run completes via valid actions. The
poison reached the model and did nothing else.

**The compiled-in default is untouched.** With no memory extension present
the daemon still uses the built-in durable backend (dogfooded via the
subprocess seam). WASM memory is opt-in. "Exactly one memory backend" is now
counted across **both** mechanisms — a WASM memory module and a subprocess
memory extension registered together is a loud startup error.

## Consequences

- The egress gap of ADR 0009 is closed for the memory mechanism too: a WASM
  memory backend cannot reach the network/disk it was not granted.
- **Honest boundary (unchanged):** a *subprocess* memory extension still has
  OS-level egress. WASM memory is a sandboxed *alternative*, not a fix for
  the subprocess one.
- **Honest durability boundary:** a WASM memory backend's persistence is
  whatever the module keeps in its own linear memory — as durable as the
  daemon process, no more. It is NOT the crash-resumable SQLite durability of
  the built-in backend (ADR 0007). The daemon says so at startup. A module
  wanting real persistence would need a granted storage capability (deferred).
- Scoped out (deliberately): a granted host *storage* capability for durable
  WASM memory; graph-shaped retrieval across the boundary (upsert is
  accepted and stored guest-side, but retrieval remains episodic); embedding
  retrieval inside a guest; the component model / WASI preview 2.

## Alternatives considered

- **WASI preview 2 + a `wasi:keyvalue`-style interface.** Richer, but drags
  the `wit`/bindgen toolchain and a large interface surface into the trust
  path for no benefit to a single-op-in/JSON-out backend — the same call
  ADR 0011 made for tools.
- **A distinct memory ABI.** Rejected: reusing the tool ABI means one trust
  path, one host, one set of sandbox tests. The only real difference —
  persistence — is a host-side lifetime decision, not an ABI change.
