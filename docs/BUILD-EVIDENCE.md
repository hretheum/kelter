# Build evidence — Phase 1 walking skeleton

Date: 2026-07-18. Machine: macOS (Darwin 25.3.0, arm64), Rust 1.95.0,
llama.cpp b10050 (Homebrew), Docker Desktop. This file logs what was
actually read, decided, run and left uncertain. Raw logs sit next to it in
`docs/evidence/`.

## 1. Sources actually consulted

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

## 2. Decision log

| Decision | Rationale |
|---|---|
| Placeholder name `agentkernel` (crates keep functional names) | Naming rule: one neutral placeholder used consistently for product/image/unit; final name is a Phase 2 decision. |
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

## 3. Definition-of-done evidence

| Gate | Result | Raw log |
|---|---|---|
| `cargo build --workspace` | PASS | `evidence/build-test-clippy-fmt.log` |
| `cargo test --workspace` | PASS — 36 tests, 0 failures (16 suites) | same |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS — no issues | same |
| `cargo fmt --all --check` | PASS | same |
| Smoke, local leg (mandatory) | **PASS e2e** — run `completed`, 3 tool steps + finish on `local:qwen2.5-0.5b-instruct` (GBNF), report produced; live WS stream captured | `evidence/smoke-local-run.json`, `evidence/smoke-local-report.md`, `evidence/smoke-local-events.ndjson`, `evidence/smoke-local-daemon.log` |
| Smoke, OpenRouter leg | Unit test on mocked HTTP: PASS (asserts bearer header, `tools` payload, arguments-as-string parsing, kernel validation round-trip). **Live leg: SKIPPED — `OPENROUTER_API_KEY` not present in this run's environment.** Runnable via `OPENROUTER_API_KEY=... scripts/smoke-openrouter.sh` | `crates/policy/tests/openrouter_mock_test.rs` (in `build-test-clippy-fmt.log`), `evidence/smoke-openrouter-SKIPPED.log` |
| One-line adapter swap | Config diff shows exactly one `engine =` line differing (plus comments); same task ran through the local leg live and through the OpenRouter adapter on mocked HTTP | `evidence/config-swap.diff` |
| Container image amd64+arm64 | see `evidence/docker-buildx.log` | `evidence/docker-buildx.log` |
| Quadlet unit | Written per podman-systemd.unit(5) conventions; **not machine-validated** (no podman on this machine) — listed in §5 | `deploy/agentkernel.container` |
| Desktop build | PASS — release build linking `kernel-core` (standalone topology compiled in) + macOS `.app` bundle; remote topology is runtime config in the same binary | `evidence/desktop-build.log` |
| README quickstart actually executed | daemon startup, `/healthz`, run submission and completed run record | `evidence/quickstart-daemon.log` |
| ADRs ×5 + architecture.md consistent with code | crate/type names in docs exist in the workspace (`Kernel`, `ActionSchema::validate`, `PolicyOracle`, `McpToolSource`, `MemoryProvider`, …) | `docs/adr/`, `docs/architecture.md` |
| No secrets in repo; key env-only | adapter reads `OPENROUTER_API_KEY` from env only; `.env*` gitignored; mock tests use obviously-fake keys | `crates/policy/src/lib.rs` |
| Naming rule (case-insensitive grep for the internal codename over the tracked tree = 0 hits; evidence paths sanitized to `<repo>`) | PASS | `evidence/naming-check.log` |

## 4. Empirical verification of constrained decoding (core of the local leg)

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

## 5. Uncertainties / TODO for human review

- **OpenRouter live leg not executed** — no key in this run's environment.
  The adapter is spec-verified + mock-tested; run
  `OPENROUTER_API_KEY=... scripts/smoke-openrouter.sh docs/evidence` to
  produce the live log. Model IDs in examples
  (`anthropic/claude-sonnet-5`) were sampled from the live models endpoint
  but should be re-checked at run time.
- **Quadlet unit not machine-validated** — podman is not installed here.
  Validate with `systemd-analyze --user verify` / a podman host before
  relying on it.
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

## 6. Version pins observed in this build

rmcp 2.2.0 · axum 0.8 · jsonschema 0.28 (chosen; `validator_for` +
`iter_errors` API) · tauri 2.11.5 / tauri-cli 2.11.4 (npm @tauri-apps/cli)
· reqwest 0.12 (rustls) · llama.cpp b10050 · model
`Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m`.
