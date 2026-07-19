---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Build evidence for Phase 1 (walking skeleton) and Phase 2 (launch-ready); raw logs in docs/evidence/"
---

# Build evidence

## Phase 1 — walking skeleton

Date: 2026-07-18. Machine: macOS (Darwin 25.3.0, arm64), Rust 1.95.0,
llama.cpp b10050 (Homebrew), Docker Desktop. This file logs what was
actually read, decided, run and left uncertain. Raw logs sit next to it in
`docs/evidence/`.

### 1. Sources actually consulted

**Internal contract:** the project assumptions document (internal working
brief; intentionally untracked) — read in full; it is the authority on
intent. Deviations from it are listed in §3.

**External, verified live on 2026-07-18 (not from memory):**

| Area | What was verified | How |
|---|---|---|
| OpenRouter API | `POST https://openrouter.ai/api/v1/chat/completions`; `Authorization: Bearer`; attribution headers `HTTP-Referer` + `X-OpenRouter-Title` (canonical) / `X-Title` (legacy alias); `tools[].function` request shape; response `message.tool_calls[].function.arguments` is a JSON-*encoded string*; `response_format.json_schema` with `strict: true`; error object `{error:{code,message}}` | live fetch of openrouter.ai/docs (api-reference/overview, authentication, parameters, streaming, errors, tool-calling, structured-outputs guides, quickstart) + live `/api/v1/models` JSON, cross-checked with repeat fetches |
| rmcp (Rust MCP SDK) | crate `rmcp` **2.2.0** (2026-07-08; 2.0 was a breaking rewrite ~3 weeks ago — memory of the old API is stale); server via `#[tool]`/`#[tool_router(server_handler)]` + `Parameters<T>`; stdio via `rmcp::transport::stdio`; client via `().serve(TokioChildProcess::new(...))`; `CallToolRequestParams` (plural; old singular name 404s), `CallToolResult{content, structured_content, is_error}` | live fetch of github.com/modelcontextprotocol/rust-sdk README, docs.rs/rmcp pages, crates.io JSON API; then **compiled and e2e-tested against 2.2.0** in this repo |
| Tauri v2 | tauri 2.11.5 / tauri-cli 2.11.4; src-tauri as a plain workspace-embeddable crate; v2 config keys (`productName`, `identifier`, `build.frontendDist`, `app.windows[].url`, `app.withGlobalTauri`, `bundle.*`); runtime window at external URL via `WebviewWindowBuilder` + `WebviewUrl::External`; `cargo tauri build --no-bundle`/`--ci`/`--no-sign`; iOS/Android documented as first-class in v2 | live fetch of v2.tauri.app (create-project, project-structure, reference/config, reference/cli, learn/window-customization, plugin/localhost, develop/sidecar) + crates.io API; then **built twice in this repo** (no-bundle + .app bundle) |
| llama.cpp server + GBNF | `-hf <repo>[:quant]` model pull; `--grammar` / per-request `grammar` field; **empirically confirmed** that `/completion` *and* `/v1/chat/completions` accept a GBNF `grammar` field and constrain decoding (0.5B model produced a byte-exact JSON envelope at temperature 0) | local binary b10050 `--help` + **live requests against the running server** (stronger than docs); see §4 |
| Claude Code headless (executor reference in architecture.md) | `-p/--print` + `--output-format json|stream-json`; result carries `session_id`; Agent SDK packages `@anthropic-ai/claude-agent-sdk` (npm) / `claude-agent-sdk` (PyPI) with `query()`; native MCP client via `claude mcp add` (stdio/http/sse/ws); `--allowedTools` + permission modes for unattended runs | live fetch of code.claude.com/docs (cli-reference, agent-sdk/overview, mcp, settings) |

### 2. Decision log

| Decision | Rationale |
|---|---|
| Placeholder name `agentkernel` (crates keep functional names) | Naming rule: one neutral placeholder used consistently for product/image/unit; final name deferred to the human gate. |
| Product name **Kelter** (decided by Eryk, 2026-07-18, post-DoD) | Placeholder renamed repo-wide: binary/image/Quadlet unit `kelter`, product prose "Kelter"; crates keep functional names. Evidence logs produced before the rename still say `agentkernel` — they are historical records of the actual runs and were left untouched. |
| `ValidatedAction` constructible only via `ActionSchema::validate` | Encodes "kernel trusts no adapter" in the type system, not in convention. |
| Adapter selection as `engine = "<adapter>:<model>"` (single key) | The brief requires the swap to be literally one config line. |
| Local adapter targets `/v1/chat/completions` + `grammar` extension | Server applies the model's chat template; grammar still constrains the decoder (verified empirically). |
| GBNF generator emits every declared property in order; enums as literal alternatives; exotic schemas fall back to a generic JSON value rule | Tight grammar for the common case; kernel-side validation still enforces the full schema on top. |
| Tools = MCP servers only; bundled `tools-server` is a real stdio MCP server; desktop standalone uses the same server over an in-process duplex stream | ADR 0002 (no custom plugin API) without forcing a sidecar binary into the app bundle; both transports covered by tests. |
| Kernel anti-stall rules: exact-repeat proposals rejected (non-wide latitudes); constrained latitude removes already-succeeded tools from the action space | Discovered empirically: the 0.5B model looped on `read_source`. The fix is deterministic kernel behavior (the latitude thesis), not model-specific prompt hacks. Trade-off: constrained latitude assumes pipeline-shaped tasks (each tool succeeds at most once). |
| Tool paths: reads unrestricted, writes confined to the workspace | The 0.5B model naturally passes the original absolute path to `extract_structure`. Reads were already unrestricted in `read_source`; confining only writes keeps the safety property that matters and gives weak models a natural success path. |
| Desktop: one binary, both topologies (standalone embeds the daemon in-process on an ephemeral port; remote points the webview at a URL) | ADR 0004; the two "modes" are runtime configuration, verified by the launcher flow. |
| Wedge task: read → extract structure → markdown report over 3 MCP tools | Deterministic output (GIF-able), real decision points (model picks action + arguments each step), exercised identically by both adapters. |
| Root workspace excludes `apps/desktop/src-tauri` | `cargo build --workspace` must not require the platform webview toolchain. |

**Deviations from the assumptions document:** none of substance. Two
additions beyond its letter: the kernel anti-stall rules (§ above) as a
concrete implementation of the latitude knob, and the in-process MCP
transport as a second (still-MCP) transport. QR pairing mentioned in the
assumptions is not implemented in Phase 1 (listed in §5).

### 3. Definition-of-done evidence

| Gate | Result | Raw log |
|---|---|---|
| `cargo build --workspace` | PASS | `evidence/build-test-clippy-fmt.log` |
| `cargo test --workspace` | PASS — 36 tests, 0 failures (16 suites) | same |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS — no issues | same |
| `cargo fmt --all --check` | PASS | same |
| Smoke, local leg (mandatory) | **PASS e2e** — run `completed`, 3 tool steps + finish on `local:qwen2.5-0.5b-instruct` (GBNF), report produced; live WS stream captured | `evidence/smoke-local-run.json`, `evidence/smoke-local-report.md`, `evidence/smoke-local-events.ndjson`, `evidence/smoke-local-daemon.log` |
| Smoke, OpenRouter leg | Unit test on mocked HTTP: PASS (asserts bearer header, `tools` payload, arguments-as-string parsing, kernel validation round-trip). **Live leg: PASS e2e** — same wedge task on `openrouter:anthropic/claude-sonnet-5`, run `completed` in 3 tool steps + finish (executed later the same day once the key was provided via env) | `crates/policy/tests/openrouter_mock_test.rs` (in `build-test-clippy-fmt.log`), `evidence/smoke-openrouter-run.json`, `evidence/smoke-openrouter-report.md` |
| One-line adapter swap | Config diff shows exactly one `engine =` line differing (plus comments); **the same task ran live through both legs** — local 0.5B + GBNF and OpenRouter frontier — completing identically (compare the two run.json files) | `evidence/config-swap.diff`, `evidence/smoke-local-run.json`, `evidence/smoke-openrouter-run.json` |
| Container image amd64+arm64 | PASS — `docker buildx build --platform linux/amd64,linux/arm64` exit 0, manifest list exported with both platforms | `evidence/docker-buildx.log` |
| Quadlet unit | Written per podman-systemd.unit(5) conventions; **not machine-validated** (no podman on this machine) — listed in §5 | `deploy/kelter.container` (path renamed with the product; pre-rename logs reference the old name) |
| Desktop build | PASS — release build linking `kernel-core` (standalone topology compiled in) + macOS `.app` bundle; remote topology is runtime config in the same binary | `evidence/desktop-build.log` |
| README quickstart actually executed | daemon startup, `/healthz`, run submission and completed run record | `evidence/quickstart-daemon.log` |
| ADRs ×5 + architecture.md consistent with code | crate/type names in docs exist in the workspace (`Kernel`, `ActionSchema::validate`, `PolicyOracle`, `McpToolSource`, `MemoryProvider`, …) | `docs/adr/`, `docs/architecture.md` |
| No secrets in repo; key env-only | adapter reads `OPENROUTER_API_KEY` from env only; `.env*` gitignored; mock tests use obviously-fake keys | `crates/policy/src/lib.rs` |
| Naming rule (case-insensitive grep for the internal codename over the tracked tree = 0 hits; evidence paths sanitized to `<repo>`) | PASS | `evidence/naming-check.log` |

### 4. Empirical verification of constrained decoding (core of the local leg)

Live requests against llama-server b10050 serving Qwen2.5-0.5B-Instruct
(q4_k_m), temperature 0, with a hand-written GBNF envelope grammar:

- `POST /completion` with `grammar` → `{"action":"echo","arguments":{"message":"hello world!"}}`
- `POST /v1/chat/completions` with `grammar` → `{"action":"echo","arguments":{"message":"Hello! How can I assist you today? "}}`

Both byte-valid instances of the grammar — the decoder, not the model,
guarantees the envelope. This is the mechanism the `local` adapter builds
on (`policy::gbnf::grammar_for`).

The wedge itself needed two kernel-side iterations before the 0.5B model
completed it (documented in §2: anti-stall rules, read/write path split) —
the failure runs are preserved in the session history and the fixes are
covered by unit tests. Final smoke: 3 steps + finish, no rejections.

### 5. Uncertainties / TODO for human review

- ~~OpenRouter live leg not executed~~ — **resolved**: executed live
  against `anthropic/claude-sonnet-5`, PASS e2e (see §3). Model IDs in
  examples remain worth re-checking over time as the catalog moves.
- **Quadlet unit not machine-validated** — podman is not installed here.
  Validate with `systemd-analyze --user verify` / a podman host before
  relying on it.
- **`grammar` on `/v1/chat/completions` is an undocumented extension** —
  llama.cpp docs document the `grammar` field only for `/completion`; on the
  OpenAI-compatible route the documented constrained path is
  `response_format: {type: "json_schema", ...}`. This build verified
  empirically that `grammar` works on `/v1/chat/completions` in b10050, but
  a future llama.cpp release could drop it; fallback plan: send the schema
  via `response_format.json_schema` (server compiles it to a grammar
  internally) or move the adapter to `/completion` with a hand-applied chat
  template.
