<!-- provenance: ai_authored | agent: claude-opus-4-5 | session_date: 2026-08-03 | human_validated: false | context: roadmap brought current through Phase 22 (state-epoch duplicate rule, ADR 0031) at HEAD 702c7a0; Phases 16-22 (ADR 0024-0031 + the A1 coding round) written from the ADRs, BUILD-EVIDENCE and git log, not re-derived from source -->

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
- **Phase 9 — data-lifecycle & log hardening (ADR 0017).** Store
  retention/pruning (the unbounded-SQLite gap): `RetentionPolicy` +
  `RunStore::prune` removes only terminal (completed/failed/canceled) aged /
  over-count runs — journal + episodes + row in one transaction; in-flight and
  paused runs are never candidates, by construction. `[retention]` config
  (`prune_on_start` default on, `max_runs` default 10 000, `max_age_days`;
  0 → noop, disable-able). A deployment log-size cap (Quadlet `LogOpt=max-size`
  + `max-file`, ≤30 MB rotation; journald alternative documented). A
  secrets-in-logs redaction pass (`scrub_secrets` at the logging boundary,
  capture-tested, extending ADR 0015). Done (on `origin/main`).
- **Phase 10 — governed self-learning (D).** The runtime distils reusable
  facts from past runs into memory — provenance-tagged, completely reversible,
  and entering only as validated context, so a learned (even poisoned) fact can
  never trigger an unvalidated action (ADR 0016). Done.
- **Phase 11 — security hardening (ADR 0018).** SSRF fix, per-caller
  principals (named bearer tokens → principal identities, attributed per run —
  additive: one token stays principal `default`, no behaviour change),
  WebSocket-channel auth, and opt-in TLS termination (tailnet stays the default
  transport). Done (on `origin/main`).
- **Web-UI redesign.** Full IA + UX respec (`apps/web-ui/design/`, both human
  gates passed) and its implementation: the vanilla web UI replaced by a React
  19 + Tailwind v4 + shadcn-style app (Vite → RustEmbed, same single artifact
  across all topologies, ADR 0004) — run-centric master–detail with a bucketed
  live execution axis (kernel rejections visible), honest 501/read-only and
  reconnection states, Lighthouse a11y 100, 300 KB bundle — plus two additive
  bearer-gated endpoints: journal-replay history (`GET /runs/{id}/events`) and
  Stop (`POST /runs/{id}/cancel`). The 3-user "input vs output" test stays a
  human evidence task. Done.
- **Skills subsystem (ADR 0019).** OpenSkills / `SKILL.md` procedural
  knowledge, open to any source (`kelter skill add <dir|bundle|url>`):
  instructions enter the oracle's context under mandatory progressive
  disclosure and budgets; bundled scripts/tools are only candidates for the
  existing sandbox-by-default registry. The invariant — advisory, never
  authorizing; the kernel re-gates every proposal — is structurally enforced
  (schema pass-through) and test-proven (265-skill ECC mass load, urged
  out-of-space action rejected, no exec on activation). Done.
