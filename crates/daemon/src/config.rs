//! Daemon configuration. Topology is config: the same kernel runs headless
//! in a container, embedded in the desktop app, or on a homelab box — only
//! this file changes.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub policy: policy::PolicyConfig,
    #[serde(default)]
    pub kernel: KernelSection,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub otel: OtelConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    /// Per-model price overrides (Phase 8, item B), keyed by the policy
    /// `describe()` string. Merged over the built-in dated defaults; a model
    /// in neither yields a `null` run cost (never a fabricated number). Prices
    /// are static config — no live fetch. Example:
    ///
    /// ```toml
    /// [pricing."openrouter:anthropic/claude-sonnet-5"]
    /// input_per_mtok = 3.0
    /// output_per_mtok = 15.0
    /// ```
    #[serde(default)]
    pub pricing: std::collections::HashMap<String, kernel_core::ModelPrice>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
    #[serde(default)]
    pub budget: BudgetSection,
}

/// The human gate on mutating actions (ADR 0038). Absent = every action
/// `allow`, exactly as before. Policies are `"allow" | "ask" | "deny"`; an
/// unknown value is a config ERROR (the daemon refuses to start) rather than
/// a silent fallback — a typo in a safety gate must fail closed. Only
/// actions that declare `idempotency_key` (the ADR 0031 "mutating"
/// criterion) are gated; an entry naming any other action is logged at
/// startup and has no effect. `review_verdict = "ask"` makes plan-mode
/// review nodes a human verdict.
///
/// ```toml
/// [approval]
/// default = "allow"
/// ask_timeout_secs = 300
/// [approval.actions]
/// write_file = "ask"
/// run_command = "deny"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalConfig {
    pub default: kernel_core::ActionPolicy,
    pub actions: std::collections::HashMap<String, kernel_core::ActionPolicy>,
    /// How long a run may wait for a decision before the sweeper denies it
    /// (principal `"system:timeout"`). `0` = wait indefinitely.
    pub ask_timeout_secs: u64,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            default: kernel_core::ActionPolicy::Allow,
            actions: std::collections::HashMap::new(),
            ask_timeout_secs: 300,
        }
    }
}

impl ApprovalConfig {
    pub fn to_gate(&self) -> kernel_core::ActionGateConfig {
        kernel_core::ActionGateConfig {
            default: self.default,
            actions: self.actions.clone(),
            ask_timeout_secs: self.ask_timeout_secs,
        }
    }
}

/// Cost budgets (ADR 0039). Absent = no limits. The per-run values are both
/// the default and the ceiling for `POST /api/v1/runs` `budget` overrides
/// (a caller may lower them, never raise them).
///
/// ```toml
/// [budget]
/// max_usd = 0.50
/// max_tokens = 200000
/// max_wall_secs = 1800
/// daily_usd_per_principal = 5.0
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetSection {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<u64>,
    pub max_wall_secs: Option<u64>,
    pub daily_usd_per_principal: Option<f64>,
    /// Tokens added to every model-call bound for what the adapter cannot
    /// see (hidden provider templates / tool preambles).
    pub prompt_overhead_tokens: u64,
    /// Completion bound when an adapter does not report `max_tokens`.
    pub fallback_completion_tokens: u64,
}

impl Default for BudgetSection {
    fn default() -> Self {
        Self {
            max_usd: None,
            max_tokens: None,
            max_wall_secs: None,
            daily_usd_per_principal: None,
            prompt_overhead_tokens: kernel_core::BudgetConfig::DEFAULT_PROMPT_OVERHEAD_TOKENS,
            fallback_completion_tokens:
                kernel_core::BudgetConfig::DEFAULT_FALLBACK_COMPLETION_TOKENS,
        }
    }
}