- **Grammar coverage** — `gbnf::grammar_for` covers the schema subset the
  reference tools use (strings, enums, numbers, booleans, nested
  objects/arrays). Exotic JSON Schema (oneOf, patterns, refs) falls back to
  a generic JSON value rule; kernel validation still enforces it, at the
  cost of more retries on weak models.
- **Constrained-latitude narrowing assumes pipeline-shaped tasks** (a tool
  succeeds at most once per run). Tasks that legitimately reuse a tool need
  `standard`/`wide` latitude.
- **Desktop standalone expects a llama-server already running** for the
  default local engine; the shell does not manage the model sidecar yet.
- **Tauri mobile targets** documented as first-class in v2 but not
  attempted here — unverified.
- **DMG bundling** fails headless on macOS (`bundle_dmg.sh` needs a UI
  session); bundle targets are set to `app`. CI builds with `--no-bundle`.
- **Memory retrieval scoring is deliberately naive** (token overlap); it is
  the tested contract seam, not a quality claim. Real backends plug in via
  `MemoryProvider`.
- **In-memory persistence only** — runs/episodes do not survive a restart;
  the traits (`RunStore`, `MemoryProvider`) are the extension points.
- QR pairing (mentioned in the assumptions document) deferred; pairing is
  currently URL + token via the `#token=` fragment.

### 6. Version pins observed in this build

rmcp 2.2.0 · axum 0.8 · jsonschema 0.28 (chosen; `validator_for` +
`iter_errors` API) · tauri 2.11.5 / tauri-cli 2.11.4 (npm @tauri-apps/cli)
· reqwest 0.12 (rustls) · llama.cpp b10050 · model
`Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m`.

## Phase 2 — launch-ready

Date: 2026-07-18 (same day, second pass). Same machine/toolchain as Phase 1.
Scope per the Phase 2 brief: rename verification, 5-minute onboarding, the
side-by-side demo package, the "extend without Rust" proof (ADR 0006 first
slice), launch-grade README + Product Hunt draft.

### 1. Sources verified live (Phase 2)

| Area | What was verified | How |
|---|---|---|
| OpenRouter models | `anthropic/claude-sonnet-5` exists **verbatim** in the live catalog; also verbatim: `anthropic/claude-opus-4.8`, `openai/gpt-5.2`, `google/gemini-3.1-pro-preview`; chat-completions endpoint unchanged | raw `https://openrouter.ai/api/v1/models` JSON (curl + parse, not summarized) + api-reference/overview; **plus both live runs in this repo** |
| Python MCP SDK | package `mcp` **1.28.1** (PyPI), Python ≥ 3.10; official FastMCP pattern `from mcp.server.fastmcp import FastMCP`, `@mcp.tool()`, `mcp.run()` defaulting to stdio; plain `python3 server.py` is the documented "Direct Execution" path; type hints → tool schema. Caveat: the repo's main-branch README documents an unreleased 2.0-alpha API — v1.28.1 tag is authoritative for what pip installs | github.com/modelcontextprotocol/python-sdk (tag v1.28.1 docs), pypi.org/pypi/mcp/json; **then run for real against rmcp 2.2 in this repo** |
| rmcp | 2.2.0 still the latest stable on crates.io — consistent with the Phase 1 lockfile | crates.io API + `Cargo.lock` |
| llama.cpp | same b10050 binary as Phase 1 (no upgrade risk taken mid-phase); `grammar`-on-chat-completions caveat from Phase 1 §5 stands | local binary |

### 2. Decision log (Phase 2)