- **Phase 12 — model self-activation of skills (ADR 0020).** The model can
  now flip an advisory skill on/off itself, mid-run, via two kernel-owned
  actions (`activate_skill`/`deactivate_skill`) handled inline in the engine
  loop exactly like `finish` — never routed through the tool substrate, so
  activation grants zero capability; the only effect is which skill
  instructions the model sees on the next step. Run-scoped and ephemeral
  (`Run.active_skills`, never written to the operator's persistent registry);
  gated by a `SkillGate` seam (existence, an operator trust policy, and a
  shared advisory-context budget — a hard floor, refused rather than
  overflowed); the out-of-space invariant is unchanged under model
  activation. Daemon-only for now (desktop is not wired). Default policy is
  `trusted`-only, which makes the feature inert until an operator explicitly
  trusts a skill — flagged to the maintainer as an open decision (`trusted`
  vs `all`). Done.
- **Phase 13 — interactive ("ask") skill activation + skills management UI
  (ADR 0021).** A third `SkillGate` outcome — `AskHuman` — lets an untrusted
  self-activation durably pause a run (`RunStatus::AwaitingSkillDecision`)
  for a human deny/allow-once/allow-always decision instead of only ever
  being silently allowed or denied; the decision is journaled and applied
  through the same propose→validate→execute mechanism every other kernel
  decision uses, so no path grants capability under any of the three
  outcomes. `[skills] ask_fallback` defaults to `"deny"` — a run under
  `model_activation = "ask"` alone never pauses until an operator opts into
  `"queue"`, at which point a background timeout sweeper bounds every wait.
  `allow_always` is an explicit, flagged amendment to ADR 0019 (the only new
  API that can move a skill's trust outside a direct sidecar edit —
  reachable only via a human's bearer-gated decision, never model-
  initiated). A mid-flight addendum (Part 6) also gives skills
  management — list/install-from-url/remove/activate/deactivate/policy — an
  HTTP surface (`/api/v1/skills*`) and a live-mutable `RegistrySkillGate`
  policy, closing a gap the frontend implementer correctly flagged rather
  than guessed at. Durability proven against real SQLite across a
  restart; 15 new tests, 0 regressions (157 → 172). Done.
- **Phase 14 — parallel plan fan-out + governed delegation (ADR 0022).**
  Plan mode's `execute_plan` is now a readiness-driven admission loop
  (`futures::stream::FuturesUnordered` + `tokio::spawn`) instead of a
  strict sequential `for` loop — independent DAG nodes (tool/A2A/review
  alike) run concurrently once their dependencies are satisfied, proven
  with real wall-clock timing (3 independent 250ms-delayed A2A nodes
  complete in ~250-350ms, not the ~750ms sequential would need). Ships
  together with governed delegation limits — `KernelConfig.delegation`
  (`max_delegations_per_run`, `max_depth`, defaults 10/3, operator-tunable
  via `[delegation]`), plus kernel-side cycle detection via
  `Run.a2a_ancestry` — enforced as a hard, journaled/emitted refusal
  (mirroring the ADR 0020/0021 skill-gate `Deny` pattern), never a silent
  skip. Depth/ancestry ride the actual A2A wire protocol as a `metadata`
  object on `SendMessage` (origin strings, not agent display names — the
  same SSRF-pinned identity `A2aClient::allowed_origin` already computes),
  so a chain of Kelter-to-Kelter hops enforces the limit correctly; a
  non-Kelter hop in the middle is exactly as opaque as it already was.
  `RunEvent` gains a `branch: Option<String>` dimension so a fan-out's
  timeline can be split into swimlanes — `crates/api` needed zero code
  changes (confirmed by grep, not assumed). Durable multi-branch resume
  (a crash with MULTIPLE in-flight A2A branches reconnects to ALL of them,
  never re-delegates) proven against real SQLite + two real mock A2A HTTP
  servers, not just reasoned about. 9 new tests, 0 regressions
  (172 → 181). Done.
- **Phase 15 — agent trust/policy/ask + governed resilience (ADR 0023).**
  Extends the ADR 0020/0021 skill trust/policy/ask machinery to A2A
  delegation itself — a new `AgentRegistry` + `DelegationGate` seam gates
  `delegate_to_*` proposals on the target agent's TRUST, with the same
  three-way `Allow`/`Deny`/`AskHuman` outcome and durable pause mechanism,
  mirrored rather than reused since delegation is a real capability gate
  (dials a remote endpoint) where skill activation was advisory-only. A
  config-declared agent starts trusted (an operator's own act, non-breaking
  for existing deployments); a dynamically-discovered agent (registered by
  card URL over `/api/v1/agents`) always starts untrusted, SSRF-pinned at
  registration via the same origin-pinning real delegation already
  enforces. Ships with governed resilience — bounded retry, sanctioned-only
  fallback (`[delegation.fallback]`, never auto-discovered), and a
  per-agent circuit-breaker — none of which may bypass the existing P2
  budget/depth/cycle check (proven: a budget of 3 against an always-failing
  agent with 5 retries configured produces exactly 3 dials, never a 4th).
  Circuit-open on the primary skips straight to fallback (agent-scoped);
  a budget/depth/cycle refusal aborts the whole retry/fallback loop
  (run-scoped) — proven as two distinct behaviors in one test. Plan-mode
  retry/fallback and its `AskHuman` pause are explicitly out of scope this
  round (disclosed, not silent) — the concurrent admission loop's
  single-writer budget counter would need to become cross-task-shareable
  first; plan mode still gets the trust gate and circuit-breaker check at
  its existing admission point. 9 new tests (all against real mock A2A
  servers + a real `Kernel`), 0 regressions in every crate this round
  touched (133/133 passing); a genuine bug (a discarded failure reason
  under a specific retry/fallback no-op path) was caught by the round's own
  test-writing, root-caused, and fixed. Full-workspace `cargo test
  --workspace` could not be completed as a single clean run this session
  due to sustained shared-machine resource contention (disclosed honestly
  in the evidence log, not papered over) — every crate touched compiles
  and tests clean individually and in combination. Done.
- **Phase 16 — WASM memory backend (ADR 0024).** The memory capability moved
  behind the ADR 0011 WASM sandbox: a memory backend can now ship as an
  in-process, capability-gated module rather than a compiled trait impl or a
  subprocess. Proves the governance invariant on a second capability axis — a
  poisoned fact from a WASM store reaches the model's context but can never
  authorize an action, an ungranted capability is refused at load, and
  retrieval stays bounded by the kernel's limit rather than the module's
  claims. Done.
- **Phase 17 — delegation ask + interactive multi-turn + streamed progress
  (ADR 0025/0026/0027).** Three slices closing the delegation UX gaps ADR
  0023 left open: plan mode gets its own `AskHuman` pause on a delegation
  proposal (0025, the explicitly-deferred half of Phase 15); a delegated
  A2A task can carry a real multi-turn exchange instead of a single
  request/response (0026); and delegation progress streams back to the
  parent run's timeline rather than surfacing only at completion (0027).
  Done.
- **Phase 18 — shared Postgres + pgvector memory backend (ADR 0028).** An
  opt-in concurrent-safe shared memory backend behind the existing memory
  seam, so a desktop kernel and a server kernel share episodic memory and
  learned facts with governance intact — provenance-tagged, completely
  reversible (forget leaves zero residue, vector included), learned-as-
  validated-context preserved. SQLite stays the default; the store stays
  dumb (zero opaque learning). 7 tests against live Postgres 16 + pgvector.
  Done.
- **Phase 19 — run-output prose + artifact files (ADR 0029).** The terminal
  result of a run renders as prose instead of raw JSON, and the files a run
  produced surface in a side artifact panel (markdown-rendered, copy +
  download), backed by a new bearer-auth'd files endpoint scoped by the
  run's own journal and hardened against path traversal, symlink escape and
  allowlist bypass. Done.
- **Phase 20 — plan-mode resilience (ADR 0030).** Bounded retry plus
  adaptive re-planning at the plan layer: a failed node no longer sinks the
  plan, and a re-plan stays validated and durable like any other plan
  (closing the "adaptive re-planning" item long parked in Deferred). Done.
- **Phase 21 — agentic coding on local models (oculink brief A1, commit
  `b6f673f`).** Role-split code tools (a reference wedge separate from the
  coding surface), model-adapter tuning, and the first round of loop-safety
  fixes — the groundwork that made the A2 measurement possible. Stopped at
  the defined human gate before A2, as designed. Done.
- **Phase 22 — state-epoch duplicate rule (ADR 0031).** The deterministic
  anti-stall rule and Constrained action-space narrowing became state-epoch
  scoped instead of state-blind: a successful MUTATING step (one whose
  action spec declares `idempotency_key` — the tool author's own existing
  side-effect declaration, no new manifest field, no trust in the model)
  opens a new epoch, and both rules only look back to it. Re-running the
  test suite after an edit is now legal; a stateless repeat with nothing
  mutating in between still dies exactly as before. The measured loop-killer
  is gone: duplicate-rule deaths 5/12 pre vs 0/12 post across an N=24
  before/after live series on gemma-3-27b (Fisher one-tailed p≈0.019),
  premature finish 0/24. Wide latitude, plan mode and the journal shape are
  untouched; exactly-once across resume re-proven post-change. Done.
- **Phase 23 — plan distillation (ADR 0032).** The second self-learning slice,
  deferred by ADR 0016 and now closed. A completed plan-mode run distils to its
  LAST journaled generation (earlier ones failed and triggered a re-plan, so
  learning from them would be learning failure); one run yields one artifact,
  plan mode a plan and step mode a fact. Candidates are re-validated against
  the LIVE action space before being offered, so a plan naming a departed tool
  ceases to exist for the model — staleness handled by the action space, not by
  a policy. The model adopts explicitly via `adopt_learned_plan`, offered on
  generation zero only, and the adopted plan re-enters the identical
  `plan::validate` floor. The journal records it BY VALUE, so `forget` removes
  future influence without rewriting a finished run's history. Plan mode also
  gained the retrieved context it never had. 280 hermetic tests (289 with
  Postgres), CI green. Loose ends: no plan leg in `scripts/smoke-selflearn.sh`,
  no web-UI surface for plans, adoption branch not independently reviewed.
  Done.

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
- **C · Hardening and real deployment** — *shipped as Phase 8 (ADR 0014); the store-retention / log-size / redaction slice is Phase 9 (done, ADR 0017).* Close the accumulated honest deferrals
  — desktop on the durable store, cost in dollars, OpenTelemetry metrics and
  logs, the embeddings retrieval switch, ledger-dedupe scaling, auth — plus
  packaging (above) and deployment on real rootless-podman hardware. Necessary
  for real users; a sweep, not a headline.
- **D · Governed self-learning** — *complete: knowledge slice shipped as Phase 10 (ADR 0016), plan slice as Phase 23 (ADR 0032).* The runtime distils reusable knowledge and
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

- **Security hardening — further (core shipped as Phase 11, ADR 0018).** The
  Phase 11 slice (SSRF fix, per-caller principals with per-run attribution,
  WebSocket-channel auth, opt-in TLS termination — tailnet stays the default
  transport) is done. Still deferred: mTLS, OAuth/OIDC, and OS-level subprocess
  containment.
- **Correctness-sensitive extensions.** Adaptive re-planning — *shipped as
  Phase 20 (ADR 0030)*. WASM for the memory capability — *shipped as Phase 16
  (ADR 0024)*. Plan distillation — *shipped as Phase 23 (ADR 0032): a learned
  plan re-validated against the live action space, adopted only by explicit
  model choice, journaled by value so forget cannot rewrite history. Loose
  ends: no plan leg in `scripts/smoke-selflearn.sh`, no web-UI surface for
  plans, adoption branch unreviewed.* Still deferred: WASM for the agent /
  policy capabilities plus the component model and a real network host-fn.
- **Governed shared-memory backend (Postgres + pgvector) — Done (ADR 0028; validated 7/7 on real Postgres: concurrency, A→B sharing, provenance + zero-residue forget, poisoned-fact-never-an-action).** A
  concurrent-safe shared memory backend behind the existing memory seam (ADR
  0005/0008/0024), so two independent kernels (desktop + a server) can share
  memory + learned facts — with Kelter's governance intact (provenance-tagged,
  completely reversible, learned-as-validated-context, re-proven on the shared
  store). A dumb store Kelter controls, deliberately NOT an opaque "smart
  memory" service (no Hindsight-style belief-formation, which would break the
  reversible/governed thesis). Opt-in; local SQLite stays the default; enables
  the two-independent-kernels sharing path (thin-client sharing already works
  via ADR 0004).
