//! The `workspace_write` host capability for WASM tools (K12 finding N1).
//!
//! A WASM module has no filesystem (ADR 0011). When — and only when — its
//! manifest grants `[permissions] workspace_write = true`, the host defines
//! one import, `kelter:fs.kelter_ws_write`, backed by [`WorkspaceWriter`].
//! A module importing it without the grant cannot be instantiated: the
//! capability is structurally absent, exactly like `kelter:net`.
//!
//! **The host is the enforcement point, not the guest.** The guest is
//! untrusted; whatever path it passes, the checks below run on the host side:
//!
//! - the path is relative, made only of normal components — no `..`, no `.`,
//!   no root, no drive prefix, no backslash, no NUL;
//! - no existing component on the way (directory or file) may be a symlink,
//!   so a link planted in the workspace cannot redirect a write outside it;
//! - an existing file is replaced only when the caller explicitly asked to
//!   overwrite; otherwise creation is atomic no-clobber (`create_new`);
//! - an overwrite goes through a fresh temp file + `rename`, which replaces
//!   the directory entry itself and never follows a link at the target;
//! - content is capped at [`HOST_MAX_WRITE_BYTES`].
//!
//! Residual (documented, not pretended away): the symlink checks are
//! check-then-act. A hostile *local process* racing the daemon inside its
//! own workspace could swap a directory for a link between check and write.
//! The workspace is the daemon's own directory; a co-resident attacker with
//! write access to it is outside this capability's threat model.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Hard host-side cap on one write. A guest may impose a smaller limit of
/// its own, never a larger one.
pub const HOST_MAX_WRITE_BYTES: usize = 1024 * 1024;
/// Cap on the relative path's length, in bytes.
pub const MAX_PATH_BYTES: usize = 512;

/// Import module / function name of the capability, as the guest sees it.
pub const FS_IMPORT_MODULE: &str = "kelter:fs";
pub const FS_WRITE_FN: &str = "kelter_ws_write";

/// `flags` bit: replace an existing file.
pub const FLAG_OVERWRITE: i32 = 1;

/// Return codes of `kelter_ws_write`, shared with the guest. Non-negative =
/// success; negative = refusal. Kept as plain integers so the ABI stays a
/// handful of `i32`s (ADR 0011).
pub mod code {
    pub const CREATED: i32 = 0;
    pub const OVERWRITTEN: i32 = 1;
    pub const INVALID_PATH: i32 = -1;
    pub const EXISTS: i32 = -2;
    pub const TOO_LARGE: i32 = -3;
    pub const ESCAPES_WORKSPACE: i32 = -4;
    pub const IO: i32 = -5;
    pub const NOT_A_FILE: i32 = -6;
    pub const BAD_ARGS: i32 = -7;
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Overwritten,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WsWriteError {
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("file already exists (set overwrite to replace it)")]
    Exists,
    #[error("content too large ({0} bytes, limit {HOST_MAX_WRITE_BYTES})")]
    TooLarge(usize),
    #[error("path escapes the workspace through a symlink")]
    EscapesWorkspace,
    #[error("target exists and is not a regular file")]
    NotAFile,
    #[error("i/o error: {0}")]
    Io(String),
}

impl WsWriteError {
    pub fn code(&self) -> i32 {
        match self {
            Self::InvalidPath(_) => code::INVALID_PATH,
            Self::Exists => code::EXISTS,
            Self::TooLarge(_) => code::TOO_LARGE,
            Self::EscapesWorkspace => code::ESCAPES_WORKSPACE,
            Self::NotAFile => code::NOT_A_FILE,
            Self::Io(_) => code::IO,
        }
    }
}

/// Writes text into one workspace directory, with the checks above.
#[derive(Debug, Clone)]
pub struct WorkspaceWriter {
    root: PathBuf,
}

