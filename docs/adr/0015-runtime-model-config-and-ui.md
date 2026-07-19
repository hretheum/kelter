---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 8.5: choose + configure the model from the UI (web + desktop) without editing config.toml — a runtime-mutable policy config API (persist + hot-swap), a real Test-connection health check, secure key handling, and first-run onboarding routing."
---

# ADR 0015: Runtime model configuration + UI onboarding

Status: accepted · Date: 2026-07-19 · Builds on: ADR 0003 (adapter contract), ADR 0004 (topology/desktop uses the web UI) · Scope: MODEL configuration only

## Context

The policy plane is already a one-line swap (`[policy] engine = "local:…"` /
`"openrouter:…"` + `base_url` + `OPENROUTER_API_KEY` from env), but that line is
only reachable by hand-editing `config.toml` and setting an env var. The web UI
is ~51 lines with no settings screen. A user cannot pick or configure a model
from the app, and a fresh install with no reachable model lands on a dead
run-view. This phase adds a config surface — API + UI + first-run routing —
scoped to the **model** only (multi-profile configs, a secrets vault, and
configuring anything else are explicitly deferred).

Two cruxes drove the design.

## Decision

### Crux 1 — runtime-mutable policy config (persist + hot-swap)

The daemon built the oracle once at startup (`build_oracle`) and the kernel
held it as an immutable `Arc<dyn PolicyOracle>`. To make the model changeable at
runtime **without a deep kernel refactor**, the kernel now holds a
`policy::SwappableOracle` — a `PolicyOracle` that wraps `RwLock<Arc<dyn
PolicyOracle>>` and delegates each call to the current inner adapter (cloning
the `Arc` out under a short read-lock, then releasing it before the slow network
call). The kernel is **unchanged**; the daemon keeps a handle and swaps the
inner adapter when the config changes.

A change is both **applied and persisted**:

- **Apply (live):** `POST /api/v1/config/model` rebuilds the adapter and calls
  `SwappableOracle::swap` — new runs use the new model immediately, no process
  restart, no in-flight run disturbed.
- **Persist:** the change is written to `model-override.toml` beside the config
  file. On startup the daemon applies this overlay over the base `[policy]`, so
  a UI-chosen model **survives a restart** (proven by `scripts/smoke-config-ux.sh`).

Honest scope boundary: `latitude` is derived from the adapter kind at build time
and lives on `KernelConfig`, fixed for the process. An engine/model/`base_url`
change applies live; a change that also crosses the local↔frontier boundary
re-derives latitude only on the **next restart** (the persisted overlay makes
that correct). Making latitude itself hot-swappable was out of scope — a
deliberate small, correct step rather than a risky broad one.

### Crux 2 — key handling (secure, no leak)

The OpenRouter key entered in the UI must never be committed or logged. Decision:
a **restricted-permission local secret file** (`kelter-secret-openrouter.key`,
mode `0600`, in the config dir, gitignored) that the adapter reads as a fallback
to the `OPENROUTER_API_KEY` env — with env still the primary, documented path.

- `POST /api/v1/config/model/key` writes the key to the 0600 file; the handler
  logs only *that* a key was stored (and where), never the value.
- `PolicyConfig` gained a `#[serde(skip)] api_key` field, so a key can **never**
  be deserialized from `config.toml`; the daemon injects the resolved key
  (env → secret file) at build time only.
- `GET /api/v1/config/model` returns `api_key_set: bool` — never the value.

`scripts/smoke-config-ux.sh` asserts the key is absent from `config.toml`, from
every daemon log, and from the API response, and that the file is `0600`. The
env-guided path remains available for anyone who prefers not to store a file.
(Deep log redaction is Phase 9; this phase simply introduces no leak.)

### The rest

- **Real health check.** `POST /api/v1/config/model/test` actually hits the
  provider before saving: llama.cpp `/health`, ollama `/api/version`, OpenRouter
  `/models` (a real GET, success/failure surfaced — never a faked "OK").
- **UI config screen** (web, inherited by the desktop shell): a Local/API
  picker + presets that pre-fill `base_url` (llama.cpp `:8080`, ollama `:11434`,
  OpenRouter) + model + key fields, a "Test connection" button, and Save —
  matching the existing minimal vanilla-JS style.
- **First-run routing.** On connect the UI reads the config and health-checks
  the current model; with none configured or an unreachable one it shows the
  config screen instead of a dead run-view.
- **Governance.** Every config endpoint is bearer-gated (anonymous → 401, via
  the same auth gate as the rest of the API, including the Phase 8 rate
  limiter); a config change goes through the daemon, never around the kernel.

## Consequences

- The model is a first-class, runtime-mutable choice from the UI; `config.toml`
  becomes the *initial* default, overlaid by the UI choice, which survives
  restarts.
- The kernel stayed untouched — the swap lives entirely in a `PolicyOracle`
  wrapper, so validation/journaling/durability are unaffected.
- Key handling is honest: env-guided primary + a 0600 gitignored file, never in
  the tree or logs; the live API leg still needs the maintainer's own key (a
  documented human step — the key is never committed).
- Deferred, explicitly: multi-profile configs, a real secrets vault/OS keychain,
  hot-swappable latitude, and configuring anything beyond the model.
