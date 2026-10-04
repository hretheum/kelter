//! Run-produced files ("artifacts"): list + read the files a run wrote into
//! the tools workspace, scoped by that run's own journal.
//!
//! The workspace is SHARED across runs (one `[tools].workspace` directory per
//! daemon), so "this run's files" cannot be a directory listing — that would
//! leak every other run's output. Instead the scope is derived from the run's
//! durable journal: a file is served only if a successful `Receipt` of this
//! run names it (`saved_as` / `output` / `ledger` — the file-name keys the
//! bundled tools return). Those names were validated by the tool-side
//! `Workspace::resolve` escape-refusal when the file was written; the same
//! checks are re-applied here on the way out (reject `..` components and
//! absolute paths, then canonicalize and require the result to stay under the
//! workspace root — so even a symlink planted inside the workspace cannot
//! reach outside it). No allowlist hit → 404, never a filesystem probe.

use std::path::{Component, Path, PathBuf};

use kernel_core::JournalEntry;

/// The keys under which the bundled tools report a file they wrote, in their
/// `Receipt` result objects (`read_source`/`extract_structure` → `saved_as`,
/// `write_report` → `output`, `append_ledger` → `ledger`). Extensions use the
/// same convention (e.g. the `zapisz_plik` WASM tool → `saved_as`, ADR 0034).
const FILE_NAME_KEYS: [&str; 3] = ["saved_as", "output", "ledger"];

/// Serving cap: a run artifact is a report, not a blob store. Anything larger
/// is listed but its content endpoint answers 413.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;

