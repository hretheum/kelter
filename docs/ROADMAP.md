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
  happens exactly once across a resume), real retrieval, OpenTelemetry. Done.
- **Phase 4 — governed extension platform + multi-agent & planning.** The
  capability registry with memory as a runtime-registered extension (4a, ADR
  0008), governed durable outbound A2A (4b, ADR 0009), and validated durable
  plans / plan mode (4c, ADR 0010). Done.
- **Phase 5 — WASM as the second extension mechanism.** In-process,
  capability-sandboxed extensions; structurally closes the subprocess network
  egress left open by 4b — for WASM extensions (5A, ADR 0011). Done.
- **Phase 6 — enforced trust posture (sandbox-by-default).** The registry
  loads sandboxed by default; running an unsandboxed subprocess needs an
  explicit opt-in (a manifest flag plus an operator allowance) and is
  journaled — governance and transparency, not containment (ADR 0012). Done.
- **Phase 7 — A2A inbound & interop.** Kelter as an A2A server: Agent Card,
  `message/send` → durable run, `tasks/get`, auth; proven with a Kelter↔Kelter
  loop and a generic client (ADR 0013). Done.
- **Phase 8 — hardening sweep & packaging (C).** All items shipped: desktop on
  the durable store, dollar cost per run, OTel metrics + logs, a real
  embeddings-retrieval switch (BM25 default), O(1) ledger dedupe, auth
  hardening (rate limiting), and packaging scaffolding (unsigned `.app` +
  signing hooks) (ADR 0014). Signing (Developer ID cert), the `.dmg` GUI step,
  and K12 deploy stay human/hardware-blocked. Done.
- **Phase 8.5 — config / onboarding UX.** Choose and configure the model
  (Local / API) from the UI with pre-filled presets and a real connection
  test; runtime-mutable config (hot-apply + persisted overlay, survives
  restart); API key in a 0600 gitignored secret, absent from tree, logs and the
  API (ADR 0015). Done. Follow-up (done): the runtime config controller, first
  wired only in the daemon, now also serves the **desktop standalone** topology
  (extracted to `crates/model-config`), so the model config surface works
  end-to-end there too (no more 501); the web UI degrades to read-only when a
  deployment returns 501.
- **Phase 9 — data-lifecycle & log hardening.** Store retention/pruning (the
  unbounded-SQLite gap), a deployment-level log-size cap (Quadlet), and a
  secrets-in-logs redaction pass. In progress.
- **Phase 10 — governed self-learning (D).** The runtime distils reusable
  facts from past runs into memory — provenance-tagged, completely reversible,
  and entering only as validated context, so a learned (even poisoned) fact can
  never trigger an unvalidated action (ADR 0016). Done.
- **Phase 11 — security hardening.** Bearer-gate/allowlist the config
  connection-test, WebSocket auth, per-caller principals, and opt-in TLS
  (tailnet stays the default transport). Queued.
- **Web-UI redesign.** Full IA + UX respec (`apps/web-ui/design/`, both human
  gates passed) and its implementation: the vanilla web UI replaced by a React
  19 + Tailwind v4 + shadcn-style app (Vite → RustEmbed, same single artifact
  across all topologies, ADR 0004) — run-centric master–detail with a bucketed
  live execution axis (kernel rejections visible), honest 501/read-only and
  reconnection states, Lighthouse a11y 100, 300 KB bundle — plus two additive
  bearer-gated endpoints: journal-replay history (`GET /runs/{id}/events`) and
  Stop (`POST /runs/{id}/cancel`). The 3-user "input vs output" test stays a
  human evidence task. Done.

## Phase 4 — the governed extension platform (delivered)

<!-- Shipped across 4a (registry + memory-as-extension, ADR 0008), 4b (governed
durable A2A, ADR 0009) and 4c (plan mode, ADR 0010). The embeddings retrieval
switch and the extension SDK + gallery remain open; WASM became Phase 5. The
prose below is the original vision; components not yet built are the ones not
listed as Done in "Where we are". -->

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

- **A · WASM as the second extension mechanism** — *shipped as Phase 5 (ADR 0011).* In-process,
  capability-sandboxed extensions for the hot path (validators, latitude
  policies, learning distillers) — and, structurally, the close of the one
  honest gap Phase 4b left open: a spawned extension's arbitrary network
  egress. A WASM module can do nothing it is not explicitly granted, so the
  kernel governs every capability instead of trusting a subprocess. This is
  also a developer-facing story worth documenting as its own selling point:
  bring your own logic in any language, sandboxed by construction, and the
  kernel decides what it may touch.
- **B · A2A inbound and interop** — *shipped as Phase 7 (ADR 0013).* Kelter as an A2A server — exposing its
  agents (and plans) via Agent Cards so other systems can delegate to Kelter
  and inherit its governance and durability. Completes the A2A story, out and
  in; more completeness than new wedge.
- **C · Hardening and real deployment** — *shipped as Phase 8 (ADR 0014); the store-retention / log-size / redaction slice is Phase 9 (in progress).* Close the accumulated honest deferrals
  — desktop on the durable store, cost in dollars, OpenTelemetry metrics and
  logs, the embeddings retrieval switch, ledger-dedupe scaling, auth — plus
  packaging (above) and deployment on real rootless-podman hardware. Necessary
  for real users; a sweep, not a headline.
- **D · Governed self-learning** — *shipped as Phase 10 (ADR 0016; knowledge slice; plan distillation deferred).* The runtime distils reusable knowledge and
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

## Deferred / backlog

Items intentionally left for later, triaged by who should build them. None
block a launch; the core is complete.

- **Security hardening (Phase 11, next).** Bearer-gate / host-allowlist the
  config connection-test (a low-risk unauthenticated outbound probe today),
  WebSocket-channel auth, per-caller principals with per-run attribution, and
  opt-in TLS termination (tailnet stays the default transport). mTLS, OAuth/OIDC
  and OS-level subprocess containment are deferred further.
- **Correctness-sensitive extensions.** Adaptive re-planning (a re-plan must
  stay validated + durable), plan distillation (the second self-learning slice —
  a learned plan re-validated like any plan, reversible per ADR 0016), and WASM
  for the memory / agent / policy capabilities plus the component model and a
  real network host-fn.
- **Additive / lower-stakes.** A2A SSE streaming, webhooks and per-tool skills;
  multi-profile model configs; an embeddings retrieval-quality harness (the
  adapter is done — this is the eval plus a dedicated embedding server).
- **Cosmetic.** ADR-0013 provenance (`claude-opus-4-8` → `claude-fable-5`); the
  self-learning provenance-list evidence sample.
- **Human / hardware / paid.** Real code-signing + notarization (Apple Developer
  ID cert), the `.dmg` build (needs a macOS GUI session), K12 deployment (on
  hardware arrival), and any live API / dedicated-embedding-server run.

## Beyond

Multi-master offline / online sync (CRDTs) stays explicitly out of scope until
there is a real single-kernel product worth extending from.