impl BudgetSection {
    pub fn to_budget(&self) -> anyhow::Result<kernel_core::BudgetConfig> {
        for (name, v) in [
            ("max_usd", self.max_usd),
            ("daily_usd_per_principal", self.daily_usd_per_principal),
        ] {
            if let Some(v) = v {
                anyhow::ensure!(v.is_finite() && v > 0.0, "[budget] {name} must be > 0");
            }
        }
        anyhow::ensure!(
            self.max_tokens != Some(0) && self.max_wall_secs != Some(0),
            "[budget] max_tokens / max_wall_secs must be > 0"
        );
        Ok(kernel_core::BudgetConfig {
            per_run: kernel_core::BudgetLimits {
                max_usd: self.max_usd,
                max_tokens: self.max_tokens,
                max_wall_secs: self.max_wall_secs,
            },
            daily_usd_per_principal: self.daily_usd_per_principal,
            prompt_overhead_tokens: self.prompt_overhead_tokens,
            fallback_completion_tokens: self.fallback_completion_tokens,
        })
    }
}

/// Skills (ADR 0019): advisory procedural knowledge in the OpenSkills /
/// SKILL.md format. Skill instructions enter the ORACLE'S CONTEXT only
/// (progressive disclosure); bundled scripts/tools reach the action space
/// exclusively through `[tools] extension_dirs` + the `[trust]` gate —
/// installing or activating a skill never executes or authorizes anything.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Master switch: absent/true → the registry loads and the catalog
    /// (name+description) is visible to the oracle.
    pub enabled: bool,
    /// The installed-skills directory (managed by `kelter skill …`).
    pub dir: String,
    /// Skills to activate at startup (full instructions into context).
    /// Runtime (de)activation via `kelter skill activate/deactivate` also
    /// persists in the skills dir and wins over this seed.
    pub active: Vec<String>,
    /// Which installed skills the MODEL may self-activate as a kernel-owned
    /// `activate_skill` action, run-scoped and ephemeral (ADR 0020):
    /// `"trusted"` (default) — only skills an operator has explicitly
    /// trusted; since every skill is `untrusted` at install, this makes the
    /// feature INERT until an operator opts a skill in. `"all"` — any
    /// installed skill. `"ask"` (ADR 0021) — an untrusted skill neither
    /// auto-allows nor auto-denies: the run pauses for a human decision,
    /// subject to `ask_fallback`. An unrecognized value falls back to
    /// `"trusted"` with a warning. In every case, activation only ever
    /// changes advisory context — it never grants capability, and the kernel
    /// re-validates every proposed action against the schema regardless.
    pub model_activation: String,
    /// Only consulted when `model_activation == "ask"` (ADR 0021). `"deny"`
    /// (default) — an ask-worthy self-activation is denied outright, same as
    /// `"trusted"`, for deployments with no one to ask (e.g. a headless A2A
    /// worker); under this default a run configured with `model_activation =
    /// "ask"` alone NEVER pauses. `"queue"` — actually pause and wait,
    /// bounded by `ask_timeout_secs`. An unrecognized value falls back to
    /// `"deny"` with a warning.
    pub ask_fallback: String,
    /// How long a paused run may wait for a human decision before the
    /// background sweeper auto-denies it (principal `"system:timeout"`).
    /// Only relevant when `ask_fallback == "queue"`.
    pub ask_timeout_secs: u64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "./skills".into(),
            active: Vec::new(),
            model_activation: "trusted".into(),
            ask_fallback: "deny".into(),
            ask_timeout_secs: 300,
        }
    }
}

