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
