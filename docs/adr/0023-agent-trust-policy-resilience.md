---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5
  session_date: 2026-07-20
  human_validated: false
---

# ADR 0023: Agent trust/policy/ask (P3) + governed resilience (P4)

Extends ADR 0009 (governed durable A2A), ADR 0013 (inbound A2A), ADR
0020/0021 (skill trust/policy/ask — the pattern mirrored here, not reused),
and ADR 0022 (parallel fan-out + budget/depth/cycle governance — the floor
this sits on top of, never replaces).

## Read this first: three things stated plainly

1. **Delegation authorization is a real capability gate, not advice.**
   Skill activation (ADR 0020/0021) changes what the model *reads*; it
   grants zero capability, so a refusal there withholds advice. Delegation
   (`delegate_to_X`) dials a real remote endpoint and sends it real work — a
   refusal here withholds **authorization to act**. The mechanics mirrored
   from skills (trust/provenance/policy/ask shapes, the pause-and-resume
   control-flow trick) are deliberately close; the vocabulary is not. Every
   type, doc comment, event name, and error string in this round says
   "authorize" / "send work to" — never "advisory" / "read" / "knowledge".
2. **The default-trust split is a real, disclosed deviation — not a bug.**
   A statically-configured agent (declared in the daemon's own extension
   manifests, an operator's explicit act) starts `trust = "trusted"`.
   A dynamically-discovered agent (registered over HTTP by card URL) starts
   `trust = "untrusted"`. A literal reading of "every agent untrusted by
   default" would break every existing A2A deployment on upgrade — static
   config delegation has shipped and worked since ADR 0009 with zero trust
   gate. Putting an agent in your own config file **is** the operator's
   trust act, exactly parallel to ADR 0019's framing of a manual skill
   sidecar edit. Dynamic discovery is the open, no-special-casing path — and
   exactly where "untrusted by default" earns its keep.
3. **Retry/fallback/circuit-breaker never bypass P2's budget/depth/cycle
   governance.** Every retry and fallback attempt re-enters
   `Kernel::authorize_delegation` (and, when a `DelegationGate` is attached,
   its trust check) exactly like a fresh model proposal. Proven by
   `retry_counts_toward_p2_budget` (`crates/a2a/tests/
   delegation_resilience_test.rs`): a budget of 3 with `max_retries_per_
   delegation = 5` against an always-failing agent produces EXACTLY 3 dials,
   never more, and the stop is a loud, journaled/emitted `Rejected`
   ("delegation budget exhausted"), never a silent halt.

## Part 1 — agent trust + provenance

`crates/a2a/src/registry.rs` — `AgentRegistry`: a single TOML file (not a
directory of sidecars — an agent has no natural per-agent directory the way
a skill's `SKILL.md` does), mtime-cached, mirroring `SkillRegistry`'s API
shape (`open`/`list`/`set_trust`/`trust_of`/`remove`).

```rust
pub struct AgentManifest {
    pub name: String,           // from the agent's OWN Agent Card, never operator-typed
    pub card_url: String,
    pub description: String,    // from the card; empty for a config-seeded agent (see below)
    pub token_env: Option<String>,
    pub trust: String,          // "untrusted" | "trusted" — subsystem-owned, never agent-claimed
    pub source_kind: String,    // "config" | "discovery"
    pub source: String,
    pub registered_at: u64,
}
```

`AgentRegistry::open(path, configured: &[RemoteAgent])` seeds itself from
the daemon's statically-configured remote agents ONCE, idempotently — an
agent name already present in the file (a previous run's discovery, or an
operator's manual edit) is never overwritten by re-seeding. Every
config-seeded entry starts `trust = "trusted"` (Part 1's default-trust
split); `source_kind = "config"`.

`AgentRegistry::register(card_url, token_env, source_kind, initial_trust)`
is how a NEW agent (always `initial_trust = "untrusted"` in the real HTTP
path) enters the registry: it dials the card immediately via
`A2aClient::new` + the new `A2aClient::verify()` — the SAME SSRF-pinning
origin resolution real delegation already enforces (`A2aClient::endpoint`'s
existing host-pinning) — and refuses to persist anything if the dial fails,
the card is unreachable, or it declares a redirecting/off-host endpoint.
Proven by `discovery_registers_untrusted_and_ssrf_pinned`: a
hostile/redirecting card is refused AT REGISTRATION (its error contains
"SSRF"), and `registry.list()` stays empty — a refused registration is
never a "registered, but flagged" half-state.

The registered agent's own declared **name** (not an operator-typed one)
becomes its identity: there is no "install a local agent" concept for a
network protocol, mirroring skills' URL-only-install scope decision.
`AgentManifest::description` is populated from the card too — but ONLY for
discovery-registered agents. A config-seeded agent's description is left
empty, deliberately: dialing every configured agent's card at DAEMON
STARTUP (synchronous, in `AgentRegistry::open`) would add a new network
dependency to a codepath that today has none (config-declared agents are
dialed lazily, on first real delegation, via `A2aClient::endpoint()`'s
`OnceCell`). This is a disclosed, minor gap — the Settings UI shows a blank
description for a config agent, which the operator who configured it
already knows about.

## Part 2 — `AgentDelegationPolicy` + the `DelegationGate` seam

`crates/a2a/src/gate.rs` — `AgentDelegationPolicy { TrustedOnly, All, Ask }`
+ `AgentAskFallback { Deny, Queue }`, mirroring `skills::
ModelActivationPolicy`/`AskFallback` field-for-field. `TrustedOnly` is the
default; because of the Part 1 default-trust split, this is NON-BREAKING
for an existing static-config deployment (every configured agent is already
trusted) while still making a fresh discovery-registered agent inert until
an operator (or a human's `allow_always` decision) trusts it.

`kernel-core/src/delegation_gate.rs` — a NEW, PARALLEL trait, deliberately
not a reuse of `kernel_core::SkillGate`:

```rust
pub enum DelegationDecisionOutcome {
    Allow(String),
    Deny(String),
    AskHuman(DelegationRequestMeta),
}
pub trait DelegationGate: Send + Sync {
    fn authorize_delegation_to(&self, action: &str, target_origin: &str) -> DelegationDecisionOutcome;
    fn elevate_trust(&self, action: &str) -> Result<(), String>;
}
```

Conflating this with `SkillGate` would risk exactly the "advisory" framing
leaking into what is actually a capability gate — see "read this first"
above. `Kernel` gains `delegation_gate: Option<Arc<dyn DelegationGate>>` +
`with_delegation_gate(...)`, mirroring `skills: Option<Arc<dyn SkillGate>>`
+ `with_skills` exactly. `crates/a2a/src/gate.rs`'s `RegistryDelegationGate`
implements it over `AgentRegistry`, matching action names to agents via the
NEW public `a2a::action_name()` helper (kernel-core only ever sees
`delegate_to_*` action names, never agent display names — the mapping lives
entirely in the a2a crate).

### Call-site ordering (re-affirmed from the ADR 0022 seam, not re-litigated)

Both existing `authorize_delegation` (P2: budget/depth/cycle) call sites —
step-mode's fresh-proposal point and the plan-mode admission loop — gained
the SAME second check, immediately after P2 passes: `DelegationGate::
authorize_delegation_to`. P2 first (cheap, target-agnostic — a
budget-exhausted run shouldn't pause a human about a SPECIFIC agent's trust
when the delegation couldn't happen regardless); P3 second (the more
expensive, potentially-interactive check, meaningful only once P2 already
allows the delegation structurally). A P3 refusal still counts as a
consumed attempt for P2's own counter, mirroring how a P2 refusal itself
was already counted in ADR 0022.

### The `Ask` pause: `RunStatus::AwaitingDelegationDecision`

A NEW status, not a reuse of `AwaitingSkillDecision` — a paused run's reason
should be discoverable from its status alone, not require inspecting
`pending_*` fields to disambiguate. `Run.pending_delegation: Option<
PendingDelegation>`, new `JournalEntry::AgentDelegationRequested/Decided`
and `RunEvent::AgentDelegationRequested/Decided` (mirroring
`SkillActivationRequested/Decided` field-for-field — `agent` instead of
`skill`, plus `target_origin`; the `RunEvent` variants also carry `branch:
Option<String>`, reusing ADR 0022's plan-node dimension, always `None`
today since plan mode does not implement this pause this round — see Part 4
below), a `still_awaiting_delegation_decision` journal scan mirroring
`still_awaiting_skill_decision`, and `Kernel::decide_delegation` mirroring
`decide_skill_activation` field-for-field, including the
`AllowAlways`-elevates-trust-then-synthesizes-a-replay-`Decision` trick and
the "journal + state write only, caller re-invokes `execute_run`" contract.

**The load-bearing fix, re-applied**: `RunStatus::AwaitingDelegationDecision`
was added to `unfinished_runs()`'s positive allowlist in BOTH stores
(`storage/src/lib.rs`'s SQL `IN (...)` clause and `kernel-core/src/store.rs`'s
in-memory filter) — this exact bug was caught and fixed once already in ADR
0021's review; grepping for every `AwaitingSkillDecision` site and adding
the delegation counterpart alongside each one avoided reintroducing it.
Proven by `paused_delegation_survives_a_restart`
(`crates/a2a/tests/delegation_durability_test.rs`): a real two-session
SQLite test — pause in session 1, reopen in session 2, assert the paused
run appears in `unfinished_runs()`, re-surface without consuming a step,
decide, and complete.

`a2a::server::state_for` also maps `AwaitingDelegationDecision` to
`TASK_STATE_INPUT_REQUIRED`, the same canonical A2A state used for
`AwaitingSkillDecision`. `cancel_run`'s terminal-exclusion list needed no
change — it already treats anything outside `{Completed, Failed, Canceled}`
as cancelable generically.

### Ask-timeout sweeper: a second small sweeper, not a generalization

`crates/daemon/src/delegation_sweep.rs` mirrors `skill_sweep.rs` almost
exactly (~50 lines). Considered generalizing the one sweeper over "kind of
pending decision", but `decide_skill_activation` and `decide_delegation`
take different pending-request shapes and different decision enums
(`SkillActivationDecision` vs `AgentDelegationDecision`) — a shared generic
sweep would need a new trait-level abstraction in kernel-core purely to
serve this one call site, more invasive than the small duplication. A
second sweeper, gated on `[delegation] agent_policy == "ask" &&
agent_ask_fallback == "queue"`, is the honest, disclosed choice.

## Part 3 — dynamic discovery (HTTP surface)

`crates/api`'s `AgentsController` trait + `AgentView`/`AgentRegisterRequest`/
`AgentPolicyView`/`AgentPolicyUpdate` DTOs mirror the skills-CRUD Part 6
addendum (ADR 0021) almost exactly, including the 404-when-absent
convention (`state.agents.is_none()` → every `/api/v1/agents*` route
reports 404 — the frontend's existing capability-flag signal).

```
GET    /api/v1/agents          -> AgentView[]
POST   /api/v1/agents          -> { card_url, token_env? } -> AgentView | 422
DELETE /api/v1/agents/{name}   -> 204
GET    /api/v1/agents/policy   -> AgentPolicyView
POST   /api/v1/agents/policy   -> AgentPolicyUpdate -> AgentPolicyView
```

Note `POST /api/v1/agents` has NO `name` field: consistent with Part 1, an
agent registers under the name ITS OWN card declares
(`AgentRegistry::register` derives it via the new `A2aClient::verify()`,
which returns the card's `(name, description)` alongside resolving its
endpoint — a small extension to the existing `endpoint()` `OnceCell` dial,
not a second request).

`A2aActionSource` (`crates/a2a/src/lib.rs`) became runtime-mutable: its
`agents: Vec<(String, A2aClient)>` is now `RwLock<Vec<(String,
Arc<A2aClient>)>>` (each client behind an `Arc` so `execute`/`circuit_state`
can clone the handle out of the lock and use it across an `.await` without
holding the lock — a plain `A2aClient` is not `Clone`). `register_runtime`/
`deregister_runtime` let a newly-registered (or removed) agent's
`delegate_to_X` action appear/disappear from `action_schema()` immediately,
no daemon restart — mirroring how `RegistrySkillGate`'s policy became a
runtime-mutable `RwLock` in ADR 0021's Part 6 addendum.

`daemon/src/agents_controller.rs`'s `DaemonAgentsController` implements
`AgentsController` over the concrete `AgentRegistry` + `RegistryDelegationGate`
+ `A2aActionSource` handles (the SAME objects, via `Arc::clone` before any
trait-object upcast, that the kernel and action space actually use — a
policy or registration change here is visible on the very next
`authorize_delegation_to`/`action_schema` call). Policy changes persist via
a small TOML overlay beside the daemon config file, mirroring
`skills_controller.rs`'s `apply_overlay` convention exactly.

The daemon always constructs an `A2aActionSource` — even with ZERO
statically-configured agents — so a deployment with no config-declared
agents can still register its FIRST agent at runtime without a restart. The
existing "no action sources configured" fail-fast bail is computed BEFORE
this (over `servers`/`wasm_sources`/`remote_agents`, the pre-discovery
state), so an always-present-but-empty A2A source never silently satisfies
a check meant to catch a genuinely empty deployment.

## Part 4 — governed resilience: retry, fallback, circuit-breaker

`kernel_core::ResilienceConfig { max_retries_per_delegation: u32,
fallback: HashMap<String, Vec<String>> }` (action-name-keyed; the daemon
normalizes the operator-facing `[delegation.fallback]` TOML — bare agent
names, both keys and values — to action names via `a2a::action_name()`
before building this). Default `max_retries_per_delegation = 2`.

### Scope decision: step-mode only this round

The retry/fallback loop (`Kernel::run_delegation_resilience`,
`dispatch_delegation_attempt`) is implemented for STEP MODE only. Plan
mode's readiness-driven concurrent admission loop (ADR 0022 Part 2) already
threads an explicit, single-writer `a2a_attempts: u32` counter through the
orchestrator specifically to avoid a race on the shared budget under
concurrent node admission; making retries ALSO consume that counter safely
from inside a spawned node's own execution (rather than only at admission)
would require turning it into a shared, cross-task-synchronized counter — a
real, non-trivial concurrency change beyond this round's scope. Plan mode
DOES get the P3 trust gate and a circuit-breaker check at its existing
admission point (refusing outright, no loop, if the circuit is open — an
honest "never dial a known-broken agent" even without full resilience) —
this is a disclosed, bounded scope reduction, not a silently-missing
feature: no plan-mode path exists that bypasses `authorize_delegation` or
dials a circuit-open agent; there simply is no retry/fallback path there
yet. Plan mode's `AskHuman` outcome is likewise treated as a refusal
(Rejected, node fails) rather than a durable pause, for the same underlying
reason: the concurrent admission loop has no single well-defined "the run
is paused" moment when several nodes might be ask-worthy at once.

### The retry/fallback loop (step mode)

Wraps the existing per-step A2A dispatch, NOT `A2aActionSource::execute`
itself (which would make repeated dials invisible to the kernel's
per-attempt budget gate — the brief's explicit STOP condition). On a failed
first dial (or a circuit already open on the primary — see below):

1. **Phase 1 — retries against the primary**, up to
   `max_retries_per_delegation`, SKIPPED ENTIRELY if the primary's circuit
   is already open (retrying a known-broken agent is pointless — go
   straight to fallback). Each attempt re-runs P2 (`authorize_delegation`
   with a locally-incremented `attempted` counter, seeded from the run's
   existing `a2a_*` step count + 1 for the already-failed first attempt)
   and, if a gate is attached, P3.
2. **Phase 2 — the sanctioned fallback list**, in config order, only if
   phase 1 didn't succeed and wasn't aborted. Each candidate is
   independently circuit-checked (an open circuit on ONE candidate skips
   only that candidate, tries the next) and re-authorized (P2 then P3)
   exactly like phase 1.

A P2 or P3 refusal (Deny or AskHuman — the latter treated as a refusal for
an automated retry/fallback attempt, since a headless loop has no clean
durability story for pausing mid-loop; the FIRST, model-proposed attempt
can still pause normally) **aborts the whole loop immediately** — it is a
run-scoped or delegation-scoped refusal, not agent-scoped, so trying yet
another candidate would not fix it. This is the deliberate, reasoned
**circuit-vs-budget distinction**: a circuit-open on the primary skips
straight to fallback (agent-scoped operational fact); a P2/P3 refusal
aborts entirely (governance fact, applies to the whole delegation attempt
regardless of target). Proven distinctly by
`circuit_open_skips_to_fallback_but_budget_refusal_aborts_entirely`.

Every attempt journals its own `Intent` (a fresh durability boundary per
attempt, keyed by a distinct tag) and builds its `ExecutionContext` with an
EMPTY notes map — deliberately NOT the original failed attempt's journaled
notes, since a retry must dial fresh, never reconnect to a task that
already failed terminally. The caller journals ONE final `Receipt` once the
whole loop concludes (success, or the last failure). Per-agent accounting
(`run.agent_usage`) is accumulated across every attempt, not just the last.

Observability: no new `RunEvent` variants for retry/fallback progress — it
reuses the EXISTING `Rejected{step, reason, attempt, branch}` for every
governance refusal AND every transient dial failure encountered mid-loop
(the `reason` string is descriptive enough to distinguish cause without new
client-side vocabulary — grounded with the frontend agent mid-round), and
exactly ONE final `Executed{ok, result, ...}` for the whole logical
delegation. `action` in that final event is always the ORIGINALLY PROPOSED
action name even when a fallback agent actually served it — the result JSON
already carries `result.agent` (from `A2aActionSource::execute`) naming
whoever actually answered.

### Circuit-breaker (daemon-level, cross-run, keyed by agent origin)

`ActionSource` gains a THIRD small default method, alongside `a2a_origin`:

```rust
fn circuit_state(&self, action: &str) -> CircuitCheck { CircuitCheck::Closed }
```

`A2aActionSource` overrides it, backed by `Mutex<HashMap<String,
CircuitState>>` keyed by origin (an agent's observed health, not one run's
budget — deliberately NOT journaled/run-scoped state). Configurable via
`with_circuit_breaker(failure_threshold, cooldown)` (defaults: 3 consecutive
failures / 60s cooldown; the daemon wires `[delegation]
circuit_failure_threshold`/`circuit_cooldown_secs`). On every dial outcome
(success or failure, from ANY caller — fresh, retry, or fallback — the
circuit tracks the agent, not who asked), `record_circuit_outcome` updates
the streak; at threshold, the circuit opens for `cooldown`. The engine
consults `circuit_state` BEFORE every dial — fresh, retry, or fallback —
never dialing a circuit-open target. After cooldown elapses, the query
reports `Closed` (a half-open probe): exactly one real dial gets through; a
failure re-opens with a fresh cooldown, a success closes it (the streak
resets to 0). Proven with a real call-count assertion by
`circuit_breaker_trips_cools_down_and_recovers`: 3 failures trip it; a dial
attempt during cooldown does NOT increase the mock agent's call count; after
cooldown, exactly one probe dials and, on success, closes the circuit.

## Tests (backend, `crates/a2a/tests/`)

- `delegation_governance_test.rs`: `trusted_only_default_gates_untrusted_
  delegation`, `ask_pauses_and_all_three_decisions_resolve_correctly`,
  `discovery_registers_untrusted_and_ssrf_pinned`.
- `delegation_resilience_test.rs`: `retry_counts_toward_p2_budget`,
  `fallback_only_to_sanctioned_agent_still_governed` +
  `fallback_to_untrusted_agent_is_refused_not_skipped` (the design doc's
  test 5 split into two focused functions),
  `circuit_breaker_trips_cools_down_and_recovers`,
  `circuit_open_skips_to_fallback_but_budget_refusal_aborts_entirely`.
- `delegation_durability_test.rs`: `paused_delegation_survives_a_restart`
  (real two-session SQLite, mirroring ADR 0021's durability idiom).

All use real mock A2A HTTP servers (axum) and a real `Kernel` — never
internal-state-only assertions — matching the rigor this codebase already
established for its A2A tests.

## What is explicitly NOT built this round

- Plan-mode retry/fallback and the plan-mode `AskHuman` pause (see the Part
  4 scope decision above) — a future round, once/if the plan-mode admission
  counter becomes safely shareable across concurrent node execution.
- Config-seeded agents' `description` (Part 1) — left blank rather than
  adding a synchronous startup network dependency.
- A generalized ask-timeout sweeper (Part 2) — a second small sweeper
  instead, documented why.