/// Governed A2A delegation limits (ADR 0022) + agent trust/policy/ask +
/// resilience (ADR 0023): a per-run attempt budget, a chain-depth cap,
/// (kernel-side, via `Run::a2a_ancestry`) cycle detection, which registered
/// agents the model may delegate to, and bounded retry/sanctioned fallback.
/// Defaults mirror `kernel_core::DelegationLimits::default()` /
/// `kernel_core::ResilienceConfig::default()` — reasonable-but-arbitrary,
/// not derived from any measurement; see ADR 0022/0023 for the honest
/// framing and the sign-off notes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DelegationConfig {
    pub max_per_run: u32,
    pub max_depth: u32,
    /// The agent-trust registry file (ADR 0023) — a single TOML file,
    /// separate from the skills directory (agents have no natural
    /// per-agent directory the way a skill's `SKILL.md` does).
    pub agents_registry_path: String,
    /// Which registered agents the MODEL may delegate to (ADR 0023):
    /// `"trusted"` (default) — only agents an operator (or a human's
    /// `allow_always` decision) has trusted. A config-declared agent starts
    /// trusted already (see the ADR's default-trust split), so this is
    /// non-breaking out of the box; a discovery-registered agent starts
    /// untrusted, so THIS is what makes it inert until trusted. `"all"` —
    /// any registered agent. `"ask"` — an untrusted agent's delegation
    /// attempt pauses for a human decision, subject to
    /// `agent_ask_fallback`. An unrecognized value falls back to
    /// `"trusted"` with a warning.
    pub agent_policy: String,
    /// Only consulted when `agent_policy == "ask"`. `"deny"` (default) — an
    /// ask-worthy delegation is denied outright, same as `"trusted"`, for a
    /// deployment with no one to ask; under this default a run configured
    /// with `agent_policy = "ask"` alone NEVER pauses. `"queue"` — actually
    /// pause and wait, bounded by `agent_ask_timeout_secs`.
    pub agent_ask_fallback: String,
    /// How long a delegation-authorization pause may wait for a human
    /// decision before the background sweeper auto-denies it. Only
    /// relevant when `agent_ask_fallback == "queue"`.
    pub agent_ask_timeout_secs: u64,
    /// Governed resilience (ADR 0023): additional dial attempts against the
    /// SAME target after the first fails, before falling through to
    /// `fallback` (if configured). Every retry re-enters the SAME
    /// budget/depth/cycle/trust checks as a fresh proposal.
    pub max_retries_per_delegation: u32,
    /// Consecutive dial failures against one agent origin before its
    /// circuit opens (never dialed again until cooldown elapses).
    pub circuit_failure_threshold: u32,
    /// How long an open circuit stays open before a single half-open probe
    /// is allowed through.
    pub circuit_cooldown_secs: u64,
    /// Sanctioned-only fallback targets (ADR 0023): NEVER auto-discovered.
    /// Keyed by the PRIMARY agent's name (as declared in its own Agent
    /// Card / config entry — NOT the `delegate_to_*` action name; the
    /// daemon normalizes both keys and values to action names when
    /// building `kernel_core::ResilienceConfig`). Example:
    /// `worker_a = ["worker_b", "worker_c"]`.
    #[serde(default)]
    pub fallback: std::collections::HashMap<String, Vec<String>>,
    /// Consume streamed progress from a running delegation (ADR 0027) when
    /// the remote agent's card advertises `capabilities.streaming`. `true`
    /// by default and safe to leave on: a non-streaming remote transparently
    /// falls back to the unchanged `SendMessage` + `GetTask` polling, and a
    /// mid-stream failure falls back to polling the already-created task.
    /// Set `false` to force polling for every dial.
    pub stream_progress: bool,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_per_run: 10,
            max_depth: 3,
            agents_registry_path: "./agents.toml".into(),
            agent_policy: "trusted".into(),
            agent_ask_fallback: "deny".into(),
            agent_ask_timeout_secs: 300,
            max_retries_per_delegation: 2,
            circuit_failure_threshold: 3,
            circuit_cooldown_secs: 60,
            fallback: std::collections::HashMap::new(),
            stream_progress: true,
        }
    }
}

/// Security surface configuration (Phase 11, ADR 0018). All additive — the
/// default local/tailnet flow (single bearer token) is unchanged when this is
/// absent.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// Extra origins the config health-check may probe, on top of the presets
    /// and the configured base_url (SSRF allowlist — see ADR 0018).
    pub test_allowlist: Vec<String>,
    /// Named principals: each names an env var holding that principal's bearer
    /// token. The token VALUE stays in the environment, never in this file.
    /// With none configured, the single `[server] token` is principal
    /// "default" and nothing changes.
    pub principals: Vec<PrincipalConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalConfig {
    /// Principal name attributed per run in the journal (e.g. "alice").
    pub name: String,
    /// Env var holding this principal's bearer token (secrets env-only).
    pub token_env: String,
}