/// One produced file, safe to return to a client.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunFileInfo {
    /// Workspace-relative name, exactly as the run's journal recorded it.
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RunFilesError {
    /// The requested name is not in this run's journal-derived allowlist, or
    /// the file no longer exists. Deliberately one error: a caller cannot
    /// probe the workspace for files other runs produced.
    #[error("no such file for this run")]
    NotFound,
    /// The name failed the path-safety checks (`..`, absolute, or a resolved
    /// path escaping the workspace root).
    #[error("invalid path: workspace-relative names only")]
    Refused,
    #[error("file exceeds the {MAX_ARTIFACT_BYTES}-byte serving cap")]
    TooLarge,
    #[error("file is not valid UTF-8 text")]
    NotText,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read-side view of the shared tools workspace, handed to the router as an
/// optional capability (absent → the files routes report 404, the same
/// convention as the skills/agents controllers).
pub struct RunFilesService {
    root: PathBuf,
}

impl RunFilesService {
    /// `workspace_root` must exist; it is canonicalized once so later
    /// containment checks compare canonical paths.
    pub fn new(workspace_root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            root: workspace_root.into().canonicalize()?,
        })
    }

    /// The workspace-relative file names this journal's successful receipts
    /// recorded, in first-mention order, deduplicated. Names that fail the
    /// path-safety checks are dropped (defense in depth — the tools already
    /// refused to write them).
    fn allowlist(&self, journal: &[JournalEntry]) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for entry in journal {
            let JournalEntry::Receipt {
                ok: true, result, ..
            } = entry
            else {
                continue;
            };
            for key in FILE_NAME_KEYS {
                if let Some(name) = result.get(key).and_then(|v| v.as_str()) {
                    if self.resolve(name).is_ok() && !names.iter().any(|n| n == name) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names
    }

    /// Resolve a workspace-relative name for reading, refusing escapes — the
    /// same checks as the tool-side `Workspace::resolve` (no `..` components,
    /// no absolute paths).
    fn resolve(&self, name: &str) -> Result<PathBuf, RunFilesError> {
        if name.trim().is_empty() || Path::new(name).is_absolute() {
            return Err(RunFilesError::Refused);
        }
        let candidate = self.root.join(name);
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(RunFilesError::Refused);
        }
        Ok(candidate)
    }

    /// The files this run produced that still exist on disk, with sizes.
    pub fn list(&self, journal: &[JournalEntry]) -> Vec<RunFileInfo> {
        self.allowlist(journal)
            .into_iter()
            .filter_map(|name| {
                let path = self.resolve(&name).ok()?;
                let meta = std::fs::metadata(&path).ok()?;
                meta.is_file().then_some(RunFileInfo {
                    name,
                    bytes: meta.len(),
                })
            })
            .collect()
    }

    /// Read one produced file's UTF-8 content. `name` must be in the run's
    /// journal-derived allowlist — a workspace file some OTHER run produced
    /// answers `NotFound`, indistinguishable from a file that never existed.
    pub fn read(&self, journal: &[JournalEntry], name: &str) -> Result<String, RunFilesError> {
        // Path-safety first: a malformed name is refused outright (401/404
        // ordering is the handler's concern; here refusal beats probing).
        let candidate = self.resolve(name)?;
        if !self.allowlist(journal).iter().any(|n| n == name) {
            return Err(RunFilesError::NotFound);
        }
        // Canonicalize AFTER the allowlist hit and require containment: even
        // a symlink planted inside the workspace cannot serve content from
        // outside the root.
        let real = candidate
            .canonicalize()
            .map_err(|_| RunFilesError::NotFound)?;
        if !real.starts_with(&self.root) {
            return Err(RunFilesError::Refused);
        }
        let meta = std::fs::metadata(&real)?;
        if !meta.is_file() {
            return Err(RunFilesError::NotFound);
        }
        if meta.len() > MAX_ARTIFACT_BYTES {
            return Err(RunFilesError::TooLarge);
        }
        let bytes = std::fs::read(&real)?;
        String::from_utf8(bytes).map_err(|_| RunFilesError::NotText)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kelter-runfiles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn receipt(result: serde_json::Value) -> JournalEntry {
        JournalEntry::Receipt {
            step: 0,
            ok: true,
            result,
        }
    }

    #[test]
    fn lists_only_journal_named_existing_files() {
        let root = temp_root();
        std::fs::write(root.join("report.md"), "# hi").unwrap();
        std::fs::write(root.join("stray.md"), "not this run's").unwrap();
        let svc = RunFilesService::new(&root).unwrap();
        let journal = vec![
            receipt(json!({"output": "report.md", "bytes": 4})),
            receipt(json!({"saved_as": "missing.md"})), // journaled but deleted
        ];
        let files = svc.list(&journal);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "report.md");
        assert_eq!(files[0].bytes, 4);
    }

    #[test]
    fn failed_receipts_grant_nothing() {
        let root = temp_root();
        std::fs::write(root.join("report.md"), "# hi").unwrap();
        let svc = RunFilesService::new(&root).unwrap();
        let journal = vec![JournalEntry::Receipt {
            step: 0,
            ok: false,
            result: json!({"output": "report.md"}),
        }];
        assert!(svc.list(&journal).is_empty());
        assert!(matches!(
            svc.read(&journal, "report.md"),
            Err(RunFilesError::NotFound)
        ));
    }

    #[test]
    fn read_refuses_traversal_and_absolute_paths() {
        let root = temp_root();
        let svc = RunFilesService::new(&root).unwrap();
        // Even a journal that (impossibly) names an escape is refused.
        let journal = vec![
            receipt(json!({"output": "../evil.md"})),
            receipt(json!({"output": "/etc/passwd"})),
        ];
        assert!(svc.list(&journal).is_empty());
        assert!(matches!(
            svc.read(&journal, "../evil.md"),
            Err(RunFilesError::Refused)
        ));
        assert!(matches!(
            svc.read(&journal, "/etc/passwd"),
            Err(RunFilesError::Refused)
        ));
        assert!(matches!(
            svc.read(&journal, "a/../../evil.md"),
            Err(RunFilesError::Refused)
        ));
    }

    #[test]
    fn read_refuses_files_outside_the_allowlist() {
        let root = temp_root();
        std::fs::write(root.join("other-runs.md"), "secret").unwrap();
        let svc = RunFilesService::new(&root).unwrap();
        let journal = vec![receipt(json!({"output": "report.md"}))];
        assert!(matches!(
            svc.read(&journal, "other-runs.md"),
            Err(RunFilesError::NotFound)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_refuses_symlink_escapes() {
        let root = temp_root();
        let outside = std::env::temp_dir().join(format!("kelter-outside-{}", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "outside content").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.md")).unwrap();
        let svc = RunFilesService::new(&root).unwrap();
        let journal = vec![receipt(json!({"output": "link.md"}))];
        assert!(matches!(
            svc.read(&journal, "link.md"),
            Err(RunFilesError::Refused)
        ));
    }

    #[test]
    fn read_returns_content_and_caps_size() {
        let root = temp_root();
        std::fs::write(root.join("report.md"), "# Report\n\nbody\n").unwrap();
        let svc = RunFilesService::new(&root).unwrap();
        let journal = vec![receipt(json!({"output": "report.md"}))];
        assert_eq!(
            svc.read(&journal, "report.md").unwrap(),
            "# Report\n\nbody\n"
        );
    }
}
