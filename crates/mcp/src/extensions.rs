//! The capability registry — the generalized extension model (ADR 0006 →
//! ADR 0008). An extension is a directory with a `kelter-extension.toml`
//! manifest declaring WHAT capability it provides (`tools`, `memory`; the
//! `agent` and `model-provider` axes are reserved and refuse to load until
//! implemented), which permissions it requires, and how to start it. Every
//! extension runs as an ordinary MCP server; the registry only decides
//! which kernel seam it plugs into:
//!
//! - `tools`  → the action space (`ActionSource`), gated by kernel-side
//!   action validation and the run journal (Phase 2).
//! - `memory` → the `MemoryProvider` seam via reserved MCP tools
//!   (`memory_write` / `memory_retrieve` / `memory_upsert`); responses are
//!   schema-validated by the kernel-side client before use.
//!
//! Governance is non-negotiable either way: extensions are untrusted by
//! construction, and a manifest that declares anything this runtime cannot
//! scope (unknown capability, unknown permission key) fails startup loudly
//! instead of being silently ignored.

use std::path::Path;

use serde::Deserialize;

use crate::client::ServerSpec;

pub const MANIFEST_NAME: &str = "kelter-extension.toml";

/// Capability axes the registry understands. `Agent` and `ModelProvider`
/// parse (forward-compatible manifests) but refuse to register until the
/// runtime implements them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Tools,
    Memory,
    Agent,
    ModelProvider,
}

impl Capability {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "tools" => Some(Self::Tools),
            "memory" => Some(Self::Memory),
            "agent" => Some(Self::Agent),
            "model-provider" => Some(Self::ModelProvider),
            _ => None,
        }
    }
}

/// Permissions an extension may request. Unknown keys are a manifest error
/// (`deny_unknown_fields`) — the registry never silently grants what it
/// cannot scope. Enforcement in this slice: `workspace_write` gates whether
/// the runtime passes `KELTER_WORKSPACE` to the server; `network` is
/// recorded and logged (transport-level enforcement is a later slice —
/// documented, not pretended).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    pub network: bool,
    pub workspace_write: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub extension: ExtensionSection,
    /// Subprocess extensions (tools, memory): how to start the MCP server.
    #[serde(default)]
    pub server: Option<ServerSection>,
    /// Remote-agent extensions: where the agent lives. The kernel dials it
    /// as an A2A client — nothing is spawned.
    #[serde(default)]
    pub agent: Option<AgentSection>,
    /// WASM extensions: the in-process, capability-sandboxed module.
    #[serde(default)]
    pub wasm: Option<WasmSection>,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmSection {
    /// Path to the `.wasm` module (relative to the extension directory).
    pub module: String,
    /// The action name the module exposes.
    pub tool: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    /// The A2A Agent Card URL (…/.well-known/agent-card.json). Its host
    /// becomes the ONLY host the kernel will dial for this agent
    /// (SSRF allowlist).
    pub card_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSection {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Exactly one capability per manifest.
    pub provides: Vec<String>,
    /// How the extension runs: `"subprocess"` (default, MCP stdio / remote
    /// A2A) or `"wasm"` (in-process, capability-sandboxed via wasmtime).
    #[serde(default = "default_mechanism")]
    pub mechanism: String,
}

fn default_mechanism() -> String {
    "subprocess".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Command to start the extension's MCP server (stdio). Relative
    /// commands resolve against the extension directory.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// How an accepted extension is reached.
#[derive(Debug, Clone)]
pub enum ExtensionKind {
    /// A spawned MCP server over stdio (tools, memory).
    Server(ServerSpec),
    /// A remote A2A agent the kernel dials as a client (never spawned).
    RemoteAgent(RemoteAgentEndpoint),
    /// An in-process, capability-sandboxed WASM module (ADR 0011).
    Wasm(WasmEndpoint),
}

#[derive(Debug, Clone)]
pub struct RemoteAgentEndpoint {
    pub card_url: String,
}

#[derive(Debug, Clone)]
pub struct WasmEndpoint {
    /// Absolute path to the `.wasm` module.
    pub module_path: String,
    /// The action name the module exposes.
    pub tool: String,
    pub description: String,
}

/// A manifest the registry accepted, routed by capability.
#[derive(Debug, Clone)]
pub struct RegisteredExtension {
    pub name: String,
    pub capability: Capability,
    pub permissions: Permissions,
    pub kind: ExtensionKind,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("invalid manifest {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error("extension '{0}' declares unknown capability {1:?} — refusing to load")]
    UnknownCapability(String, String),
    #[error(
        "extension '{0}' declares capability '{1}', which this runtime does not implement yet — \
         refusing to load rather than silently ignoring it"
    )]
    UnimplementedCapability(String, &'static str),
    #[error("extension '{0}' must declare exactly one capability (got {1:?})")]
    OneCapability(String, Vec<String>),
    #[error(
        "extension '{0}' (capability '{1}') needs a [{2}] section in its manifest — refusing to load"
    )]
    MissingSection(String, &'static str, &'static str),
    #[error(
        "agent extension '{0}' does not request the `network` permission — a remote agent \
         cannot be dialed without it; declare [permissions] network = true"
    )]
    NetworkRequired(String),
    #[error("agent extension '{0}' card_url must be http(s), got '{1}'")]
    BadCardUrl(String, String),
    #[error(
        "extension '{0}' declares unknown mechanism '{1}' (expected \"subprocess\" or \"wasm\")"
    )]
    UnknownMechanism(String, String),
    #[error(
        "extension '{0}' capability '{1}' does not support mechanism '{2}' in this runtime slice — \
         WASM currently supports only the `tools` capability"
    )]
    UnsupportedMechanism(String, &'static str, &'static str),
}