- **Run output UI — prose summary + artifact panel — Done (ADR 0029).** The
  run result renders as a readable prose summary (not raw JSON, graceful for
  unknown shapes), and files a run produces (e.g. an `.md` report) surface in a
  side artifact panel — opened by default, markdown-rendered (react-markdown, no
  innerHTML), with copy + download. Produced files are exposed **journal-scoped**
  (only names from the run's own receipts, never a workspace listing), behind
  bearer auth and path-traversal-safe (validated live + in tests).
- **Plan-mode delegation ask — Done (ADR 0025); plan-mode resilience — residual.**
  The plan-mode `AskHuman` gap is closed: it now pauses (drain-then-pause)
  instead of degrading to a refusal, with interactive deny / allow-once /
  allow-always (allow-once for this run only; allow-always elevates trust
  persistently) — shipped and tested. Remaining residual: plan-mode
  retry/fallback (a re-plan on failure) — a small future round, not a blocker.
- **Multi-agent, further (see `docs/multi-agent-expansion-plan.md`).** P1–P4 of
  that plan shipped as Phases 14–15 (parallel fan-out + governed limits; agent
  trust/policy/ask + resilience). **P5 also shipped:** interactive multi-turn
  delegation (a delegation that pauses awaiting the remote agent — durable
  across restart, cancellable; ADR 0026) and streamed remote-delegation progress
  into the timeline with a bounded reassembly buffer + poll fallback (ADR 0027).
- **Additive / lower-stakes.** A2A SSE streaming, webhooks and per-tool skills;
  multi-profile model configs; an embeddings retrieval-quality harness (the
  adapter is done — this is the eval plus a dedicated embedding server).
- **Cosmetic.** ADR-0013 provenance (`claude-opus-4-8` → `claude-fable-5`); the
  self-learning provenance-list evidence sample.
- **Human / hardware / paid.** Real code-signing + notarization (Apple Developer
  ID cert) — note the unsigned `.app`/`.dmg` build is now one command
  (`scripts/build-dmg.sh`, hdiutil-based, no GUI/AppleScript needed). K12
  deployment (on hardware arrival), and any live API / dedicated-embedding-server
  run.

## Beyond

Multi-master offline / online sync (CRDTs) stays explicitly out of scope until
there is a real single-kernel product worth extending from.