impl WorkspaceWriter {
    /// `root` is created if missing and canonicalized once, here.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate a guest-supplied relative path into its components.
    fn components(rel: &str) -> Result<Vec<&std::ffi::OsStr>, WsWriteError> {
        let bad = |why: &str| WsWriteError::InvalidPath(why.to_string());
        if rel.is_empty() {
            return Err(bad("empty"));
        }
        if rel.len() > MAX_PATH_BYTES {
            return Err(bad("too long"));
        }
        if rel.contains('\0') {
            return Err(bad("contains NUL"));
        }
        if rel.contains('\\') {
            return Err(bad("contains a backslash — use '/'"));
        }
        let mut out = Vec::new();
        for c in Path::new(rel).components() {
            match c {
                Component::Normal(s) => out.push(s),
                Component::ParentDir => return Err(bad("contains '..'")),
                Component::CurDir => return Err(bad("contains '.'")),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(bad("must be relative to the workspace"))
                }
            }
        }
        if out.is_empty() {
            return Err(bad("names no file"));
        }
        Ok(out)
    }

    pub fn write(
        &self,
        rel: &str,
        content: &[u8],
        overwrite: bool,
    ) -> Result<WriteOutcome, WsWriteError> {
        let io = |e: std::io::Error| WsWriteError::Io(e.kind().to_string());
        let parts = Self::components(rel)?;
        if content.len() > HOST_MAX_WRITE_BYTES {
            return Err(WsWriteError::TooLarge(content.len()));
        }

        // Walk the parent directories one component at a time, never
        // following a link: create what is missing, refuse any symlink.
        let (file_name, dirs) = parts.split_last().expect("non-empty");
        let mut dir = self.root.clone();
        for d in dirs {
            dir.push(d);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => return Err(WsWriteError::EscapesWorkspace),
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(WsWriteError::NotAFile),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&dir).map_err(io)?
                }
                Err(e) => return Err(io(e)),
            }
        }

        let target = dir.join(file_name);
        let existing = match std::fs::symlink_metadata(&target) {
            Ok(m) if m.file_type().is_symlink() => return Err(WsWriteError::EscapesWorkspace),
            Ok(m) if !m.is_file() => return Err(WsWriteError::NotAFile),
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(io(e)),
        };

        if !overwrite {
            if existing {
                return Err(WsWriteError::Exists);
            }
            // Atomic no-clobber: fails if anything (file or link) appeared
            // at the target since the check above.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        WsWriteError::Exists
                    } else {
                        io(e)
                    }
                })?;
            f.write_all(content).map_err(io)?;
            return Ok(WriteOutcome::Created);
        }

        // Overwrite (or create) via temp + rename: the rename replaces the
        // directory entry itself, so a link swapped in at the target is
        // replaced, never followed. Readers never see a half-written file.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(
            ".{}.kelter-tmp-{nanos}",
            file_name.to_string_lossy()
        ));
        let res = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &target)
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        Ok(if existing {
            WriteOutcome::Overwritten
        } else {
            WriteOutcome::Created
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> WorkspaceWriter {
        let dir = std::env::temp_dir().join(format!("kelter-ws-{}", uuid::Uuid::new_v4()));
        WorkspaceWriter::new(dir).unwrap()
    }

    #[test]
    fn writes_new_file_and_creates_parents() {
        let w = ws();
        assert_eq!(
            w.write("a/b/c.txt", b"hello", false).unwrap(),
            WriteOutcome::Created
        );
        assert_eq!(std::fs::read(w.root().join("a/b/c.txt")).unwrap(), b"hello");
    }

    #[test]
    fn refuses_traversal_absolute_and_odd_paths() {
        let w = ws();
        for p in [
            "../x",
            "a/../../x",
            "a/..",
            "/etc/passwd",
            "./x",
            "a\\b",
            "",
            "a\0b",
        ] {
            assert!(
                matches!(w.write(p, b"x", false), Err(WsWriteError::InvalidPath(_))),
                "{p:?} must be refused"
            );
        }
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(matches!(
            w.write(&long, b"x", false),
            Err(WsWriteError::InvalidPath(_))
        ));
    }

    #[test]
    fn no_overwrite_without_the_flag() {
        let w = ws();
        w.write("f.txt", b"one", false).unwrap();
        assert_eq!(w.write("f.txt", b"two", false), Err(WsWriteError::Exists));
        assert_eq!(std::fs::read(w.root().join("f.txt")).unwrap(), b"one");
        assert_eq!(
            w.write("f.txt", b"two", true).unwrap(),
            WriteOutcome::Overwritten
        );
        assert_eq!(std::fs::read(w.root().join("f.txt")).unwrap(), b"two");
    }

    #[test]
    fn size_limit_is_enforced() {
        let w = ws();
        let big = vec![b'x'; HOST_MAX_WRITE_BYTES + 1];
        assert_eq!(
            w.write("big.txt", &big, false),
            Err(WsWriteError::TooLarge(HOST_MAX_WRITE_BYTES + 1))
        );
        assert!(!w.root().join("big.txt").exists());
        let max = vec![b'x'; HOST_MAX_WRITE_BYTES];
        w.write("max.txt", &max, false).unwrap();
    }

    #[test]
    fn refuses_writing_onto_a_directory() {
        let w = ws();
        std::fs::create_dir(w.root().join("d")).unwrap();
        assert_eq!(w.write("d", b"x", true), Err(WsWriteError::NotAFile));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_a_write_outside() {
        let w = ws();
        let outside = std::env::temp_dir().join(format!("kelter-out-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();

        // A linked directory on the way…
        std::os::unix::fs::symlink(&outside, w.root().join("link")).unwrap();
        assert_eq!(
            w.write("link/x.txt", b"x", false),
            Err(WsWriteError::EscapesWorkspace)
        );
        // …or a link as the target itself, with or without overwrite.
        std::os::unix::fs::symlink(outside.join("t.txt"), w.root().join("t.txt")).unwrap();
        assert_eq!(
            w.write("t.txt", b"x", true),
            Err(WsWriteError::EscapesWorkspace)
        );
        assert_eq!(
            w.write("t.txt", b"x", false),
            Err(WsWriteError::EscapesWorkspace)
        );
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }
}