/// Opt-in TLS termination (Phase 11, ADR 0018). Absent → plain HTTP (the
/// default; a tailnet provides transport security). mTLS is deferred.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    /// PEM certificate chain file (gitignored; never committed).
    pub cert: String,
    /// PEM private key file (gitignored; never committed).
    pub key: String,
}

/// Store data-lifecycle policy (ADR 0017, Phase 9). Bounds unbounded growth of
/// the SQLite store by pruning **terminal** runs (+ their journal/episodes)
/// beyond a window — never an in-flight run. A generous default keeps growth
/// bounded out of the box without losing recent history.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Prune on daemon start (and it is always available via the `prune`
    /// subcommand). Set false to never auto-prune.
    pub prune_on_start: bool,
    /// Keep at most this many terminal runs (newest). `None`/0 = unlimited.
    pub max_runs: Option<usize>,
    /// Prune terminal runs older than this many days. `None` = no age limit.
    pub max_age_days: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            prune_on_start: true,
            max_runs: Some(10_000),
            max_age_days: None,
        }
    }
}

impl RetentionConfig {
    pub fn to_policy(&self) -> kernel_core::RetentionPolicy {
        kernel_core::RetentionPolicy {
            max_runs: self.max_runs.filter(|n| *n > 0),
            max_age_secs: self.max_age_days.map(|d| d * 86_400),
        }
    }
}

/// Memory retrieval configuration (Phase 8, item D). BM25 is the default; set
/// `embeddings_url` to point the built-in durable memory at a real embedding
/// server (a dedicated llama.cpp `--embedding` instance) for cosine retrieval.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Which backend holds episodic memory + learned facts:
    /// - `"sqlite"` (default) — the local single-file store (single instance).
    /// - `"postgres"` — a SHARED Postgres+pgvector backend (ADR 0028) so TWO
    ///   independent kernels (desktop + server) share memory + learned facts.
    ///   Requires the daemon to be built with the `postgres` feature; the
    ///   connection string is read from the env var named by
    ///   `postgres_url_env` (never from this file). Opt-in and non-breaking:
    ///   existing sqlite deployments are unaffected.
    pub backend: String,
    /// Name of the env var holding the Postgres connection string (with any
    /// password) when `backend = "postgres"`. The value stays in the
    /// environment — never in the config file or logs. Default:
    /// `KELTER_MEMORY_POSTGRES_URL`.
    pub postgres_url_env: String,
    /// OpenAI-compatible embeddings server origin (e.g. http://127.0.0.1:8090).
    /// Absent → BM25 lexical retrieval (the default, no network). When
    /// `backend = "postgres"` this also turns on the pgvector cosine path.
    pub embeddings_url: Option<String>,
    /// Model name sent in the embeddings request (llama.cpp ignores the value
    /// but the field is required by the OpenAI schema).
    pub embeddings_model: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            postgres_url_env: "KELTER_MEMORY_POSTGRES_URL".into(),
            embeddings_url: None,
            embeddings_model: None,
        }
    }
}

