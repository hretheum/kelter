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
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct MemoryConfig {
    /// OpenAI-compatible embeddings server origin (e.g. http://127.0.0.1:8090).
    /// Absent → BM25 lexical retrieval (the default, no network).
    pub embeddings_url: Option<String>,
    /// Model name sent in the embeddings request (llama.cpp ignores the value
    /// but the field is required by the OpenAI schema).
    pub embeddings_model: Option<String>,
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
}

impl Default for KernelSection {
    fn default() -> Self {
        Self {
            max_steps: 12,
            reflection: true,
            mode: "step".into(),
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
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: "./work".into(),
            bundled_tools: true,
            servers: Vec::new(),
            extension_dirs: vec!["./extensions".into()],
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