| Decision | Rationale |
|---|---|
| Extension slice = manifest-declared MCP tool servers (`kelter-extension.toml`, `provides = ["tools"]`, `extension_dirs` scan) | Shortest real slice of ADR 0006 that proves "extend without recompiling"; unknown capabilities fail startup loudly (forward-compatible gate), extensions run in their own cwd with `KELTER_WORKSPACE` passed in env. |
| Example extension = Python tool (`wordstats`, ~25 lines, official `mcp` SDK) | The brief's default: shortest, most readable money-shot. Self-bootstrapping `run.sh` creates a private venv on first start (all setup output on stderr — stdout belongs to the MCP protocol) and picks the first Python ≥ 3.10 (macOS system python3 is 3.9). Dependency pinned `mcp>=1.28,<2` — the SDK's v2 line is a breaking rewrite (per the v1.x docs' own pinning recommendation). |
| `GET /api/v1/tools` added to the API | Makes extension registration *observable* — the demo and smoke assert against the live action space instead of trusting logs. |
| `[tools] bundled_tools = false` option | Runs the kernel with only user extensions; also gives the extension smoke a deterministic 2-action space (`word_stats` + `finish`) a 0.5B model reliably completes. |
| MCP client parses JSON-as-text tool results into structured values | FastMCP (and other servers) return plain dicts as text content; surfacing them structured keeps the kernel journal machine-readable for any server. |
| Extension smoke asserts on the **journaled step result**, not the final file | Found empirically: with the full tool set, the 0.5B model overwrote the extension's output file in later wandering steps. The journal is the authoritative record; the deterministic fix (narrow action space via `bundled_tools = false`) plus this assertion makes the smoke honest and stable. |
| OpenRouter adapter: dedicated native-tool-calling system prompt + `temperature: 0` | Found empirically: the Phase 1 envelope-style prompt ("respond only with JSON") conflicts with native tool calling — the frontier model sometimes answered in prose instead of calling `finish`, failing the run. The native prompt instructs one tool call per turn; temperature 0 makes demo runs reproducible. |
| README provenance as an HTML comment instead of YAML frontmatter | CLAUDE.md requires provenance tags on agent-authored .md; YAML frontmatter renders as a table on GitHub and would wreck the launch page. The HTML comment carries the same fields invisibly. All other Phase 2 docs use regular frontmatter. |
| Committed `CLAUDE.md` + ADR 0006 (human-authored, no provenance tag per the rule) and the `.gitignore` `prompts/` entry | They are intended tracked artifacts referenced by the brief; briefs themselves stay untracked. |

### 3. Definition-of-done evidence (Phase 2)

| Gate | Result | Raw log |
|---|---|---|
| Rename consistency | Verified: `git grep -i` for the old codename = 0; `agentkernel` only in pre-rename `docs/evidence/` logs + the notes describing them; one stale path in the Phase 1 Quadlet row fixed (`deploy/kelter.container`) | `evidence/phase2-naming-check.log` |
| build/test/clippy/fmt after rename | PASS — build clean; **39 tests, 0 failures (16 suites)**; clippy `-D warnings` clean; fmt clean | `evidence/phase2-gates.log`, `evidence/phase2-test-full.log` |
| 5-minute onboarding | **PASS — fresh `git clone` → `scripts/bootstrap.sh` → first completed run in 53 s** (model already in the shared HF cache; a cold machine adds the ~0.4 GB model download and the first cargo build). Preflight fails softly with actionable messages (missing rustup/llama-server/curl). | `evidence/bootstrap-clone.log` |
| Demo side-by-side | **PASS, both legs live**: local (`local:qwen2.5-0.5b-instruct`, GBNF) completed in 2 s; frontier (`openrouter:anthropic/claude-sonnet-5`) completed in 12 s; identical step sequence; config diff = one `engine =` line. Storyboard with exact commands/timing present. | `evidence/demo-side-by-side.log`, `evidence/demo-local-run.json`, `evidence/demo-frontier-run.json`, `docs/launch/demo-storyboard.md` |
| "Extend without Rust" | **PASS**: `wordstats` (Python, ~25 lines) discovered via manifest, `word_stats` visible in `GET /api/v1/tools`, then **used by the local 0.5B model in a live run** (journaled structured result `{words: 142, unique: 108, top5: …}`), run `completed`. Kernel binary unchanged. Guide `extensions/README.md` written from the working example. | `evidence/smoke-extension-tools.json`, `evidence/smoke-extension-run.json` |
| README launch-grade | Rewritten; leads with the differentiator (deployable runtime + weak-model reliability + governed extensibility); no competitor names; quickstart command is the bootstrap actually executed above; both demos linked to runnable scripts | `README.md`, `evidence/bootstrap-clone.log` |
| PH assets | Taglines, maker comment, FAQ, gallery shot list, hero spec — marked as human-finalized draft | `docs/launch/product-hunt.md` |
| No secrets / no push / no registrations | Key read from env only and masked in logs; no `git push`, no remotes, no handle/domain registration attempted | `git remote -v` empty |

### 4. Uncertainties / TODO for human review (Phase 2)

- **Handles/domain/crate/npm/GitHub-org for "kelter" are NOT secured** —
  gate prerequisite for any publication; not attempted by the agent.
- Frontier-leg wall time varies with API latency (observed 12 s); step
  sequence is stable at temperature 0 but tool-argument phrasing may vary
  slightly between models — re-run the dry runs right before recording.
- The extension venv bootstrap needs a network on first run (`pip install
  mcp`); the recording prep step warms it.
- Phase 1 carried-over items still open: Quadlet not machine-validated
  (no podman here); `grammar` on `/v1/chat/completions` is an undocumented
  llama.cpp extension (fallback documented); persistence/memory are
  in-memory reference impls.
- `docs/PHASE-1-RESULTS.md` (reviewer document) states the OpenRouter live
  leg was not executed — that was true at its writing time and was resolved
  later the same day (Phase 1 §3); left unedited as a human-authored record.

### 5. Version pins observed (Phase 2)

mcp (Python) 1.28.1 · rmcp 2.2.0 · llama.cpp b10050 ·
model `Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m` · frontier demo model
`anthropic/claude-sonnet-5` (verified verbatim in the live catalog).

## Phase 3 — durable, operable runtime

Date: 2026-07-18 (same day, third pass). Same machine/toolchain.
Scope per the Phase 3 brief: durable RunStore (SQLite), crash-resume from a
journal, idempotent tool boundaries, durable memory with real retrieval,
OpenTelemetry, op-hardening.

### 1. Sources verified live (Phase 3)

| Area | What was verified | How |
|---|---|---|
| rusqlite | **0.40.1** latest stable; `bundled` feature (no system dep); exact API: `Connection::open/execute/prepare/query_row`, `params!`, `pragma_update`/`pragma_query_value` (user_version, WAL); `Connection` is Send-not-Sync → mutex/blocking-pool patterns; companion `rusqlite_migration` 2.6.0 exists (chose DIY `user_version` migrations for dep-minimalism); sqlx 0.9.0 noted for comparison | crates.io JSON API + docs.rs Connection/params pages |
| OpenTelemetry Rust | opentelemetry/opentelemetry_sdk/opentelemetry-otlp **0.32.x** pair with tracing-opentelemetry **0.33.x**; OTLP default transport is now HTTP (`http-proto` + `reqwest-blocking-client`), gRPC needs explicit `grpc-tonic`; current builder API (`SpanExporter::builder().with_http().with_endpoint()`, `SdkTracerProvider::builder().with_batch_exporter()`); no auto-flush — explicit `shutdown()`; official opentelemetry.io getting-started is stale (0.28) — docs.rs authoritative | crates.io + docs.rs (SpanExporterBuilder, SdkTracerProvider, tracing-opentelemetry); **then compiled and exercised against a live collector in this repo** |
| Retrieval approach | BM25 implemented in-repo (~90 lines, zero deps) per the brief's dep-light preference; embeddings remain an optional future backend behind the same trait | design decision; contract tested (see §3) |
| rmcp / reqwest / axum | unchanged from the Phase 1/2 lockfile (rmcp 2.2.0) | `Cargo.lock` |

### 2. Decision log (Phase 3)

| Decision | Rationale |
|---|---|
| Journal model: `Decision → Intent → Receipt → Terminal` per run, replayed by folding per-step | The Decision entry is written the moment a proposal passes validation — **before** anything acts on it — which is what makes "LLM journaled at first execution, never re-queried on replay" airtight. |
| Replay re-validates journaled decisions against the current schema | Guards against tool-configuration drift between crash and resume; a mismatch fails the run explicitly instead of executing under a different contract. |
| Idempotency via schema-declared `idempotency_key` (kernel-injected, `run_id:step`) | The kernel cannot make a non-cooperating tool exactly-once (fundamental distributed-systems limit). Declaring the key in the tool schema is an honest, checkable contract; the kernel detects it from the schema it already validates against. Tools without it: dangling intents fail the resume loudly ("manual check required") — at-most-once, never silent duplication. |
| Ledger tool: the appended line carries the key; dedupe = scan for the key | Effect and dedupe marker are one atomic write — no window in which the effect exists but the marker doesn't. This is the money-shot tool. |
| One SQLite file (WAL) for runs + journal + episodes + graph; single connection behind an async mutex | Kelter writes one journal row per kernel step — microsecond statements; a pool/spawn_blocking would be complexity without measurable benefit (documented seam if load changes). WAL keeps readers non-blocking and survives OS crashes consistently. |
| Migrations: DIY `PRAGMA user_version`, forward-only array | Dep-light; `rusqlite_migration` exists but adds a dependency for what is ~20 lines here. |
| BM25 (k1=1.2, b=0.75, +1-smoothed IDF) in `memory::retrieval`, shared by both providers | Real scoring with a tested query contract (incl. an IDF case naive overlap gets wrong), zero dependencies, clean seam for embeddings (llama.cpp embeddings endpoint) later. |
| `KELTER_CRASH_POINT` env hook (`after_intent:<tool>` / `after_execute:<tool>`) | Kill-and-resume smokes must crash at exact boundaries of the real daemon process — impossible to time externally. Env-gated, documented, inert in normal operation. |
| Auth review: constant-time token comparison (HTTP header + WS query param) | Closes the timing side channel on token comparison; noted during the op-hardening pass. |
| OTel spans named `kelter.run/step/model_call/validate/tool_call`; export off by default | Table-stakes operability without imposing a collector on every user; batch exporter flushed on Ctrl-C shutdown. |
| Usage accounting on the run record (`run.usage`), fed from adapter-reported token counts journaled per Decision | Per-run cost visibility through the existing API without a new endpoint. |

### 3. Definition-of-done evidence (Phase 3)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — **55 tests, 0 failures (20 suites)** incl. 7 new durability unit tests, 4 storage tests, BM25 contract tests | `evidence/phase3-gates.log` |
| Naming | case-insensitive grep for the internal codename over the tracked tree = 0 hits; `agentkernel` only in historical evidence + notes | `evidence/phase3-naming-check.log` |
| Persistence across restart | **PASS** — run completed → daemon killed → restarted: run + journal (5 entries) + reflection episode still present (SQLite/WAL) | `evidence/durability-restart.log` |
| Crash-resume without repeating steps | **PASS** — run killed mid-flight resumes to `completed`; resume log shows `replaying journaled decision — model NOT queried` and receipt-based skip | `evidence/durability-resume-after-effect.log` |
| Exactly-once side effect | **PASS, both crash windows** — killed *after* the ledger append (before receipt): resume re-dispatches with the journaled key, tool reports `deduped:true`, ledger has exactly 1 line; killed *before* the append: resume fires the effect exactly once | `evidence/durability-resume-after-effect.log`, `evidence/durability-resume-before-effect.log`, `evidence/durability-resume-run.json` |
| Memory: real retrieval + persistence | **PASS** — BM25 ranking contract (6 query→top-1 assertions + an IDF-beats-overlap case + negative cases) green; episodes persist in SQLite and survive restart (scenario A) | `memory::retrieval` tests + `storage` tests (in `evidence/phase3-gates.log`) |
| Observability | **PASS** — live collector captured the span tree for one run: `kelter.run` (1) → `kelter.step` (2) → `kelter.model_call` (2) / `kelter.validate` (2) / `kelter.tool_call` (1), OTLP/HTTP | `evidence/otel-trace-sample.log` |
| Per-run accounting | **PASS** — `GET /api/v1/runs/{id}` returns `usage: {prompt_tokens: 646, completion_tokens: 67, calls: 2}` for the OTel demo run | `evidence/otel-trace-sample.log` header / run output in session log |
| Quadlet harness | Present; static checks PASS on this host; **real `quadlet -dryrun` requires podman — printed as explicit human step (K12)** | `evidence/quadlet-harness.log` |
| No secrets / no push | key env-only; no push (human gate) | — |

### 4. Uncertainties / TODO for human review (Phase 3)

- **Quadlet real validation is a human act** on the podman host (K12):
  `scripts/validate-quadlet.sh` prints the exact commands.
- The exactly-once guarantee holds for tools that declare
  `idempotency_key`; third-party MCP tools without it get honest-failure
  resume, not exactly-once. Documented in ADR 0007 and `extensions/README.md`
  ecosystem docs should repeat it (currently in the ADR).
- Ledger dedupe scans the ledger file per call — O(n) per append; fine for
  the demo tool, not a general transactional store.
- SQLite retrieval loads all episodes and ranks in memory — the scale seam
  (term index / embeddings) is documented in code and architecture.md.
- Cost-in-dollars is not computed (token counts only) — model price tables
  change; pricing lookup would need a live catalog (OpenRouter returns
  pricing metadata; wiring it is a small follow-up).
- OTel: traces only; metrics/logs signals not wired. tracing-opentelemetry
  0.33 pairs with opentelemetry 0.32 — pin all four crates together on
  upgrades (ecosystem churns).
- Desktop standalone topology still uses in-memory stores (durable SQLite
  is wired in the daemon; pointing the desktop shell at the same storage
  crate is a small follow-up).

### 5. Version pins observed (Phase 3)

rusqlite 0.40.1 (bundled SQLite) · opentelemetry 0.32.0 /
opentelemetry_sdk 0.32.1 / opentelemetry-otlp 0.32.0 /
tracing-opentelemetry 0.33.0 · rmcp 2.2.0 (unchanged) ·
otel/opentelemetry-collector 0.156.0 (evidence capture).

## Phase 4a — capability registry + memory as an extension

Date: 2026-07-18 (fourth pass). Precondition verified: Phase 3 interfaces
present at HEAD (`MemoryProvider` + BM25 in `crates/memory/`, journal/store
+ ADR 0007, `crates/storage/`). Note: HEAD is `962e072` — the Phase 3
content with a different hash than the `e7dc335` recorded pre-push
(history was evidently amended when the maintainer pushed; parent chain
and tree content match, so the precondition is satisfied on substance).

### 1. Sources verified live (Phase 4a)

| Area | What was verified | How |
|---|---|---|
| Python MCP SDK | `mcp` still **1.28.1** on PyPI (requires ≥3.10) — unchanged from Phase 2; pin `<2` still correct | pypi.org JSON API |
| rmcp | still **2.2.0** on crates.io — unchanged | crates.io JSON API |

### 2. The key design decision: memory protocol = MCP reuse

Considered a dedicated memory protocol vs reusing MCP. **Chose MCP reuse**:
the provider contract (`write`/`retrieve`/`upsert`) maps 1:1 onto three
reserved MCP tools with typed schemas and structured results; MCP already
supplies transport, schemas and SDKs in every language. A bespoke protocol
would add surface without expressiveness. What MCP does NOT decide —
keeping memory tools out of the model's action space — is the registry's
job, not the protocol's (routing by declared capability). Full rationale
in ADR 0008.

### 3. Decision log (Phase 4a)

| Decision | Rationale |
|---|---|
| One capability per manifest; `agent`/`model-provider` reserved words that parse but refuse to load | Forward-compatible manifests with zero silent ignoring — the 4b/4c seams are pre-declared. |
| `[permissions]` with `deny_unknown_fields`; `workspace_write` gates the `KELTER_WORKSPACE` handover; `network` recorded+logged only | Enforce exactly what this slice can enforce; declare the rest honestly (transport-level sandboxing is staged, not simulated). |
| Kernel-side schema gate on `memory_retrieve` responses (shape + score ∈ [0,1]) | "Untrusted by construction" extended to memory: a hostile/broken backend cannot inject malformed context (proven by the rogue-backend test). Episodes are kernel-constructed and provenance-tagged before the backend ever sees them. |
| Built-in durable memory = bundled `memory-server` bin spawned via the registry seam | Dogfooding per ADR 0006 made literal: the daemon has NO in-process memory path when storage=sqlite; whatever the built-in can do, a third-party backend can. |
| External memory extension replaces built-in; >1 memory extension = startup error | Memory is a singleton backend; replacement must be deterministic and explicit. |
| `memstore` example placed in `examples/extensions/`, NOT auto-scanned `extensions/` | A toy backend silently replacing durable memory in every repo-root daemon would be a footgun; tools are additive (safe to auto-load), storage replacement is an explicit act. |
| Memory retrieval failures degrade to "no context" with a warning | Backend quality must never become run-integrity failure (graceful degradation applies to memory too). |

### 4. Definition-of-done evidence (Phase 4a)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — see `evidence/phase4a-gates.log` (**60 tests, 0 failures** incl. 6 registry tests + 2 memory-over-MCP integration tests) | `evidence/phase4a-gates.log` |
| Naming | codename grep = 0; `agentkernel` only in historical evidence + notes | `evidence/phase4a-naming-check.log` |
| Registry: capability + permissions + loud failures | PASS — unit tests cover unknown capability, declared-but-unimplemented (`agent`), unknown permission key, >1 capability; live daemon startup refused a `provides=["agent"]` manifest with an explicit error | `evidence/memory-extension-badmanifest.log` |
| Tool extensions regression | PASS — wordstats smoke green, unchanged behavior (now with explicit `workspace_write` permission) | session log; `scripts/smoke-extension.sh` |
| Memory as registered extension + dogfood | PASS — daemon log: `memory: builtin durable extension (dogfooded via the registry seam)`; durability smoke (restart survival incl. episodes) green **through the spawned memory-server**, no in-process path | `evidence/durability-restart.log` |
| Non-Rust memory proof | **PASS** — `memstore` (~30 lines Python) registered via manifest; real model run wrote its reflection episode into the Python backend's store; a second real run retrieved that context back (`memory: retrieved context from the registered provider`); zero kernel recompilation | `evidence/memory-extension-daemon.log`, `evidence/memory-extension-store.json` |
| Governance intact | PASS — rogue backend returning score=42 rejected by the kernel-side schema gate (integration test over real MCP duplex); tool-side validation/journal unchanged (durability + extension smokes) | `crates/mcp/tests/memory_ext_test.rs` (in gates log) |
| No secrets / no push | key env-only; commits local, push = human gate | — |

### 5. Uncertainties / TODO for human review (Phase 4a)

- `network` permission is recorded, not enforced at transport level —
  staged (ADR 0008 notes it); don't present it as sandboxing.
- Memory now crosses a process boundary (stdio MCP) — negligible at
  current call rates (one retrieve per run start, one write per
  reflection); a hot-path memory revisits via the WASM mechanism.
- The desktop standalone topology still uses in-process in-memory stores
  (unchanged Phase 3 note; the registry work here is daemon-path only, per
  the brief).
- The memstore example's overlap scoring is deliberately simple — the
  quality contract (BM25 tests) applies to the built-in backend; example
  docs say so.
- Uncommitted human edit to `docs/ROADMAP.md` was present in the working
  tree before this phase started — intentionally left unstaged for the
  maintainer.

### 6. Version pins observed (Phase 4a)

mcp (Python) 1.28.1 · rmcp 2.2.0 · rusqlite 0.40.1 — all unchanged from
Phase 3; no new dependencies added beyond wiring `jsonschema`/`storage`
into the `mcp` crate.

## Phase 4b — governed, durable outbound A2A

Date: 2026-07-18 (fifth pass). Precondition verified: 4a + Phase 3 on origin
(capability registry with reserved `agent` in `crates/mcp/src/extensions.rs`
+ ADR 0008; journal/durability + ADR 0007). Scope: OUTBOUND only (Kelter as
an A2A client delegating to remote agents); inbound + subprocess network
sandboxing explicitly deferred.

### 1. Sources verified live (Phase 4b)

| Area | What was verified | How |
|---|---|---|
| A2A protocol v1.0.0 | well-known path `/.well-known/agent-card.json`; card has `supportedInterfaces[]` (not top-level `url`) — the JSONRPC binding's `url` is the endpoint; **PascalCase** JSON-RPC methods `SendMessage`/`GetTask` (not slash-style); `SendMessage` returns a oneof Task|Message (JSON `result.task`/`result.message`, no `kind`); Task = `{id, contextId, status:{state}, artifacts[], history[]}`; states are full enum strings `TASK_STATE_*` (COMPLETED/FAILED/CANCELED/REJECTED terminal; INPUT/AUTH_REQUIRED interrupted); `GetTask{id, historyLength?}` returns the Task unwrapped (the reconnect primitive); result text at `task.artifacts[].parts[].text`; every request needs `A2A-Version` header; A2A errors `-32001..-32009` (TaskNotFoundError=-32001) | raw `specification.md` + `a2a.proto` from github.com/a2aproject/A2A main (the a2a-protocol.org rendered page was too large for reliable summarization — used canonical source) |
| A2A SDK landscape | official a2aproject Rust SDK published as `a2a-lf`/`a2a-client-lf` (0.x, released the day before — very new); the plain `a2a-rs` crate is an *unrelated* third party; Python official `a2a-sdk` 1.1.1 (Starlette/FastAPI optional) | crates.io + pypi JSON APIs + a2aproject repo list |

### 2. The two key design decisions

**A2A client = minimal own, not the SDK.** The JSON-RPC binding needs two
methods + card resolution; a hand-rolled `reqwest`+`serde` client (~470
lines incl. the action source) keeps the egress trivially wrappable in the
origin-pin/SSRF gate and keeps a day-old 0.x crate out of the trust path.
The official `a2a-client-lf` is noted as a future swap. The Python **toy
target** likewise skips the SDK (a ~90-line Starlette server) — it is the
delegation *target*, not Kelter-as-server (inbound is out of scope).

**Durable delegation = journaled task id + `GetTask` reconnect.** A new
`Note` journal entry records the remote task id via
`ExecutionContext::record_note` the instant the task is created (before
polling). Resume folds notes per step; if the id is present the client
reconnects with `GetTask` and never re-sends. This is the A2A analogue of
Phase 3's intent→receipt idempotency, across a process+network boundary.

### 3. Decision log (Phase 4b)

| Decision | Rationale |
|---|---|
| `agent` extension = remote endpoint (`[agent] card_url`), never spawned; `network` permission REQUIRED | An agent is dialed, not run; the kernel owns that egress, so `network` (only *recorded* in 4a) is now *enforced* here — an agent manifest without it is refused at load. |
| `ExecutionContext` handed to `ActionSource::execute` (trait change) | Gives an action source durable journaling (`record_note`/`note`) and per-agent attribution without leaking the store into every tool. Cross-cutting but mechanical; all existing sources updated, MCP tools ignore it. |
| `CompositeActionSource` merges MCP tools + A2A delegations | One action space, uniform kernel validation/journal/durability over local tools and remote delegations. |
| SSRF allowlist: pin every dial to the `card_url` origin; disable redirects; re-check on each RPC | The dial target must not be steerable by the model or a hostile Agent Card (e.g. cloud metadata IP). This is the enforceable case *because* the A2A client is the kernel's. |
| Reject unknown A2A task states as protocol violations | Same trust posture as the 4a memory schema gate: a lying agent cannot inject an unvalidated observation. |
| Interactive states (`input-required`/`auth-required`) → explicit step failure | Outbound delegation v1 is fire-and-collect; human-in-the-loop remote tasks are a later increment (declared, not faked). |
| Per-agent accounting on `run.agent_usage` | Extends Phase 3 per-run accounting; surfaced by the runs API. |
| Subprocess-extension egress still un-sandboxed — documented, not simulated | A2A egress is kernel-owned (enforced here); tool/memory subprocess egress needs container/WASM network isolation (staged). Keeping the honest ADR-0008 boundary, not regressing to pretending. |

### 4. Definition-of-done evidence (Phase 4b)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — **71 tests, 0 failures (25 suites)** incl. 8 A2A client/delegation tests + 3 new registry agent tests | `evidence/phase4b-gates.log` |
| Naming | codename grep = 0; `agentkernel` only in historical evidence + notes | `evidence/phase4b-naming-check.log` |
| Registry: agent = remote endpoint; no-`network` refused; tool/memory no regression | PASS — unit tests (agent loads with card_url+network; refused without network; needs `[agent]` not `[server]`); live startup refusal of a no-network agent; tool + memory smokes green | `evidence/a2a-network-refused.log`; `scripts/smoke-extension.sh`, `scripts/smoke-memory-extension.sh` |
| Outbound delegation e2e | PASS — a real 0.5B run used `delegate_to_toy`; the toy agent's answer (`SUMMARIZE KELTER`) returned as a validated observation journaled with agent identity; `run.agent_usage` = `{toy:{delegations:1, remote_ms:~6000}}` | `evidence/a2a-resume-run.json`, `evidence/a2a-durable.log` |
| **Durable A2A (money shot)** | **PASS** — delegation created a remote task (id journaled); daemon killed at `after_a2a_create`; resume log shows `reconnecting to existing remote task from the journal — NOT re-delegating`; run completed referencing the SAME task id — zero re-delegation | `evidence/a2a-durable.log`, `evidence/a2a-resume-run.json` |
| Network enforced | PASS — (a) agent without `network` → startup refused; (b) SSRF: a card advertising an endpoint on another host is refused (unit test `ssrf_refuses_endpoint_outside_declared_host`) | `evidence/a2a-network-refused.log`; `crates/a2a/tests/a2a_client_test.rs` |
| Governance: hostile response rejected; controlled degradation | PASS — unknown task state rejected (unit test); remote timeout/failure → explicit step failure (poll timeout + non-completed terminal states) | `crates/a2a/tests/a2a_client_test.rs` |
| No secrets / demo localhost-only / no push | PASS — toy agent binds 127.0.0.1; key env-only; commits local | — |

### 5. Uncertainties / TODO for human review (Phase 4b)

- **Subprocess-extension egress is NOT sandboxed** (tool/memory servers can
  make their own network calls) — the honest ADR-0008 boundary; the real
  fix (container `--network none` / WASM) is a separate staged increment.
  Only the kernel-owned A2A egress is enforced here.
- Inbound A2A (Kelter as an A2A server / Agent Card exposure) is out of
  scope — the toy server is a delegation target, not Kelter.
- Interactive remote tasks (`input-required`/`auth-required`) fail the step
  rather than pausing for human input — a later increment.
- A2A auth (the card's `securitySchemes`) is not exercised — the toy agent
  is unauthenticated localhost; a real agent needs credential handling
  (env-only) before use.
- The official `a2a-client-lf` crate (day-old 0.x) may be worth adopting
  once stable; the minimal client covers the two methods used.
- Poll timeout is a fixed 120s; long-running remote tasks would want
  push-notification (A2A `pushNotifications`) instead of polling.

### 6. Version pins observed (Phase 4b)

A2A protocol v1.0.0 (own minimal client; no A2A crate dependency) · reqwest
0.12 (unchanged) · rmcp 2.2.0 · rusqlite 0.40.1 — no new Rust dependencies.
Toy target: starlette + uvicorn (example venv, not a repo dependency).

## Phase 4c — plan mode (validated, durable DAG)

Date: 2026-07-18 (sixth pass). Precondition verified: Phase 3 + 4a + 4b on
origin (journal/durability ADR 0007; capability registry ADR 0008; A2A
ADR 0009). Plan mode composes those seams; no new external SDK.

### 1. The key design decision: the DAG journal model

A plan is a **memoized decision** — the Phase 3 replay law generalized from
a linear sequence to a graph. The validated plan is journaled once
(`PlanProposed`); on resume it is replayed from the journal and the model is
NOT re-queried for it. Each node reuses the existing per-action durable
entries (`Intent`/`Receipt`/`Note`) keyed by the node's index, so a
completed node (receipt present) is skipped and a dangling intent
re-dispatches with the journaled idempotency key. Review-node model calls
are memoized as their receipts. Result: the model is consulted at most once
for the plan plus once per review node; tool/a2a nodes need no model call;
resume is fully deterministic. Full rationale in ADR 0010.

### 2. Decision log (Phase 4c)

| Decision | Rationale |
|---|---|
| Plan = one `propose_plan` action (schema-constrained), not a new action surface | The brief: plan composes existing seams. The plan DAG is the arguments of a single proposed action; GBNF constrains its structure. |
| Node execution reuses the step-mode durable machinery keyed by node index | Zero new durability code path: idempotency, crash points, A2A reconnect and per-agent accounting all apply to plan nodes unchanged. |
| `PlanProposed` is the only new journal entry | Minimal extension of ADR 0007; nodes reuse Intent/Receipt/Note. |
| Node types `tool`/`a2a`/`review`; `a2a` node = a `delegate_to_*` action | A2A nodes inherit Phase 4b egress governance for free (they ARE actions in the composite space). `review` is kernel-owned (approve/reject over upstream outputs), memoized. |
| Plan validation (cycles via Kahn topo-sort, type/reference checks) runs BEFORE the plan is journaled and before any node executes | Invalid plan → run fails with no side effect and no journaled plan (governance gate, like the 4a memory schema gate). |
| Fail-on-node-error; adaptive re-planning deferred | Per the phase's failure guidance — never ship an uncertain re-plan that pretends to work. Node failure fails the plan explicitly. |
| Plan node `arguments` constrained to a shallow string→string map under GBNF | **Empirical finding**: a generic recursive-object grammar for `arguments` makes constrained generation pathologically slow on a 0.5B (single proposal hung >180s). Bounding it to a string map made generation fast; the kernel re-validates each node's arguments against the real tool schema at execution. Tools needing non-string args in a plan are a documented follow-up. |

### 3. Definition-of-done evidence (Phase 4c)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — **85 tests, 0 failures (26 suites)** incl. 8 plan-validation unit tests + 5 plan-execution/durability integration tests | `evidence/phase4c-gates.log` |
| Naming | codename grep = 0; `agentkernel` only in historical evidence + notes | `evidence/phase4c-naming-check.log` |
| Plan validated pre-execution; invalid plan rejected | PASS live — 6/6 model-proposed plans with a self-referential dependency were rejected as cycles **before any node ran** (no side effect); the reject-before-execution gate is also unit-tested for cycle/unknown-action/type-mismatch/unknown-dep/dup-id/empty | `evidence/plan-validation.log`, `crates/kernel-core/src/plan.rs` tests |
| **Durable plan (money shot)** | **PASS (deterministic)** — `resume_replays_plan_and_skips_completed_nodes`: a run whose journal holds `PlanProposed` + node-0 receipt resumes, does NOT re-run node 0, and calls the model exactly once (the review verdict) — proving the plan is replayed without re-querying and completed nodes are not repeated. Stronger than a live 0.5B run (see §note). | `crates/kernel-core/tests/plan_test.rs` (in gates log) |
| Composition tool+a2a+review, each node validated + journaled | PASS (deterministic) — `plan_runs_all_node_types_in_order`: a DAG with a `record` tool node, a `delegate_to_toy` a2a node and a `review` node executes in topological order, each through its seam; review-reject fails the plan (`review_reject_fails_the_plan`) | `crates/kernel-core/tests/plan_test.rs` |
| Governance not bypassed per node | PASS — nodes go through kernel-side schema validation + journal at execution (same path as step mode); a2a node inherits 4b egress governance (it is a composite-space action) | code + a2a smoke |
| No secrets / demo localhost-only / no push | PASS | — |

**Note on the live money shot.** Live plan *proposal* on the 0.5B is
slow/unreliable: the small model rambles under the constrained plan grammar
or self-references node ids (consistently producing a cycle the kernel
rejects). That is a model-capability limit, not a kernel one — so the
durable-execution money shot is proven by the deterministic harness
(authoritative), while the live smoke proves the reliable-live part: the
kernel validating and rejecting bad plans before execution.

### 4. Uncertainties / TODO for human review (Phase 4c)

- **Live durable-plan execution on a 0.5B is not smoke-able reliably**
  (proposal is slow/self-cyclic). Proven deterministically instead. A
  frontier model would execute plans reliably live, but Phase 4c's tool
  access is localhost-only (no OpenRouter for the demo), so it wasn't used.
- Plan node `arguments` are string-valued under constrained decoding;
  tools needing typed (int/bool/nested) args in a plan need the follow-up
  noted in ADR 0010 (kernel validation already rejects wrong types).
- Adaptive re-planning on node failure is deferred (fail-on-node-error
  ships); this is the largest scoped-out piece.
- A GBNF generator bug was found and fixed en route (enum/alternation
  values inlined into object/array sequences weren't parenthesized, so `|`
  escaped the rule) — this also hardens step-mode grammars for any enum
  argument.
- Review nodes use the policy oracle for an approve/reject verdict; a richer
  review (structured critique, partial acceptance) is future work.

### 5. Version pins observed (Phase 4c)

No new dependencies. rmcp 2.2.0 · rusqlite 0.40.1 · reqwest 0.12 —
unchanged. Plan mode is entirely in `kernel-core` (+ config plumbing).

## Phase 5A — WASM extension mechanism (in-process, capability-sandboxed)

Date: 2026-07-18/19. Precondition verified: Phase 3 + 4a + 4b on origin
(registry ADR 0008 — WASM is a new mechanism in it; egress deferral ADR 0009
— this closes it for WASM; journal/governance Phase 3). 4c is orthogonal
(WASM tools also work as plan nodes, for free).

### 1. Sources verified live (Phase 5A)

| Area | What was verified | How |
|---|---|---|
| wasmtime | latest stable **46.0.1**; core embedding API (`Engine`/`Module`/`Store`/`Linker::func_wrap`/`Instance::get_typed_func`/`Memory::write`+`data`), fuel bounding (`Config::consume_fuel` + `Store::set_fuel`); a core module with unsatisfied imports has NO ambient I/O — the host grants capabilities only by defining host functions; pure-compute needs no `wasmtime-wasi` | crates.io JSON + docs.rs; **then compiled and run against 46.0.1 in this repo** |
| WASI preview 2 / component model | wasmtime's stability-tiers page now positions the component model / WASIp2 as **Tier 1** (production-supported), not experimental (WebFetch-summarized — flagged for a human double-check). Kept OUT of the trust path anyway — for *simplicity* (no `wit`/`wasi-bindgen` toolchain, minimal surface), not instability | crates.io + docs.wasmtime.dev (security, stability-tiers) |

### 2. The key design decision: host↔guest interface

**Core WASM + a minimal byte ABI, not the component model / WASI preview 2.**
The guest exports `alloc`/`run`/`memory`; the host passes the input string
through linear memory and reads a JSON result. Rationale: the ABI is tiny,
stable across wasmtime releases, and easy to keep in the trust path — the
same dep-light / stability-over-richness call as the A2A client in 4b. A
pure-compute module needs no WASI at all. Full rationale in ADR 0011.

### 3. Decision log (Phase 5A)

| Decision | Rationale |
|---|---|
| `mechanism = "wasm"` in the manifest, routed by the registry to a WASM host (not a subprocess) | Extends ADR 0008 with a second mechanism without a new capability axis; `tools`-only in this slice (memory/agent/model-provider with wasm fail loudly). |
| Capability sandbox = absence of host imports (structural) | A core module has no ambient authority; the host defines a capability's host function ONLY if granted. Ungranted → the module can't be instantiated (unknown import). This is *structural*, not a runtime check — exactly the property the whole thesis needs. |
| Default grants: nothing (no network, no disk) | Least authority by default; a manifest opts in via `[permissions]`. |
| Fresh `Store`/instance per call + fuel bound | Stateless, deterministic guest execution; fuel caps runaway loops. |
| Shallow tool schema (single `input` string → JSON) | Same constrained-decoding reason as ADR 0010 (a deep object grammar is pathologically slow on a 0.5B). |
| wasmtime features: `cranelift` + `runtime` only (no `gc`, no `wasi`) | Minimal surface for pure-compute core modules; `gc` requires a collector feature and isn't needed. |
| Demo `.wasm` checked in + `scripts/build-wasm-demo.sh` | Reproducible from source (`examples/wasm-tools/*`, own workspaces so `cargo build --workspace` never compiles wasm with the host toolchain); the small binaries are committed for a zero-setup smoke. |
| Daemon action space made source-agnostic | MCP source is now optional; WASM-only (or A2A-only) deployments work (`no action sources` fails loudly). |

### 4. Definition-of-done evidence (Phase 5A)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt (incl. `.wasm` build) | PASS — **92 tests, 0 failures (28 suites)** incl. 3 wasmhost tests + 4 registry wasm-routing tests; demo modules build to `wasm32-unknown-unknown` | `evidence/phase5a-gates.log` |
| Naming | codename grep = 0; `agentkernel` only in historical evidence + notes | `evidence/phase5a-naming-check.log` |
| WASM mechanism routing; ungranted capability refused; MCP no regression | PASS — `mechanism = "wasm"` routes to the WASM host (registry test + live registration log); memory/agent+wasm and unknown mechanism fail loudly (unit tests); tool/memory/a2a subprocess smokes green | `evidence/wasm-registration.log`; `scripts/smoke-extension.sh`, `smoke-a2a.sh`, `smoke-memory-extension.sh` |
| **WASM tool runs in-process** | **PASS** — `word_count` (Rust→WASM) registered `mechanism = "wasm"`, appeared in `GET /api/v1/tools`, and was used by the local 0.5B in a real run returning `{words, chars, lines}` — no subprocess spawned | `evidence/wasm-tool-run.json`, `evidence/wasm-registration.log` |
| **Sandbox money shot** | **PASS** — a WASM module importing `kelter:net` with NO network grant is **structurally blocked**: wasmtime refuses to instantiate it (`unknown import`), the step fails, no network touched; the unit tests also show the SAME module instantiates WHEN the grant is present (proving the gate is the only thing between the module and the capability) | `evidence/wasm-sandbox-run.json`, `crates/wasmhost/src/lib.rs` tests |
| Governance | PASS — the WASM tool's output flows into the run through kernel-side schema validation like any tool; provenance/journal unchanged | code + wasm-tool run |
| No secrets / no push | PASS | — |

### 5. Uncertainties / TODO for human review (Phase 5A)

- **Honest boundary (unchanged and important):** WASM sandboxes the WASM
  mechanism only. Subprocess (MCP) tool/memory/agent extensions STILL have
  OS-level egress — WASM is a sandboxed *alternative*, not a fix for the
  subprocess mechanism. Do not read this as "subprocess egress is now
  closed".
- The granted `kelter:net` host function is a STUB (returns a sentinel) —
  the point of this slice is the *gate*, not a real network implementation.
  A real network capability behind the gate is follow-up work.
- WASM is `tools`-only here; WASM memory/agent/policy are deferred.
- The byte ABI takes a single string input and returns JSON; richer typed
  I/O (or the component model) is deferred.
- The demo `.wasm` is checked in; a supply-chain-conscious deployment would
  build from source in CI (the `build-wasm-demo.sh` path) and verify.
- No `wasmtime-wasi` dependency (pure compute); adding filesystem/clock
  capabilities later would introduce WASI (preview 1 or, when mature,
  preview 2) behind the same grant model.

### 6. Version pins observed (Phase 5A)

wasmtime 46.0.1 (features `cranelift`, `runtime`) · rustc 1.95 with target
`wasm32-unknown-unknown` · rmcp 2.2.0 / rusqlite 0.40.1 / reqwest 0.12
unchanged.

## Phase 6 — extension trust posture (sandbox-by-default + two-part opt-in)

Date: 2026-07-18/19. Implements ADR 0012. Precondition verified: Phase 5A
(WASM containment) + 4b (A2A egress governance) + 3 (journal/durability) on
origin. This phase adds a registry trust policy on top of ADR 0008; it changes
no mechanism — it decides, loudly and attributably, *whether* a subprocess
mechanism is allowed to run unsandboxed.

### 1. The design decision: where the two halves of the opt-in live

The crux of ADR 0012 is that a manifest is untrusted input, so it cannot
authorise its own escape from the sandbox-by-default posture. The opt-in is
therefore split across the trust boundary, and the two halves live in different
code and different files:

- **Manifest half — the registry** (`crates/mcp/src/extensions.rs`). A `tools`
  subprocess must declare `[extension] trust = "subprocess-unsandboxed"`. The
  registry resolves a `TrustMode` per extension: a WASM tool is
  `wasm-sandboxed` by construction (a subprocess `trust` on it is an error); a
  subprocess with no declaration, a subprocess claiming `wasm-sandboxed`, or an
  unknown value are each **refused loudly** (`TrustNotDeclared`,
  `WasmTrustMismatch`, `UnknownTrust`) — the same forward-compatible refusal
  style used for unknown capabilities/mechanisms.
- **Operator half — the daemon** (`crates/daemon/src/config.rs` +
  `main.rs`). A declared subprocess-unsandboxed extension loads only if the
  operator lists it in `[trust] allow_unsandboxed_subprocess`. Missing consent
  → the daemon refuses to start, naming the extension and the exact key to add,
  and stating that allowing it does **not** sandbox the subprocess.

Neither half alone lets an unsandboxed subprocess run — that is the money shot
(legs B/C of the smoke).

### 2. Decision log (Phase 6)

| Decision | Rationale |
|---|---|
| Trust mode is a manifest field (`trust`), resolved to a `TrustMode` enum, not inferred | Explicit and forward-compatible; an unknown value fails loudly rather than silently downgrading. |
| Operator consent is a *separate* daemon-config list, never the manifest alone | The manifest is untrusted input (ADR 0006); a two-key gate is the only way "explicit opt-in" is meaningful. A manifest cannot authorise its own escape. |
| WASM tools need no opt-in (`wasm-sandboxed` by construction) | Sandbox-by-default: the safe mechanism is the silent default; only the unsafe one is gated. No regression for Phase 5A tools. |
| Never label an allowed subprocess a "sandbox"/"containment" anywhere | ADR 0012 honest-labelling hard rule. The startup warning and the label say "subprocess-unsandboxed (operator-trusted)"; allowing it is explicitly not containment (it keeps OS egress, ADR 0009). |
| Per-run trust posture journaled once (`JournalEntry::TrustPosture`) + exposed on `Run.trust_posture` | Transparency/provenance: "what ran unsandboxed" is durably answerable via journal and API. Journaled once and replayed on resume (replay law), like the plan and the model decision. |
| Existing subprocess manifests (wordstats, memstore, toy-agent) migrated to declare `trust`; smoke/bootstrap configs gained the operator allow-list | Migration without weakening policy: the ecosystem keeps working, but now as an explicit, attributable decision. |

### 3. Definition-of-done evidence (Phase 6)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — **97 tests, 0 failures (28 suites)**, incl. 5 new registry trust tests; clippy `-D warnings` clean; fmt clean | `evidence/phase6-gates.log` |
| Naming | codename grep = 0 over the whole tracked tree; `agentkernel` only in historical evidence + notes describing them | `evidence/phase6-naming-check.log` |
| **A — sandbox-by-default** | **PASS** — a subprocess tool with NO `trust` declaration → the daemon refuses to start (`did not declare a trust mode`) | `evidence/trust-refuse-no-declaration.log` |
| **B — manifest cannot self-authorise (money shot pt 1)** | **PASS** — manifest opts in (`trust = "subprocess-unsandboxed"`) but the operator has NOT allowed it → still refused (`operator has not allowed it`) | `evidence/trust-refuse-no-operator.log` |
| **C — two-part opt-in loads (money shot pt 2)** | **PASS** — manifest opt-in AND operator consent → loads, recorded `trust=SubprocessUnsandboxed`, honest "operator-trusted subprocess (unsandboxed)" startup warning (never "sandbox") | `evidence/trust-loaded-with-consent.log` |
| **D — WASM sandbox-by-default + posture journaled** | **PASS** — a WASM tool loads with NO opt-in (`trust=WasmSandboxed`); a real 0.5B run exposed `Run.trust_posture = [["wordcount-wasm","wasm-sandboxed"]]` via the API AND journaled a `TrustPosture` entry (durably attributable) | `evidence/trust-run.json` |
| No regression | PASS — tool/memory/a2a/wasm/durability/plan smokes all green after migration (wordstats & memstore now opt-in; toy-agent unaffected) | `scripts/smoke-extension.sh`, `smoke-memory-extension.sh`, `smoke-a2a.sh`, `smoke-wasm.sh`, `smoke-durability.sh`, `smoke-plan.sh` |
| No secrets / no push | PASS | — |

### 4. Uncertainties / TODO for human review (Phase 6)

- **This is governance, not containment (the ADR 0012 honest boundary).**
  Allowing a subprocess-unsandboxed extension does NOT sandbox it — it keeps
  full OS-level network/disk egress (ADR 0009). The gate makes the choice
  explicit, operator-owned and auditable; it does not retroactively contain the
  subprocess. Real containment for a subprocess still needs an OS layer
  (network-restricted namespace/container), which remains deferred — WASM
  sidesteps it by not being a subprocess.
- The operator allow-list is by extension *name*; a deployment that pins by
  content hash / signature is stronger and is follow-up work.
- No audited capability gateway/broker is built here — ADR 0012 explicitly
  reserves the right to build one later for its governance value, but forbids
  ever calling it a sandbox. Nothing in this phase does.

### 5. Version pins observed (Phase 6)

No new dependencies. serde/toml (config), rmcp 2.2.0, rusqlite 0.40.1,
wasmtime 46.0.1 — all unchanged. The trust policy is pure kernel/registry/config
logic plus one new journal variant.

## Phase 7 — A2A inbound (Kelter as an A2A server)

Date: 2026-07-19. Implements ADR 0013. Precondition verified: Phase 3
(durability — an inbound task maps onto a durable run) + 4b (crate `a2a` +
outbound client, reused for the Kelter↔Kelter loop) on origin; the bearer-auth
API (Phase 1/2) that the inbound endpoint is mounted on. This phase completes
A2A in both directions.

### 1. Sources verified live (Phase 7)

| Area | What was verified | How |
|---|---|---|
| A2A server spec (v1.0.x) | Agent Card served at `/.well-known/agent-card.json`; top-level fields incl. `interfaces`/`supportedInterfaces`, and the **security declaration** (`securitySchemes` map of OpenAPI-style schemes + `security` array marking them required); the canonical Task state enum (`TASK_STATE_*`). JSON-RPC core methods are the **proto-derived names `SendMessage`/`GetTask`/`CancelTask`** in the v1.0 spec (the pre-1.0 JSON-RPC spellings `message/send`/`tasks/get`/`tasks/cancel` are the older generation) | `a2a-protocol.org/latest/specification/` (WebFetch, §4.x data structures, §9.4 core methods, §14.3 discovery) |
| Interop naming reality | Kelter's own 4b outbound client + toy target use `SendMessage`/`GetTask` and read `supportedInterfaces` | `crates/a2a/src/lib.rs`, `examples/a2a-agent/server.py` |

Design response to the naming ambiguity: the inbound server **accepts both
generations** of method name, and the Agent Card emits **both** `interfaces`
(v1.0) and `supportedInterfaces` (what the 4b client reads) with identical
content — so a v1.0 client, a pre-1.0 client, and Kelter's own 4b client all
interop. The conformance client uses the v1.0 `SendMessage`.

### 2. The design decision: an inbound task IS a durable run

The A2A `task.id` is the Kelter `run_id`. `message/send` → `create_run` (durable
from creation) + spawn `execute_run`; `tasks/get` → read the run store by id →
map status to the A2A task state. **There is no separate task store**, so
`tasks/get` is journal-backed for free and a server crash mid-task is handled
by the *existing* Phase 3 resume path (`unfinished_runs` → `execute_run` replays
the journal, exactly-once). This is the crux of ADR 0013 — durability for the
remote delegate is inherited, not rebuilt. (Escalation path if this had been
impossible: an inbound task that dies *explicitly* on crash rather than faking
continuity. Not needed.)

### 3. Decision log (Phase 7)

| Decision | Rationale |
|---|---|
| `task.id == run_id`; no separate task store | `tasks/get` reads the durable run store → crash-durability + journal-backing for free (reuses ADR 0007/Phase 3). |
| Server wire helpers in the existing `a2a` crate (`a2a::server`), HTTP surface in the existing `api` crate | Reuse the 4b types/machinery; no new server, port, or auth mechanism. Transport-agnostic helpers stay unit-testable. |
| Endpoint requires the API bearer token; only card discovery is public | Auth is the security surface (ADR 0013): an anonymous task-executing server is an RCE footgun. A client must still be able to discover the scheme, so the card is public and declares it. |
| Caller attributed via a `Note` journal entry at `message/send` time | The inbound mirror of 4b's per-agent outbound accounting — "who delegated, over which authenticated surface" is durable. |
| Accept both `SendMessage`/… and `message/send`/… ; card emits `interfaces` + `supportedInterfaces` | Interop across A2A naming generations + Kelter's own 4b client, with no client change. |
| New terminal `RunStatus::Canceled` + cooperative `Kernel::cancel_run` | `tasks/cancel` needs a distinct terminal state (the spec separates CANCELED from FAILED). Cancel aborts the in-flight handle then writes `Terminal{Canceled}` durably; `unfinished_runs` excludes it; terminal runs are a no-op. |
| Optional per-agent bearer token on the 4b outbound client via `[agent] token_env` | Needed for the Kelter↔Kelter loop (B requires auth). Secrets env-only — the manifest names the env var, never the token. Open endpoints (toy target) unaffected. |

### 4. Definition-of-done evidence (Phase 7)

| Gate | Result | Raw log |
|---|---|---|
| build/test/clippy/fmt | PASS — **109 tests, 0 failures (28 suites)**, incl. 6 new `a2a::server` unit tests, 4 new api A2A integration tests, 2 new kernel cancel tests; clippy `-D warnings` clean; fmt clean | `evidence/phase7-gates.log` |
| Naming | codename grep = 0 over the whole tracked tree; `agentkernel` only in historical evidence + notes | `evidence/phase7-naming-check.log` |
| **Agent Card + methods** | **PASS** — card resolves at `/.well-known/agent-card.json`, names Kelter, declares the bearer scheme, advertises the `/a2a/rpc` JSON-RPC interface; `message/send`→task, `tasks/get`→status, `tasks/cancel`→CANCELED all shown | `evidence/a2a-inbound-agent-card.json`, `scripts/smoke-a2a-inbound.sh` |
| **Durable inbound (money shot)** | **PASS** — inbound task crashed mid-run (`KELTER_CRASH_POINT=after_execute:append_ledger`); on restart the run auto-resumed (**"model NOT queried"**, keyed re-dispatch), `tasks/get` by the same id resolved to COMPLETED, and the ledger had exactly **1** line (no double execution) | `evidence/a2a-inbound-durable-resume.log` |
| **Auth** | **PASS** — anonymous `message/send` refused with `401`; a bearer call accepted; caller attributed in the journal (`smoke-tester`, and `kelter-a2a-client` for the loop) | `evidence/a2a-inbound-attribution.log` |
| **Interop loop (money shot)** | **PASS** — instance A delegated to instance B's inbound endpoint (authenticated, via 4b outbound + `token_env`); B ran it governed+durable and returned; A completed with a `delegate_to_kelter_b` step | `evidence/a2a-inbound-loop-run.json` |
| **Conformance** | **PASS** — a generic A2A client (`examples/a2a-generic-client.py`, urllib only, no SDK, no Kelter code) resolved the well-known card and drove a task to `TASK_STATE_COMPLETED` over JSON-RPC — real A2A, not a private protocol | `evidence/a2a-inbound-generic-client.log` |
| Governance | PASS — the inbound task text is untrusted input run through the same kernel validation/journal as any run; no bypass path | code (`api::a2a_send` → `kernel.create_run`/`execute_run`) |
| No secrets / no push / localhost-only | PASS — token via env (`token_env`); demo binds 127.0.0.1 only | — |

### 5. Uncertainties / TODO for human review (Phase 7)

- **Scope kept narrow on purpose:** `message/send` + `tasks/get` + `tasks/cancel`
  + auth. Deferred (honestly, not simulated): SSE streaming (`message/stream`),
  push notifications/webhooks, per-tool A2A skills, plan-mode over A2A, and
  interactive inbound states (`input-required`) — an inbound task is
  fire-and-durably-complete.
- **`tasks/cancel` is cooperative + best-effort.** It aborts the in-flight
  handle this process owns, then writes the durable canceled state. A run
  resumed after a crash (handle not held) is not force-abortable and a cancel
  may arrive too late — A2A permits this. The durable Canceled write itself is
  reliable (unit-tested).
- **Auth is a single shared bearer token** (the API token), not per-caller
  principals. The caller *label* (`A2A-Caller` header) is attribution, not
  authentication — a stronger deployment would issue per-caller tokens/mTLS.
- **Conformance client is hand-rolled** (urllib, spec-following) rather than the
  `a2a-sdk`, to stay dep-light and offline — the same posture as ADR 0009's
  hand-rolled client. A future check against `a2a-sdk` is a valid add.
- The Kelter↔Kelter loop's A-side delegation is model-driven (0.5B); the smoke
  hard-gates on B receiving the authenticated delegation and logs whether A's
  small model emitted the delegate step that run.

### 6. Version pins observed (Phase 7)

No new dependencies. The inbound server is `axum` (already in `api`) + the
existing `a2a`/`kernel-core` crates; the generic client is Python stdlib
(`urllib`). rmcp 2.2.0 / rusqlite 0.40.1 / reqwest 0.12 unchanged.

## Phase 8 — production-hardening sweep

Date: 2026-07-19. Implements ADR 0014 — a SWEEP of independent items (each with
its own DoD + evidence), closing honest deferrals from the Phase 3
"uncertainties" list. Precondition: Phase 3 + 4a/4b/4c/5A/6/7 on origin.

### 1. Sources verified live (Phase 8)

| Area | What was verified | How |
|---|---|---|
| OTel Rust 0.32 (metrics + logs) | `MetricExporter`/`SdkMeterProvider` (feature `metrics`), `LogExporter`/`SdkLoggerProvider` (feature `logs`), `opentelemetry-appender-tracing` 0.32 pairs with 0.32; the metrics PeriodicReader/batch processors run off-runtime so a **blocking** OTLP client is required | context7 + crates.io; then compiled and exported live to a local OTLP sink |
| llama.cpp embeddings | `POST /v1/embeddings` → `{data:[{index,embedding[]}]}`, L2-normalized; a `--embedding` server is embedding-only and needs a **dedicated** model (separate from the generation server) | llama.cpp server docs (context7) |
| Tauri v2 bundle/signing | `bundle.targets` + `bundle.macOS.signingIdentity`/`bundle.windows.certificateThumbprint` hooks; universal via the CLI `--target universal-apple-darwin` | tauri.app schema; built with tauri-cli 2.11.4 |

### 2. Definition-of-done evidence (per item)

| Item | Result | Raw evidence |
|---|---|---|
| build/test/clippy/fmt | PASS — **121 workspace tests + 1 desktop test, 0 failures**; clippy `-D warnings` clean; fmt clean | `evidence/phase8-gates.log` |
| Naming | codename grep = 0 over the tracked tree; `agentkernel` only in historical evidence + notes | `evidence/phase8-naming-check.log` |
| **A — desktop durable store** | **PASS** — the standalone shell builds `SqliteRunStore` + `SqliteMemoryProvider` on one WAL db in the app data dir and resumes on launch; a run + episode written in one "session" is still present after the store is dropped and reopened ("restart") | `apps/desktop/src-tauri/tests/durable_store.rs` (passes: "restart-survival OK … persisted across reopen") |
| **B — dollar cost** | **PASS** — a real run exposes `cost_usd` on the API: `0.0` for a priced local model (642+65 tokens, 2 calls); unit tests show a real \$ figure (sonnet 1M+1M = \$18.00) and `null` for an unpriced model (never fabricated) | `evidence/cost-run-known.json`; `crates/kernel-core/src/cost.rs` + engine tests |
| **C — OTel metrics + logs** | **PASS** — a local OTLP sink captured all three pillars from a live run: metrics (`kelter.runs.started/finished`, `steps.executed`, `tool_calls`, `run.latency`), logs (log bodies), traces (2981 bytes). Off by default | `evidence/otel-metrics-logs-sample.log` |
| **D — embeddings switch** | **PASS** — `Retrieval::{Bm25\|Embeddings}` switch; the embedding path is a **real** `/v1/embeddings` client, exercised over actual HTTP against a mock server and ranking a lexically-disjoint doc first (BM25 would miss it); BM25 default retained; backend error falls back to BM25. Live comparison vs a dedicated embedding server documented (needs a separate server) | `crates/memory/tests/embeddings_switch.rs` (3 tests), `crates/memory/src/embeddings.rs` |
| **E — ledger dedupe scaling** | **PASS** — O(n)-per-append full-file scan replaced by an in-memory per-ledger key index (lazy load once → O(1) lookup); a fresh index still dedupes a key that only exists in the file (restart survival); durability smoke (exactly-once) green | `crates/mcp/src/tools.rs` + tests; `scripts/smoke-durability.sh` |
| **F — auth hardening** | **PASS** — bearer token from `KELTER_TOKEN` env (secrets out of the tree); failed-auth rate limiter → `429` after repeated failures (test). Scope documented: TLS/mTLS, per-caller principals, WS-auth deferred | `crates/api` (`AuthLimiter`) + api test `repeated_bad_auth_is_rate_limited` |
| **G — packaging (unsigned)** | **PASS** — `tauri.conf.json` declares targets + signing hooks (unsigned); `scripts/package-desktop.sh` built an UNSIGNED **`Kelter.app`** (23 MB, release) — product name correctly `Kelter`. The `.dmg` step then failed on `bundle_dmg.sh` (needs a macOS GUI/Finder session — the known headless limitation), so `.dmg` is built from a desktop session or a UI CI runner. Signing/notarization + K12 deploy documented as human steps | `evidence/dmg-unsigned-build.log`; `docs/packaging.md` |
| No secrets / no push / **nothing signed** / no K12 deploy | PASS | — |

### 3. Cruxes / decisions (Phase 8)

- **B honesty:** local = \$0 (no API bill), unknown model = `null`. Prices are
  static, dated config — no live fetch.
- **C robustness:** blocking OTLP client (the reader/batch threads have no tokio
  reactor — an async client panics, observed and fixed); bounded (3s) shutdown
  flush so a dead collector never hangs Ctrl-C.
- **D honest boundary:** the embedding backend is real code (HTTP-tested), but a
  live run needs a **separate** llama.cpp `--embedding` server with a dedicated
  model — that end-to-end comparison is the piece left for a deployment that
  runs one; the switch, adapter, BM25 default and graceful fallback are all
  proven here. Not faked.
- **E exactly-once preserved:** the file line stays the durable marker; the
  index is a rebuilt-from-file cache, so a crash reloads the key.

### 4. Uncertainties / TODO for human review (Phase 8)

- **Human-blocked (documented, not done):** real code-signing / notarization
  (needs an Apple Developer ID cert — paid, human-held) and K12 deploy (hardware
  pending). See `docs/packaging.md`.
- Headless `.dmg`: creation scripts Finder and needs a macOS GUI session; build
  `.dmg` from a desktop session (or `--bundles app` in CI).
- Embeddings: a live retrieval-quality comparison vs BM25 needs a running
  dedicated embedding server; the adapter is real + HTTP-tested meanwhile.
- Auth: TLS/mTLS + per-caller tokens + WS-endpoint rate-limiting are the next
  scope, deliberately not attempted here.

### 5. Version pins observed (Phase 8)

opentelemetry / opentelemetry_sdk / opentelemetry-otlp 0.32 (features `metrics`,
`logs`; **blocking** reqwest client) · opentelemetry-appender-tracing 0.32 ·
tauri 2.11.5 / tauri-cli 2.11.4 · rusqlite 0.40.1 · reqwest 0.12 (now also a
`memory` dep for the embedding client) — otherwise unchanged.

## Phase 8.5 — runtime model config + UI onboarding

Date: 2026-07-19. Implements ADR 0015 — choose + configure the model from the
UI (no `config.toml` edit). Precondition: Phases 1–3 + Phase 8 (this builds on
Phase 8's `config.rs`).

### 1. Sources verified live (Phase 8.5)

| Area | What was verified | How |
|---|---|---|
| Health endpoints | llama.cpp `/health` → `{"status":"ok"}`; OpenRouter `GET /api/v1/models` public + reachable; ollama `/api/version` (docs — not running here) | live `curl` to :8080 / openrouter.ai; ollama per docs |
| Policy seam | `build_oracle`, `LOCAL_DEFAULT_BASE_URL` = `:8080`, key from `OPENROUTER_API_KEY`; kernel holds `Arc<dyn PolicyOracle>` (immutable) | `crates/policy/src/lib.rs` |

### 2. Cruxes / decisions (Phase 8.5)

- **Runtime-mutable policy:** the kernel now holds a `policy::SwappableOracle`
  (an `RwLock<Arc<dyn PolicyOracle>>` wrapper implementing `PolicyOracle`), so
  the kernel is **unchanged** while the daemon swaps the inner adapter on a
  config change. A change applies live (`swap`) **and** persists to
  `model-override.toml` beside the config, applied over `[policy]` on startup →
  restart-survival. Honest scope: `latitude` is fixed at build time; a
  local↔frontier change re-derives it only on the next restart (the overlay
  makes that correct) — a deliberate small scope, not a faked hot-reload.
- **Key handling:** env-guided primary + a `0600`, gitignored secret file
  fallback. `PolicyConfig.api_key` is `#[serde(skip)]` so a key can never come
  from `config.toml`; the daemon injects env-or-file at build time. The key is
  never returned by the API, never logged (only "stored, at <path>").

### 3. Definition-of-done evidence (Phase 8.5)

| Gate | Result | Raw evidence |
|---|---|---|
| build/test/clippy/fmt | PASS — **122 workspace tests, 0 failures**; clippy `-D warnings` clean; fmt clean | `evidence/phase85-gates.log` |
| Naming | codename grep = 0 over the tracked tree; **no API key anywhere in the tree** | `evidence/phase85-naming-check.log` |
| **Config API + runtime-mutable** | **PASS** — `POST /config/model` swaps the model live (a subsequent run's `policy` = the new engine) AND persists `model-override.toml`; a daemon **restart keeps the UI-chosen model** ("applying persisted model overlay" in the log), no `config.toml` edit | `evidence/config-ux-set-persist-restart.log` |
| **UI config view** | **PASS** — the daemon serves the config screen: `id="config"`, provider/preset pickers, "Test connection", Save; `app.js` calls `/api/v1/config/model{,/test,/key}` (6×); presets pre-fill `:8080` / `:11434` / OpenRouter | `evidence/config-ux-ui.html` |
| **Health-check (real)** | **PASS** — `POST /config/model/test` to `:8080` → `ok=true`; to a dead `:59999` → `ok=false` (a real GET, never faked) | `config-ux` smoke leg 2 |
| **First-run routing** | **PASS** — on connect the UI health-checks the current model; unreachable/absent → shows the config screen and hides the run view (`routeByModelHealth` in `app.js`) | `apps/web-ui/app.js` |
| **Key handling (no leak)** | **PASS** — key stored in a `0600` gitignored secret file; asserted **absent** from `config.toml`, from every daemon log, and from the API response; `GET` returns only `api_key_set` | `config-ux` smoke leg 5 |
| Governance | PASS — every config endpoint bearer-gated (anonymous → 401, via the same gate + rate limiter); a config change goes through the daemon, not around the kernel | api test `config_endpoint_is_bearer_gated_and_optional` |
| No secrets / no push | PASS — `.gitignore` covers `**/kelter-secret-*.key` + `**/model-override.toml`; `git grep` for the dummy smoke key = 0 | — |

### 4. Uncertainties / TODO for human review (Phase 8.5)

- **Live API leg needs the maintainer's OpenRouter key** (never committed) — the
  local leg is proven live here; the OpenRouter leg is a documented human step
  (paste the key in the UI, or set `OPENROUTER_API_KEY`).
- **Latitude on model-class change** re-derives on restart, not live (documented
  above) — the persisted overlay makes it correct after a restart.
- **First-run health probe** uses the stored `provider` (`local`/`openrouter`);
  a model configured as the local adapter pointed at ollama is probed via
  `/health` rather than `/api/version` — a heuristic edge, not a correctness
  issue.
- Deferred (explicit): multi-profile configs, an OS-keychain/vault, configuring
  anything beyond the model. Deep **log redaction** is Phase 9 — this phase
  introduces no key leak but does not yet scrub all possible logging paths.

### 5. Version pins observed (Phase 8.5)

No new dependencies beyond wiring: `reqwest` + `async-trait` added to `daemon`
(health-check + the controller trait); `async-trait` added to `api` (the
`ModelConfigController` trait). Web UI stays dependency-free vanilla JS. Kernel
untouched.

## Phase 10 — governed, reversible self-learning

Date: 2026-07-19. Implements ADR 0016 — the runtime distils reusable **facts**
from past runs into a distinct, provenance-tagged, reversible memory layer that
informs proposals but never bypasses validation. Precondition: Phase 3
(journal/episodes — the distillation source) + 4a (memory-as-extension) on
origin. First slice = facts; plans / background distillation / sharing deferred.

### 1. Cruxes / decisions (Phase 10)

- **Reversibility is complete (crux #1).** Learned facts are a DISTINCT tagged
  layer (`LearnedStore` / the `learned_facts` table), never merged into
  `episodes` or a persistent ranking index — ranking is recomputed per
  retrieval (BM25 over `task_context`), so `forget` (a row delete) leaves **zero
  residue**. Keeping learned facts distinct is what makes forgetting complete.
- **Learning informs, never bypasses validation (crux #2).** A learned fact
  enters ONLY as retrieved *context* on the model's prompt (the tagged
  `[learned]` `PolicyContext.memory` seam), so the kernel still validates every
  proposed action (ADR 0001 floor unchanged). Even a poisoned fact cannot
  trigger an unvalidated action.
- **Governed + provenance.** `Kernel::distill` is a bearer-gated, logged,
  command-driven operation (never silent background); every fact carries
  `ai_extracted` + `human_validated:false` + source run ids; runs already
  distilled are skipped (no duplication).

### 2. Definition-of-done evidence (Phase 10)

| Gate | Result | Raw evidence |
|---|---|---|
| build/test/clippy/fmt | PASS — **127 workspace tests, 0 failures**; clippy `-D warnings` clean; fmt clean | `evidence/phase10-gates.log` |
| Naming | codename grep = 0 over the tracked tree; `agentkernel` only historical | `evidence/phase10-naming-check.log` |
| **Distillation + provenance** | **PASS** — a completed run distils into a learned fact tagged `level=ai_extracted`, `human_validated=false`, `source_run_ids=[the run]`; listed via `GET /api/v1/learn` (inspectable) | `evidence/selflearn-run.log`; unit test `distillation_tags_provenance_and_forget_batch_reverses_it` |
| **Learning informs, not bypasses (money-shot #1)** | **PASS** — a later similar run retrieves the fact as context ("learned: injected distilled facts as context" in the log) yet still validates + completes; and a **poisoned** learned fact (suggesting the tool `nuke`) reaches the model but the proposed `nuke` is REJECTED and never executes — the floor is unchanged | smoke leg 3; unit test `poisoned_learned_fact_cannot_trigger_an_unvalidated_action` |
| **Reversibility (money-shot #2)** | **PASS** — `forget_batch` → `GET /learn` empty AND a later run shows **no** learned context ("+0" injections) — the influence is completely gone, zero residue | smoke leg 4; unit tests `forget_is_complete_no_residue`, `sqlite_learned_store_ranks_and_forgets_without_residue` |
| Governance | PASS — distill/list/forget bearer-gated (anonymous `GET /learn` → 401); learned = `human_validated:false` lower-trust; no path bypasses the kernel | smoke leg 0 |
| No regression | PASS — migration v2 (`learned_facts`) is additive; durability exactly-once + a2a/tool/wasm smokes green | `scripts/smoke-durability.sh` |
| No secrets / no push | PASS | — |

### 3. Uncertainties / TODO for human review (Phase 10)

- **Extraction is deterministic** (task + ordered successful actions), not
  model-assisted — honest and reproducible for this first slice; a
  model-summarised fact is a future refinement (still governed + reversible).
- **Plan-mode runs do not yet receive learned context** (step-mode only) —
  consistent with plan-template distillation being the deferred second slice.
- Deferred, explicit: plan distillation, background/auto distillation, and
  cross-user sharing. The core discipline holds: learning here is
  reversible, governed and inspectable — never a silent behaviour mutation.

### 4. Version pins observed (Phase 10)

No new dependencies. The learned layer is `kernel-core` types + a `storage`
SQLite table (migration v2) reusing `memory::retrieval` BM25; the API adds four
bearer-gated routes. Kernel `Kernel::new` signature unchanged (learned attaches
via the `with_learned` builder).

## Phase 9 — data-lifecycle & log hardening

Date: 2026-07-19. Implements ADR 0017 — bounded store retention, a
deployment-level log-size cap, and a real secret-redaction pass. Precondition:
Phase 3 (durable store/journal) + Phase 8 (ledger scaling / OTel — built on, not
duplicated) on origin. Logging stays on stdout (twelve-factor); no in-app file
logging added.

### 1. Sources verified live (Phase 9)

| Area | What was verified | How |
|---|---|---|
| Quadlet log keys | `[Container] LogDriver=` + `LogOpt=` (repeatable) → `--log-driver`/`--log-opt max-size/max-file`; `quadlet -dryrun` generates cleanly | podman **6.0.1** quadlet generator in the local podman machine + podman docs |
| Store/journal model | `RunStore` (runs+journal), `SqliteMemoryProvider` (episodes) share one Db; terminal statuses = completed/failed/canceled; `unfinished_runs` = pending/running | `crates/kernel-core/src/store.rs`, `crates/storage` |

### 2. Decisions (Phase 9)

- **Retention prunes TERMINAL runs only** — the SQL/filter selects from
  terminal statuses, so an in-flight run can never be a victim; each victim's
  journal + episodes + row are deleted in one transaction (no half-prune, no
  orphaned journal). Durability/exactly-once are preserved by construction.
- **Log cap at the platform, not the app** — stdout stays; `deploy/kelter.container`
  gets `LogDriver=k8s-file` + `LogOpt max-size/max-file` (journald + bare-daemon
  documented as alternatives).
- **Redaction is enforced + tested** — audit shows no secret value is logged;
  the daemon logs a masked token fingerprint; `kernel_core::redact` +
  a capture test lock it in.

### 3. Definition-of-done evidence (Phase 9)

| Gate | Result | Raw evidence |
|---|---|---|
| build/test/clippy/fmt | PASS — **135 workspace tests, 0 failures**; clippy `-D warnings` clean; fmt clean | `evidence/phase9-gates.log` |
| Naming | codename grep = 0; **no secret value in any tracked file** | `evidence/phase9-naming-check.log` |
| **Retention (money-shot)** | **PASS** — 4 terminal runs + 1 injected in-flight → `prune` (max_runs=2) → `{runs_pruned:2, journal_entries_pruned:10, episodes_pruned:2}`, exactly 2 terminal remain, the **in-flight run + its journal UNTOUCHED**, no orphaned journal rows | `evidence/retention-prune.log`; unit tests `prune_keeps_n_terminal_and_never_touches_in_flight`, `prune_removes_terminal_runs_journal_episodes_but_not_in_flight` |
| **Durability intact** | **PASS** — exactly-once durability smoke (A/B/C) green after retention landed (prune never touches pending/running) | `scripts/smoke-durability.sh` |
| **Log cap** | **PASS** — `deploy/kelter.container` bounds logs (`k8s-file` + max-size=10mb + max-file=3); `quadlet -dryrun` on podman 6.0.1 generates `--log-opt max-size=10mb` / `max-file=3` with no errors | `evidence/quadlet-logcap-dryrun.log` |
| **Redaction (real, tested)** | **PASS** — a capture test installs a tracing subscriber over a buffer, logs a real secret through the redaction path, and asserts the secret value is **absent** (only `…redacted` present); the retention smoke also asserts the token value is absent from the live daemon log (masked fingerprint only) | `crates/kernel-core/tests/redaction_test.rs`; `evidence/retention-prune.log` |
| **Levels** | PASS — default `info` (`EnvFilter` fallback); `RUST_LOG` documented; no debug firehose | `docs/observability.md` |
| Docs | PASS — `observability.md` (logging dest/levels/redaction + log caps + retention) + ADR 0017 | — |
| No secrets / no push | PASS | — |

### 4. Uncertainties / TODO for human review (Phase 9)

- **Retention default** is generous (`max_runs = 10000`, `prune_on_start = true`);
  it bounds growth without losing recent history and only ever removes terminal
  runs. Operators wanting time-based retention set `max_age_days`; wanting none
  set a large cap or `prune_on_start = false`.
- **Ledger.txt** (the durability-smoke workspace file) is workspace-local, not
  part of store retention — its markers live in the pruned run's *journal*
  (removed with the run) and in a per-workspace file the operator manages.
- **Log cap uses `k8s-file`** so max-size/max-file apply; a journald deployment
  caps via `SystemMaxUse` instead (documented). `quadlet -dryrun` validates
  syntax, not runtime driver-opt compatibility.
- Redaction covers the audited paths + provides a scrubber for future
  body/header logging; per-field structured-log redaction is out of scope.

### 5. Version pins observed (Phase 9)

No new runtime dependencies. `tracing-subscriber` added as a `kernel-core`
**dev**-dependency for the redaction capture test. podman 6.0.1 (quadlet
generator). rusqlite 0.40.1 unchanged (prune is one transaction over existing
tables).