/// Parse one manifest file into a registered extension.
pub fn load_manifest(manifest_path: &Path) -> Result<RegisteredExtension, ManifestError> {
    let display = manifest_path.display().to_string();
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|e| ManifestError::Io(display.clone(), e))?;
    let manifest: Manifest =
        toml::from_str(&raw).map_err(|e| ManifestError::Parse(display.clone(), e))?;

    let name = manifest.extension.name.clone();
    if manifest.extension.provides.len() != 1 {
        return Err(ManifestError::OneCapability(
            name,
            manifest.extension.provides,
        ));
    }
    let declared = &manifest.extension.provides[0];
    let capability = Capability::parse(declared)
        .ok_or_else(|| ManifestError::UnknownCapability(name.clone(), declared.clone()))?;

    let mechanism = manifest.extension.mechanism.as_str();
    if mechanism != "subprocess" && mechanism != "wasm" {
        return Err(ManifestError::UnknownMechanism(name, mechanism.to_string()));
    }
    // WASM is a tools-only mechanism in this slice; anything else with
    // mechanism = "wasm" fails loudly (memory/agent/model-provider deferred).
    if mechanism == "wasm" && capability != Capability::Tools {
        let cap = match capability {
            Capability::Memory => "memory",
            Capability::Agent => "agent",
            Capability::ModelProvider => "model-provider",
            Capability::Tools => unreachable!(),
        };
        return Err(ManifestError::UnsupportedMechanism(name, cap, "wasm"));
    }

    let kind = match capability {
        Capability::ModelProvider => {
            return Err(ManifestError::UnimplementedCapability(
                name,
                "model-provider",
            ));
        }
        Capability::Agent => {
            let agent = manifest
                .agent
                .ok_or_else(|| ManifestError::MissingSection(name.clone(), "agent", "agent"))?;
            if !manifest.permissions.network {
                return Err(ManifestError::NetworkRequired(name));
            }
            if !agent.card_url.starts_with("http://") && !agent.card_url.starts_with("https://") {
                return Err(ManifestError::BadCardUrl(name, agent.card_url));
            }
            ExtensionKind::RemoteAgent(RemoteAgentEndpoint {
                card_url: agent.card_url,
            })
        }
        Capability::Tools if manifest.extension.mechanism == "wasm" => {
            let wasm = manifest
                .wasm
                .ok_or_else(|| ManifestError::MissingSection(name.clone(), "wasm", "wasm"))?;
            let dir = manifest_path.parent().unwrap_or(Path::new("."));
            let module_path = dir.join(&wasm.module).to_string_lossy().into_owned();
            ExtensionKind::Wasm(WasmEndpoint {
                module_path,
                tool: wasm.tool,
                description: manifest.extension.description.clone(),
            })
        }
        Capability::Tools | Capability::Memory => {
            let server = manifest.server.ok_or_else(|| {
                ManifestError::MissingSection(
                    name.clone(),
                    if capability == Capability::Tools {
                        "tools"
                    } else {
                        "memory"
                    },
                    "server",
                )
            })?;
            let dir = manifest_path.parent().unwrap_or(Path::new("."));
            // A relative command (e.g. "./run.sh" or ".venv/bin/python")
            // resolves against the extension directory; bare program names
            // ("python3") are left to PATH.
            let command = if server.command.contains('/') {
                dir.join(&server.command).to_string_lossy().into_owned()
            } else {
                server.command.clone()
            };
            ExtensionKind::Server(ServerSpec {
                name: manifest.extension.name,
                command,
                args: server.args.clone(),
                cwd: Some(dir.to_string_lossy().into_owned()),
                env: Vec::new(),
            })
        }
    };

    Ok(RegisteredExtension {
        name,
        capability,
        permissions: manifest.permissions,
        kind,
    })
}

