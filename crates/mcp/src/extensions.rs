//! Extension discovery — the first slice of the governed extension model
//! (ADR 0006). An extension is a directory containing a
//! `kelter-extension.toml` manifest declaring what it provides and how to
//! start it. The kernel reads the manifest and runs the extension as an
//! ordinary MCP server: same protocol, same kernel-side validation, no
//! recompilation.
//!
//! Governance in this slice: only manifests declaring `provides = ["tools"]`
//! are registered as tool servers, commands/args are resolved relative to
//! the extension's own directory, and everything an extension proposes still
//! passes kernel-side schema validation and run journaling like any other
//! tool.

use std::path::Path;

use serde::Deserialize;

use crate::client::ServerSpec;

pub const MANIFEST_NAME: &str = "kelter-extension.toml";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub extension: ExtensionSection,
    pub server: ServerSection,
}

#[derive(Debug, Deserialize)]
pub struct ExtensionSection {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Capabilities this extension provides. This slice understands
    /// `"tools"`; anything else is refused (forward-compatible gate).
    pub provides: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSection {
    /// Command to start the extension's MCP server (stdio). Relative
    /// commands resolve against the extension directory.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("invalid manifest {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error("extension '{0}' provides {1:?}; this runtime slice only supports [\"tools\"]")]
    UnsupportedCapability(String, Vec<String>),
}

/// Parse one manifest file into a spawnable server spec.
pub fn load_manifest(manifest_path: &Path) -> Result<ServerSpec, ManifestError> {
    let display = manifest_path.display().to_string();
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|e| ManifestError::Io(display.clone(), e))?;
    let manifest: Manifest =
        toml::from_str(&raw).map_err(|e| ManifestError::Parse(display.clone(), e))?;

    if !manifest.extension.provides.iter().any(|p| p == "tools") {
        return Err(ManifestError::UnsupportedCapability(
            manifest.extension.name,
            manifest.extension.provides,
        ));
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

    Ok(ServerSpec {
        name: manifest.extension.name,
        command,
        args: manifest.server.args.clone(),
        cwd: Some(dir.to_string_lossy().into_owned()),
        env: Vec::new(),
    })
}

/// Scan directories for extension manifests: each immediate subdirectory
/// (or the directory itself) containing `kelter-extension.toml` is an
/// extension. Invalid manifests are reported, not silently skipped.
pub fn discover(dirs: &[String]) -> (Vec<ServerSpec>, Vec<ManifestError>) {
    let mut specs = Vec::new();
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
                Ok(spec) => specs.push(spec),
                Err(e) => errors.push(e),
            }
        }
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    (specs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ext(root: &Path, name: &str, provides: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_NAME),
            format!(
                "[extension]\nname = \"{name}\"\ndescription = \"test\"\nprovides = [{provides}]\n\n\
                 [server]\ncommand = \"./run.sh\"\nargs = [\"--flag\"]\n"
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn discovers_tool_extensions_and_resolves_paths() {
        let root = std::env::temp_dir().join(format!("kelter-ext-{}", uuid::Uuid::new_v4()));
        let dir = write_ext(&root, "demo", "\"tools\"");
        let (specs, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "demo");
        // relative command resolved into the extension dir
        assert_eq!(specs[0].command, dir.join("./run.sh").to_string_lossy());
        assert_eq!(specs[0].cwd.as_deref(), Some(&*dir.to_string_lossy()));
    }

    #[test]
    fn refuses_unknown_capabilities() {
        let root = std::env::temp_dir().join(format!("kelter-ext-{}", uuid::Uuid::new_v4()));
        write_ext(&root, "memoryext", "\"memory\"");
        let (specs, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("only supports"));
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let (specs, errors) = discover(&["/nonexistent/kelter-ext".to_string()]);
        assert!(specs.is_empty());
        assert!(errors.is_empty());
    }
}
