---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 8: a sweep of accumulated honest deferrals from Phase 3 — desktop durability, dollar cost, OTel metrics+logs, an embeddings switch, ledger dedupe scaling, auth hardening, and packaging scaffolding — plus the two acts that stay human-blocked (code-signing and K12 deploy)."
---

# ADR 0014: Production-hardening sweep

Status: accepted · Date: 2026-07-19 · Closes deferrals from: ADR 0004 (topology/desktop), ADR 0005 (memory seam), ADR 0007 (durable execution) · Scope: a bounded SWEEP of independent items, each with its own DoD

## Context

The Phase 3 "uncertainties" list (docs/BUILD-EVIDENCE.md) accumulated several
honest deferrals — each a small, real gap rather than a feature. This ADR
records closing them as a **sweep**: independent, bounded sub-deliverables, not
one design. Each is small enough that the rationale is short; the through-line
is "make the runtime production-honest without overclaiming."

Two acts are **not** in this phase and stay human-blocked (documented, not
done): real code-signing / notarization (needs an Apple Developer ID
certificate — a paid, human-held credential; an agent must not sign) and
deploying to the K12 homelab host (the hardware has not arrived; the Podman
Quadlet is already locally validated).

## Decisions

**A. Desktop → durable store.** The standalone desktop shell now builds a
`storage::SqliteRunStore` + `SqliteMemoryProvider` on one WAL database in the
app data dir (two handles, one file) instead of the in-memory stores, and
resumes unfinished runs on launch. Runs, journal and episodes survive a desktop
restart — the same durability guarantee (ADR 0007) the daemon already had.

**B. Dollar cost.** A per-model price table (`kernel_core::cost::PricingTable`)
maps the policy `describe()` string to a $/million-token price; a run's
journaled token usage is multiplied through it at finalize and exposed as
`Run.cost_usd`. Prices are static config (built-in dated defaults, extendable
via `[pricing]`), never a live fetch. **Honest by construction:** a local
(self-hosted) model is priced $0 — it has no API bill — and a model absent from
the table yields `null`, never a fabricated number.

**C. OTel metrics + logs.** The observability path gains the other two pillars
beside traces: **metrics** (counters for runs started/finished, steps,
tool-calls, validation rejections + a run-latency histogram, recorded by a task
that translates the kernel's run-event stream) and **logs** (tracing events
bridged to OTel logs via `opentelemetry-appender-tracing`). All three export
over OTLP/HTTP and are **off by default** (only when `[otel] endpoint` is set),
like traces. Two robustness decisions fell out of implementing it: the OTLP
exporter uses a **blocking** HTTP client (the metrics PeriodicReader and batch
processors run on their own threads with no tokio reactor — an async client
panics there), and shutdown flush runs on a worker thread with a 3-second
timeout so a dead collector can never wedge Ctrl-C.

**D. Embeddings switch.** The Phase 3 retrieval seam is now switchable
(`memory::Retrieval`): BM25 stays the **default**; `Embeddings` uses a **real**
backend — `LlamaEmbeddingBackend` calls an OpenAI-compatible `/v1/embeddings`
endpoint and ranks by cosine similarity (no stub, no fake vectors). It is
selected via `[memory] embeddings_url` (daemon → env → the built-in
memory-server). If the backend errors at runtime, retrieval **falls back to
BM25** rather than failing a run. Honest boundary (verified against the
llama.cpp server docs): a llama.cpp `--embedding` server is embedding-only and
needs a dedicated embedding model, so this points at a **separate** server from
the generation model; the adapter is unit-tested over real HTTP against a mock
`/v1/embeddings` server, and the end-to-end comparison against a live embedding
server is the one piece left for a deployment that runs one.

**E. Ledger dedupe scaling.** The exactly-once ledger tool no longer re-reads
and scans the whole file on every append (O(n) per append → O(n²) per run). It
keeps an in-memory per-ledger key index, **lazily rebuilt from the file** on
first touch (one scan per process), so lookups are O(1). The file line remains
the single durable source of truth — a crash between the write and the index
insert just reloads the key from the file on the next process — so exactly-once
is unchanged (durability smoke stays green).

**F. Auth hardening.** Scoped, not sprawling: the bearer token can now come from
the `KELTER_TOKEN` **env** (secrets out of the config file / tree), taking
precedence over the config value; and a failed-auth **rate limiter** (a sliding
window; past a threshold the endpoint returns `429` for a cooldown) sits on top
of the existing constant-time compare as a basic brute-force slowdown.
Deliberately **out of scope** (documented, not attempted): TLS/mTLS,
per-caller principals/tokens, and auth on the WebSocket event stream (it keeps
the existing query-token check). These are real next steps, not silently
skipped.

**G. Packaging scaffolding.** `tauri.conf.json` declares the bundle targets
(`app`, `dmg`, `nsis`, `deb`, `appimage`) and the macOS/Windows signing hooks,
left **unsigned** (`signingIdentity` / `certificateThumbprint` = null).
`scripts/package-desktop.sh` builds an unsigned `.app`/`.dmg` (universal
arm64+x86_64 with `--universal`). Real signing + notarization is a documented
human step (`docs/packaging.md`) requiring the Developer ID cert.

## Consequences

- The runtime is production-honest on the axes that were deferred: durable on
  the desktop, cost-aware without fabrication, observable on all three OTel
  pillars, retrieval-pluggable with a real embedding option, and scalable in the
  dedupe path — with auth hardened within a stated scope.
- Nothing was faked to close an item: unpriced models are `null`, embeddings are
  real (or fall back to BM25), and unsigned bundles are labelled unsigned.
- What remains human-blocked is exactly two acts, both requiring a human-held
  credential or hardware: signing/notarization and the K12 deploy. Both are
  documented as steps, not simulated.
- Follow-ups noted honestly: a live embedding-server retrieval comparison, TLS
  and per-caller auth, and headless `.dmg` creation (which needs a macOS GUI
  session — a known Tauri/macOS constraint).
