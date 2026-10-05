---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-18
  human_validated: false
  session_context: "Extension trust posture: the line between containment and governance, an honest-labelling guardrail against broker-as-sandbox, and a sandbox-by-default registry policy (enforcement in Phase 6)."
---

# ADR 0012: Extension trust — containment vs governance, sandbox-by-default

Status: implemented (Phase 6) · Date: 2026-07-18 · Builds on: ADR 0006 (two mechanisms), ADR 0008 (registry), ADR 0009 (subprocess egress gap), ADR 0011 (WASM containment)

## Context

Kelter has two extension mechanisms: subprocess (MCP) and in-process WASM
(ADR 0011). WASM contains untrusted code structurally — a module has no
authority it was not granted. Subprocess extensions have OS-level egress the
kernel cannot contain (the honest gap of ADR 0009). A recurring temptation is
to "wrap" a subprocess in a managed service — a broker — and call it a sandbox.
This ADR draws the line so the project never overclaims. It is the same
discipline that motivates the whole kernel: don't pretend the model is the
runtime; don't pretend a broker is containment.

## Decision

**Containment and governance are different things.**

- **Containment** is a trust boundary a hostile or buggy extension *cannot
  escape*. Kelter offers it two ways only: (1) **WASM** — structural, the
  module cannot import a capability it was not granted (ADR 0011); (2) an **OS
  mechanism** — a network-restricted namespace or container (`--network none`,
  rootless podman / user-namespaces on Linux). Only these are containment.
- **Governance** is mediating and auditing an extension's *intended* capability
  use — a capability gateway/broker that offers network or disk through a
  policy (host allowlist, journaling, provenance). It is valuable, but a
  cooperating extension uses the broker while a hostile one opens its own
  socket with a direct syscall. **A broker without an OS enforcement layer is
  not a trust boundary.**

**Honest labelling (hard rule).** A managed wrapper or broker that lacks an OS
enforcement layer MUST NOT be called a "sandbox" in code, docs, or UI. It is an
"audited capability gateway". Calling audit "containment" is exactly the
overclaim this project exists to avoid.

**Sandbox-by-default posture.** The registry's default is the safe mechanism.
For a capability that has a sandboxed path (tools → WASM), the sandboxed
mechanism is the expected one; running an extension as an **unsandboxed
subprocess** is a deliberate, attributable decision, never a silent default:

- the manifest must explicitly declare its trust mode (e.g.
  `trust = "subprocess-unsandboxed"`), **and**
- the deployment/operator must allow it — a policy the operator sets, not the
  manifest alone. The manifest is untrusted input and cannot authorise its own
  escape.
- Absent either, the registry refuses to load the unsandboxed subprocess,
  loudly (the same forward-compatible refusal used for unknown
  capabilities/mechanisms).
- The chosen trust mode is journaled and provenance-tagged per run, so "what
  ran unsandboxed" is always answerable.

A2A remote agents are a separate axis — not local processes; their boundary is
the Phase 4b origin-pin / SSRF egress, already governed.

**What this buys, and what it does not.** It makes the safe choice the default
and the unsafe choice explicit, operator-gated and auditable. It does **not**
retroactively contain subprocess extensions: a subprocess opted-in as
unsandboxed still has OS-level egress (ADR 0009). Real containment for such a
subprocess still requires an OS layer (a network-restricted container /
namespace), which remains deferred — WASM sidesteps it by not being a
subprocess at all.

## Consequences

- Secure-by-default narrative: *extensions run sandboxed unless you explicitly,
  at the operator level, trust a subprocess* — a defensible position, not a
  caveat.
- The MCP subprocess ecosystem is preserved (any language, any binary), but as
  an explicit, attributable trust decision rather than a silent default.
- A written guardrail against building a broker-as-sandbox: if an audited
  capability gateway is built later for its governance value, it must be
  labelled as such and must not claim containment.
- Enforcement — the registry policy (trust-mode declaration, operator gate,
  loud refusal, journaling) — is implemented (Phase 6).

## Enforcement (Phase 6)

The two-part opt-in is split so the manifest can never authorise its own
escape — each part lives on a different side of the trust boundary:

- **Manifest side (registry, `crates/mcp/src/extensions.rs`).** A `tools`
  subprocess must declare `[extension] trust = "subprocess-unsandboxed"`. A
  WASM tool is `wasm-sandboxed` by construction (declaring a subprocess trust
  on it is an error); a subprocess with no trust line, or one claiming
  `wasm-sandboxed`, or an unknown value, is refused loudly — sandbox-by-default.
- **Operator side (daemon, `crates/daemon/src/config.rs` + `main.rs`).** A
  declared subprocess-unsandboxed extension loads only if the operator lists it
  in `[trust] allow_unsandboxed_subprocess`. Missing consent → the daemon
  refuses the extension, naming it and the exact config key to add. (Amended
  by ADR 0035, 2026-10-04: the refusal skips that one extension — ERROR log +
  `skipped_extensions` on `GET /api/v1/tools` — instead of aborting the whole
  daemon start; what is refused is unchanged.) The
  refusal message states plainly that allowing it does **not** sandbox the
  subprocess.
- **Transparency.** The resolved per-extension trust posture is journaled once
  per run (`JournalEntry::TrustPosture`) and exposed on the run
  (`Run.trust_posture`), so "what ran unsandboxed" is durably answerable via
  API and journal. Subprocess-unsandboxed extensions also emit an honest
  startup warning (never the word "sandbox").

Evidence: `scripts/smoke-trust.sh` (legs A–D) and
`docs/evidence/BUILD-EVIDENCE.md` §6.