/// Scan directories for extension manifests: each immediate subdirectory
/// (or the directory itself) containing `kelter-extension.toml` is an
/// extension. Invalid manifests are reported, not silently skipped.
pub fn discover(dirs: &[String]) -> (Vec<RegisteredExtension>, Vec<ManifestError>) {
    let mut extensions = Vec::new();
    let mut errors = Vec::new();

    for dir in dirs {
        let root = Path::new(dir);
        let mut candidates = vec![root.join(MANIFEST_NAME)];
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    candidates.push(p.join(MANIFEST_NAME));
                }
            }
        }
        for manifest in candidates.into_iter().filter(|p| p.is_file()) {
            match load_manifest(&manifest) {
                Ok(ext) => extensions.push(ext),
                Err(e) => errors.push(e),
            }
        }
    }
    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    (extensions, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ext(root: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_NAME), body).unwrap();
        dir
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kelter-reg-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn discovers_tool_and_memory_extensions() {
        let root = tmp();
        let tool_dir = write_ext(
            &root,
            "tooly",
            "[extension]\nname = \"tooly\"\nprovides = [\"tools\"]\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        write_ext(
            &root,
            "memy",
            "[extension]\nname = \"memy\"\nprovides = [\"memory\"]\n\n[permissions]\nworkspace_write = true\n\n[server]\ncommand = \"python3\"\nargs = [\"server.py\"]\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0].capability, Capability::Memory);
        assert!(exts[0].permissions.workspace_write);
        assert_eq!(exts[1].capability, Capability::Tools);
        // relative command resolved into the extension dir; bare left to PATH
        let (mem_cmd, tool_cmd) = match (&exts[0].kind, &exts[1].kind) {
            (ExtensionKind::Server(m), ExtensionKind::Server(t)) => {
                (m.command.clone(), t.command.clone())
            }
            other => panic!("expected two server extensions, got {other:?}"),
        };
        assert_eq!(tool_cmd, tool_dir.join("./run.sh").to_string_lossy());
        assert_eq!(mem_cmd, "python3");
    }

    #[test]
    fn agent_extension_loads_with_card_url_and_network() {
        let root = tmp();
        write_ext(
            &root,
            "remote",
            "[extension]\nname = \"remote\"\nprovides = [\"agent\"]\n\n[permissions]\nnetwork = true\n\n[agent]\ncard_url = \"http://127.0.0.1:9999\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts[0].capability, Capability::Agent);
        match &exts[0].kind {
            ExtensionKind::RemoteAgent(a) => assert_eq!(a.card_url, "http://127.0.0.1:9999"),
            other => panic!("expected remote agent, got {other:?}"),
        }
    }

    #[test]
    fn wasm_tool_extension_routes_to_wasm_kind() {
        let root = tmp();
        write_ext(
            &root,
            "wc",
            "[extension]\nname = \"wc\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\ntool = \"word_count\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts.len(), 1);
        match &exts[0].kind {
            ExtensionKind::Wasm(w) => {
                assert!(w.module_path.ends_with("m.wasm"));
                assert_eq!(w.tool, "word_count");
            }
            other => panic!("expected wasm kind, got {other:?}"),
        }
    }

    #[test]
    fn wasm_needs_wasm_section() {
        let root = tmp();
        write_ext(
            &root,
            "nowasm",
            "[extension]\nname = \"nowasm\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("[wasm] section"));
    }

    #[test]
    fn wasm_only_supports_tools_capability() {
        let root = tmp();
        write_ext(
            &root,
            "wmem",
            "[extension]\nname = \"wmem\"\nprovides = [\"memory\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\ntool = \"t\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0]
            .to_string()
            .contains("does not support mechanism 'wasm'"));
    }

    #[test]
    fn unknown_mechanism_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "weird-mech",
            "[extension]\nname = \"weird-mech\"\nprovides = [\"tools\"]\nmechanism = \"telepathy\"\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("unknown mechanism"));
    }

    #[test]
    fn agent_without_network_permission_is_refused() {
        let root = tmp();
        write_ext(
            &root,
            "sneaky-agent",
            "[extension]\nname = \"sneaky-agent\"\nprovides = [\"agent\"]\n\n[agent]\ncard_url = \"http://127.0.0.1:9999\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("network"));
    }

    #[test]
    fn agent_needs_agent_section_not_server() {
        let root = tmp();
        write_ext(
            &root,
            "confused",
            "[extension]\nname = \"confused\"\nprovides = [\"agent\"]\n\n[permissions]\nnetwork = true\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("[agent] section"));
    }

    #[test]
    fn unknown_capability_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "weird",
            "[extension]\nname = \"weird\"\nprovides = [\"telepathy\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("unknown capability"));
    }

    #[test]
    fn declared_but_unimplemented_capability_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "modelp",
            "[extension]\nname = \"modelp\"\nprovides = [\"model-provider\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("does not implement yet"));
    }

    #[test]
    fn unknown_permission_key_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "sneaky",
            "[extension]\nname = \"sneaky\"\nprovides = [\"tools\"]\n\n[permissions]\nsudo = true\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(matches!(errors[0], ManifestError::Parse(..)));
    }

    #[test]
    fn exactly_one_capability_required() {
        let root = tmp();
        write_ext(
            &root,
            "greedy",
            "[extension]\nname = \"greedy\"\nprovides = [\"tools\", \"memory\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("exactly one capability"));
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let (exts, errors) = discover(&["/nonexistent/kelter-ext".to_string()]);
        assert!(exts.is_empty());
        assert!(errors.is_empty());
    }
}
