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
}

impl Default for KernelSection {
    fn default() -> Self {
        Self {
            max_steps: 12,
            reflection: true,
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