/// Operator-side trust policy (ADR 0012). The manifest declares a trust
/// mode; THIS is the separate operator consent. A subprocess extension that
/// opted into `subprocess-unsandboxed` runs only if the operator lists it
/// here by name — the manifest cannot authorise its own escape.
///
/// Note: this is NOT a sandbox. An allowed subprocess still has OS-level
/// egress; the operator is explicitly accepting that (see ADR 0009/0012).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct TrustConfig {
    /// Names of extensions the operator trusts to run as unsandboxed
    /// subprocesses.
    pub allow_unsandboxed_subprocess: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct OtelConfig {
    /// OTLP/HTTP endpoint (e.g. "http://127.0.0.1:4318"). Absent = tracing
    /// export disabled (console logging only).
    pub endpoint: Option<String>,
    /// service.name resource attribute.
    pub service_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// "sqlite" (durable, default) or "memory" (ephemeral, for tests).
    pub backend: String,
    /// SQLite database file (runs + journal + memory share it).
    pub path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            path: "./kelter.db".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    /// Bearer token; when absent a random one is generated and printed once.
    pub token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7466".into(),
            token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KernelSection {
    pub max_steps: u32,
    pub reflection: bool,
    /// "step" (default, one action at a time) or "plan" (validated durable
    /// DAG proposed up front).
    pub mode: String,
    /// Plan-mode resilience (ADR 0030): additional attempts for a FAILED
    /// tool/a2a plan node before the failure counts (total attempts =
    /// 1 + this). 0 (default) = a node failure counts immediately.
    pub plan_node_retries: u32,
    /// Plan-mode resilience (ADR 0030): max revised-plan proposals after a
    /// plan failure. 0 (default) = adaptive re-planning off — a failed plan
    /// fails the run, the pre-ADR-0030 behavior.
    pub plan_replans: u32,
}

impl Default for KernelSection {
    fn default() -> Self {
        Self {
            max_steps: 12,
            reflection: true,
            mode: "step".into(),
            plan_node_retries: 0,
            plan_replans: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Directory the reference tools operate in.
    pub workspace: String,
    /// Whether to start the bundled reference tool server. Disable to run
    /// the kernel with only your own servers/extensions.
    pub bundled_tools: bool,
    /// Additional MCP servers to spawn (stdio).
    pub servers: Vec<mcp::ServerSpec>,
    /// Directories scanned for extensions (subdirectories containing a
    /// `kelter-extension.toml` manifest). Extensions register at startup —
    /// no recompilation (ADR 0006).
    pub extension_dirs: Vec<String>,
    /// Agentic-coding tool surface (A1/C4/C5, opt-in): when present, the
    /// bundled code-server is spawned against `root`. SECURITY: this hands
    /// the model command execution (allowlisted, in-root) with the daemon's
    /// full OS authority — point it ONLY at a disposable checkout/fixture
    /// until the E-gate review, and prefer `bundled_tools = false` alongside
    /// so the planner's action space is purely the coding loop.
    pub coding: Option<CodingSection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodingSection {
    /// The repository/fixture directory the agent works on. Reads, writes
    /// and command cwd are all confined to this root (E2-style symlink
    /// containment included).
    pub root: String,
    /// A1 measurement toggle: `false` (default) = separated roles — the
    /// planner's ONLY route to changing code is `popraw_kod(sciezka,
    /// instrukcja)`, applied by a tool-side executor-model call. `true` =
    /// merged roles — the planner writes content directly via
    /// `zastosuj_patch` (the measured-worse "before" configuration).
    pub merged_roles: bool,
    /// Executor model endpoint for `popraw_kod`; defaults to the
    /// `[policy]` base_url (same server, same model — the measured win is
    /// the role split, not a second model).
    pub executor_url: Option<String>,
    /// Executor model name (informational for llama-server).
    pub executor_model: Option<String>,
    /// Executor edit budget (B2 applies to edits too).
    pub executor_max_tokens: u32,
    /// Commands the model may run via `uruchom_komende`.
    pub allow: Vec<String>,
    /// Per-command wall-clock limit.
    pub timeout_secs: u64,
    /// C5: how many error blocks the tool-side summary keeps.
    pub summary_errors: usize,
}

impl Default for CodingSection {
    fn default() -> Self {
        Self {
            root: String::new(),
            merged_roles: false,
            executor_url: None,
            executor_model: None,
            executor_max_tokens: 2048,
            allow: vec!["cargo".into(), "git".into()],
            timeout_secs: 180,
            summary_errors: 5,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: "./work".into(),
            bundled_tools: true,
            servers: Vec::new(),
            extension_dirs: vec!["./extensions".into()],
            coding: None,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config '{path}': {e}"))?;
        let cfg: Config =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("invalid config '{path}': {e}"))?;
        Ok(cfg)
    }
}
