<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-18 | human_validated: false | context: roadmap + Phase 4 suggestions -->

# Kelter roadmap

Kelter is a governed-microkernel runtime for AI agent orchestration: a
deterministic kernel validates every action a model proposes, the model is a
swappable policy engine, and it stays reliable even on small local models.

## Where we are

- **Phase 1 — walking skeleton.** Deterministic kernel, dual policy adapters
  (local GBNF + OpenRouter), MCP tool substrate, one-line model swap, PWA +
  desktop topologies. Done.
- **Phase 2 — launch-ready.** 5-minute onboarding, side-by-side demo, the
  first "extend without Rust" slice (a Python MCP tool registered via a
  manifest), launch assets. Done.
- **Phase 3 — durable, operable runtime.** Persistent run store + memory,
  crash-resume from the journal, idempotent tool boundaries (a side effect
  happens exactly once across a resume), real retrieval, OpenTelemetry.
  In progress.

## Phase 4 — the governed extension platform (planned)

Realizes ADR-0006 in full. The moat is an extension ecosystem on stable
protocols — the VS Code shape, where the value is the extension API and
marketplace, not the host's implementation language. Phase 2 shipped only the
first slice (tools).

- **Uniform capability registry in the kernel.** Generalize manifest-declared
  extensions from tools to every axis: memory backends, agents, and external
  model providers. Each declares its capabilities, required permissions, and
  the schemas its actions are subject to; the kernel scopes and enforces.
  Extensions stay untrusted by construction — validation, authorization and
  the journal remain non-bypassable.
- **Memory as a runtime-registered extension.** Promote Phase 3's persistent
  memory *provider* (a compiled trait) to a manifest-declared, runtime-
  registered backend — including a bring-your-own knowledge-graph provider,
  with no recompilation.
- **Optional embeddings retrieval (a config switch).** A lever that swaps
  Phase 3's dependency-light lexical scorer for an embeddings-backed one
  through the retrieval seam — embeddings stay opt-in (e.g. a local embedding
  model), so the default stays single-binary and dependency-light.
- **WASM as the second extension mechanism.** An in-process sandbox for the
  hot path: validators, latitude policies, learning distillers. The
  distinctive but costlier bet (component-model maturity, debugging); adopt
  deliberately, not by default.
- **Agents as governed extensions.** Manifest-declared agents turn multi-agent
  workflows and delegation into governed extensions instead of bespoke code.
- **Extension SDK + example gallery.** Scaffolding plus a gallery of working
  non-Rust examples (Python / TypeScript) across each capability — the
  foundation for a skills marketplace.

Building on Phase 3's durable execution, the runtime also gains a governed
multi-agent and planning layer:

- **A2A as the governed inter-agent wire.** Speak the Agent2Agent protocol
  both ways — expose Kelter's agents via Agent Cards (discoverable and
  delegatable) and consume remote A2A agents as a validated action space.
  Every inter-agent task, inbound or outbound, passes kernel-side validation,
  authorization and journaling, with per-agent identity and cost attribution —
  closing the governance gap that A2A and bolt-on gateways leave to
  implementers. A2A tasks are long-running and stateful, so they inherit
  Phase 3's journal, resume and idempotent invocation: a delegated task
  survives a crash and resumes without double-delegating. Wired as a governed
  extension (ADR-0006) on the official A2A SDK; an ACP-native adapter can
  follow.
- **Validated, durable plans (plan mode).** The model proposes a plan — a DAG
  of steps with dependencies — and the kernel validates it (schema, allowed
  actions, no cycles) before executing it durably with resume and idempotency.
  Adaptive re-planning on a failed step is journaled. A plan node can be a tool
  call, an A2A sub-agent delegation, or a review step, so planning composes the
  rest of the layer. Schema-constrained plans stay reliable even on a small
  local model.
- **ReAct reasoning capture.** Record the model's reasoning as an optional
  per-step field, but keep the validated action and result in the journal as
  the source of truth — a stronger transparency story than a raw reasoning
  trace, which a model can confabulate.

Model adapters stay compiled on purpose: they are performance-critical, hold
API keys, and implement constrained decoding, and OpenRouter already covers the
"new models" demand through configuration. Not every axis needs to be
runtime-pluggable.

## After Phase 4 — candidate directions

Once Phase 4 lands — the governed extension platform plus the governed durable
multi-agent and planning layer — the next moves are a menu, not a fixed
sequence: each is large enough to be its own phase when chosen, and not all
will be built. In rough priority.

- **A · WASM as the second extension mechanism.** In-process,
  capability-sandboxed extensions for the hot path (validators, latitude
  policies, learning distillers) — and, structurally, the close of the one
  honest gap Phase 4b left open: a spawned extension's arbitrary network
  egress. A WASM module can do nothing it is not explicitly granted, so the
  kernel governs every capability instead of trusting a subprocess. This is
  also a developer-facing story worth documenting as its own selling point:
  bring your own logic in any language, sandboxed by construction, and the
  kernel decides what it may touch.
- **B · A2A inbound and interop.** Kelter as an A2A server — exposing its
  agents (and plans) via Agent Cards so other systems can delegate to Kelter
  and inherit its governance and durability. Completes the A2A story, out and
  in; more completeness than new wedge.
- **C · Hardening and real deployment.** Close the accumulated honest deferrals
  — desktop on the durable store, cost in dollars, OpenTelemetry metrics and
  logs, the embeddings retrieval switch, ledger-dedupe scaling, auth — plus
  packaging (above) and deployment on real rootless-podman hardware. Necessary
  for real users; a sweep, not a headline.
- **D · Governed self-learning.** The runtime distils reusable knowledge and
  plans from past runs into memory — governed, provenance-tagged, and
  reversible, so learning can never silently corrupt behaviour. On-brand and
  ambitious; also the highest-risk.

## Packaging & distribution

Release hardening, not a capability phase. The Tauri desktop shell currently
bundles a bare `.app`; shipping it to users means adding a `.dmg` target,
Developer ID signing and notarization (to clear Gatekeeper), and a universal
(arm64 + x86_64) build. Windows and Linux bundle targets follow the same
pattern. The container / daemon path already ships via the Quadlet unit and
Dockerfile.

## Beyond

Multi-master offline / online sync (CRDTs) stays explicitly out of scope until
there is a real single-kernel product worth extending from.
