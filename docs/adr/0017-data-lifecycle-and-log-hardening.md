---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 9: data-lifecycle & log hardening — bounded store retention (prune TERMINAL runs only, durability intact), a platform-level log-size cap (Quadlet LogDriver/LogOpt), and a real secret-redaction pass with a capture test. Logging stays on stdout (twelve-factor)."
---

# ADR 0017: Data-lifecycle & log hardening

Status: accepted · Date: 2026-07-19 · Builds on: ADR 0004 (topology/deploy), ADR 0007 (durable store/journal — retention is added here), Phase 8 (ADR 0014 — ledger scaling + OTel; not duplicated)

## Context

Three data-lifecycle gaps remained open. (1) The durable SQLite store
(runs/journal/episodes) grew **unbounded** — the main gap. (2) The daemon logs
to stdout (correct, twelve-factor), but `deploy/kelter.container` had **no log
cap**, so container/journald logs could grow without bound. (3) There was no
explicit **redaction** guarantee for secrets in logs. This ADR closes all three
without changing the logging design (stdout stays; no in-app file logging).

## Decisions

### A. Store retention — prune TERMINAL runs only (the safety crux)

A configurable retention policy prunes the store; the non-negotiable invariant
is that it **never** touches a non-terminal (in-flight) run, so durability and
exactly-once are untouched.

- `RetentionPolicy { max_runs, max_age_secs }`; a run is a prune candidate only
  if its status is **terminal** (`completed`/`failed`/`canceled`) AND it is
  older than `max_age` or beyond the newest `max_runs`. The SQL/filter selects
  from terminal runs only — a `pending`/`running` run can never be selected.
- `RunStore::prune` deletes, in **one transaction** per store, each victim
  run's journal rows, its episodes, and the run row — so a run and its markers
  are pruned together (never half-pruned; no orphaned journal). The Phase-3
  resume path is unaffected: it resumes `pending`/`running` runs, which prune
  never removes.
- Triggered by the `kelter --config X prune` **subcommand** (on-demand) and, by
  default, once on daemon start (after resume) — `[retention] prune_on_start`.
  A generous default (`max_runs = 10000`) bounds growth out of the box without
  losing recent history; set `prune_on_start = false` or a larger cap to change
  it. Prune runs *after* resume and only ever targets terminal runs, so a
  just-resumed run is never at risk.

Proven (not claimed): `scripts/smoke-retention.sh` — N+extra terminal runs →
prune → exactly N remain, an injected in-flight run + its journal untouched, no
orphaned journal rows; plus deterministic unit tests
(`prune_keeps_n_terminal_and_never_touches_in_flight`,
`prune_removes_terminal_runs_journal_episodes_but_not_in_flight`) and the
durability smoke still green.

### B. Log-size cap at the deployment (stdout stays)

The daemon keeps logging to **stdout** — no in-app rolling file logging (that is
the correct design, not a gap). The size bound belongs to the platform:
`deploy/kelter.container` now sets `LogDriver=k8s-file` +
`LogOpt=max-size=10mb` + `LogOpt=max-file=3` (Quadlet keys, verified live
against podman 6.0.1), capping total container log at 3 × 10 MB = 30 MB. The
generated podman command carries `--log-driver` + `--log-opt max-size/max-file`
and `quadlet -dryrun` passes cleanly. Alternatives, documented: `LogDriver=journald`
+ journald `SystemMaxUse=`, and for a bare daemon the operator redirects stdout
to a rotated file (`logrotate`).

### C. Secret redaction — real, with a test

Audit result: the codebase does **not** pass any secret value to a log macro —
auth headers, request/response bodies, the OpenRouter key and A2A payloads are
never logged (the OpenRouter key is only referenced as "stored at <path>"; the
only full-token output is the deliberate one-time *pairing* `println` of a
generated token to stdout, which an operator captures — set a config/env token
to avoid it). To keep this enforceable:

- `kernel_core::redact` provides `redact_secret` (a masked, non-reversible
  fingerprint) and `scrub_secrets` (strips `Bearer …`/`sk-…`/`key-…` runs from
  any string before it is logged).
- The daemon logs only a **masked fingerprint** of the active bearer token
  (`redact_secret`), never the value.
- A capture test (`redaction_test.rs`) installs a tracing subscriber over a
  buffer, logs a real secret through the redaction path, and asserts the secret
  value is **absent** from the captured output (only the `…redacted` marker
  appears). Real, not a claim.

### D. Log-level discipline

Default level is `info` (`EnvFilter` falls back to `"info"`); `RUST_LOG`
overrides it and is documented in `docs/observability.md`. No `debug` firehose
in the default/production path.

## Consequences

- The store no longer grows unbounded, and it can never lose an in-flight run to
  retention — the durability guarantee is preserved by construction (terminal-only
  selection + transactional per-run delete).
- Container logs are bounded at the platform, with the twelve-factor stdout
  design intact.
- "Secrets never in logs" is an enforced, tested invariant, not an aspiration.
- Deferred, explicit: in-app rolling file logging (correctly not done), advanced
  archival/export, remote log shipping, and per-field structured-log redaction
  beyond the audited paths.
