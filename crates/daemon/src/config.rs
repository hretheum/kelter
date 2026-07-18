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
    /// MCP servers to spawn (stdio). Empty = spawn the bundled reference
    /// tool server found next to the daemon binary.
    pub servers: Vec<mcp::ServerSpec>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: "./work".into(),
            servers: Vec::new(),
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
