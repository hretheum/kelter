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
    pub server: ServerSection,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSection {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Exactly one capability per manifest.
    pub provides: Vec<String>,
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

/// A manifest the registry accepted, routed by capability.
#[derive(Debug, Clone)]
pub struct RegisteredExtension {
    pub name: String,
    pub capability: Capability,
    pub permissions: Permissions,
    pub spec: ServerSpec,
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
    match capability {
        Capability::Agent => {
            return Err(ManifestError::UnimplementedCapability(name, "agent"));
        }
        Capability::ModelProvider => {
            return Err(ManifestError::UnimplementedCapability(
                name,
                "model-provider",
            ));
        }
        Capability::Tools | Capability::Memory => {}
    }

    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    // A relative command (e.g. "./run.sh" or ".venv/bin/python") resolves
    // against the extension directory; bare program names ("python3") are
    // left to PATH.
    let command = if manifest.server.command.contains('/') {
        dir.join(&manifest.server.command)
            .to_string_lossy()
            .into_owned()
    } else {
        manifest.server.command.clone()
    };

    Ok(RegisteredExtension {
        spec: ServerSpec {
            name: manifest.extension.name,
            command,
            args: manifest.server.args.clone(),
            cwd: Some(dir.to_string_lossy().into_owned()),
            env: Vec::new(),
        },
        name,
        capability,
        permissions: manifest.permissions,
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
        assert_eq!(
            exts[1].spec.command,
            tool_dir.join("./run.sh").to_string_lossy()
        );
        assert_eq!(exts[0].spec.command, "python3");
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
            "agent",
            "[extension]\nname = \"agent\"\nprovides = [\"agent\"]\n\n[server]\ncommand = \"x\"\n",
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
