//! Skills: procedural knowledge in the OpenSkills / Anthropic `SKILL.md`
//! format — **advisory, never authorizing** (ADR 0019).
//!
//! The architectural stance, held to the letter:
//!
//! - A skill's instructions enter the **oracle's context** (progressive
//!   disclosure: name+description always, full instructions only on
//!   activation). They shape what the model *proposes* — nothing else.
//! - A skill's scripts/tools are only **candidate** capabilities: they reach
//!   the action space exclusively through the existing sandbox-by-default
//!   extension registry (`kelter-extension.toml` + the operator's two-man
//!   opt-in for unsandboxed subprocesses). Activation never executes
//!   anything and never widens what the kernel accepts.
//! - The kernel remains the only validator: every proposed action is
//!   re-gated against the action schema regardless of any active skill.
//!   Structurally enforced here: [`SkillContextOracle`] passes the kernel's
//!   `ActionSchema` through untouched — there is no code path by which skill
//!   content can reach it.
//!
//! Because activation adds knowledge, not capability, letting **anyone add
//! any skill** is safe by construction; provenance/trust is recorded per
//! skill (untrusted by default) and surfaced, never inferred from skill
//! content itself.

mod oracle;
mod registry;

pub use oracle::SkillContextOracle;
pub use registry::{
    InstallReport, SkillRegistry, MAX_CATALOG_BYTES, MAX_INSTRUCTIONS_BYTES, MAX_TOTAL_ACTIVE_BYTES,
};

use std::path::{Path, PathBuf};

/// Hard caps for defensive parsing: a SKILL.md is untrusted input.
pub const MAX_SKILL_FILE_BYTES: u64 = 1024 * 1024; // 1 MiB per SKILL.md
pub const MAX_NAME_LEN: usize = 128;
pub const MAX_DESCRIPTION_LEN: usize = 1024;

/// Where a skill came from — recorded at install time by the SUBSYSTEM, never
/// read from the skill's own frontmatter (an adversarial SKILL.md must not be
/// able to claim its own provenance or trust).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    /// "local-dir" | "url" | "bundle".
    pub source_kind: String,
    /// The directory/URL the skill was installed from.
    pub source: String,
    /// Unix seconds at install.
    pub installed_at: u64,
    /// Trust label. Always `"untrusted"` at install; only an operator edit
    /// of the sidecar file can change it — no API in this crate does.
    pub trust: String,
}

pub const SIDECAR_NAME: &str = ".kelter-skill.toml";

/// A parsed, installed skill. `instructions` are NOT held here — progressive
/// disclosure means the body is read from disk only when the skill is active.
#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    /// The installed skill directory (contains SKILL.md).
    pub dir: PathBuf,
    /// Whether the package carries scripts/ (candidate capability — inert
    /// until the operator routes it through the extension registry).
    pub has_scripts: bool,
    /// Whether the package carries a `kelter-extension.toml` (a candidate
    /// for the EXISTING registry; the skills subsystem itself never loads
    /// it — extension_dirs + the trust gate do).
    pub has_extension_manifest: bool,
    pub provenance: Provenance,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid skill: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("fetch failed: {0}")]
    Fetch(String),
}

/// The parsed frontmatter of a SKILL.md — only the fields the subsystem
/// uses. Unknown keys are tolerated and ignored (the corpus in the wild
/// carries `license`, `allowed-tools`, `origin`, …); none of them grants
/// anything.
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Defensively parse a SKILL.md: `---` frontmatter (a deliberately small
/// `key: value` subset — no YAML engine, no anchors/aliases/tags, so
/// adversarial YAML has nothing to exploit) followed by the Markdown body.
///
/// Frontmatter values may continue over indented follow-up lines (the common
/// folded style in the wild). Anything unparseable is skipped, never fatal —
/// a malformed skill loses metadata, it never gains authority.
pub fn parse_skill_md(raw: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();
    let Some(rest) = raw.strip_prefix("---") else {
        // No frontmatter: whole file is body; name comes from the directory.
        return (fm, raw.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (fm, raw.to_string()); // unterminated → treat as body
    };
    let (front, body) = rest.split_at(end);
    let body = body
        .trim_start_matches("\n---")
        .trim_start_matches(['-', '\n'])
        .to_string();

    let mut current: Option<(String, String)> = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in front.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Continuation: indented line extends the current value.
        if (line.starts_with(' ') || line.starts_with('\t')) && current.is_some() {
            if let Some((_, v)) = current.as_mut() {
                if !v.is_empty() {
                    v.push(' ');
                }
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            if let Some(done) = current.take() {
                fields.push(done);
            }
            let value = v.trim().trim_matches(['"', '\'']);
            // Folded/literal scalars: the value starts on following lines.
            let value = if value == ">" || value == "|" || value == ">-" || value == "|-" {
                String::new()
            } else {
                value.to_string()
            };
            current = Some((k.trim().to_ascii_lowercase(), value));
        }
        // Lines that are neither `key: value` nor continuations are skipped.
    }
    if let Some(done) = current.take() {
        fields.push(done);
    }
    for (k, v) in fields {
        match k.as_str() {
            "name" if !v.is_empty() => fm.name = Some(truncate(&v, MAX_NAME_LEN)),
            "description" if !v.is_empty() => {
                fm.description = Some(truncate(&v, MAX_DESCRIPTION_LEN))
            }
            // Everything else (license, allowed-tools, origin, trust, …) is
            // metadata at best; a frontmatter `trust:` claim is IGNORED by
            // design — provenance is subsystem-recorded only.
            _ => {}
        }
    }
    (fm, body)
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// A valid installable skill directory: contains a readable SKILL.md within
/// the size cap. Returns (frontmatter, dir-derived fallback name).
pub(crate) fn read_skill_dir(dir: &Path) -> Result<(Frontmatter, String), SkillError> {
    let md = dir.join("SKILL.md");
    let meta = std::fs::metadata(&md)
        .map_err(|_| SkillError::Invalid(format!("{}: no SKILL.md", dir.display())))?;
    if meta.len() > MAX_SKILL_FILE_BYTES {
        return Err(SkillError::Invalid(format!(
            "{}: SKILL.md exceeds the {} byte cap",
            dir.display(),
            MAX_SKILL_FILE_BYTES
        )));
    }
    let raw = std::fs::read_to_string(&md)?;
    let (fm, _body) = parse_skill_md(&raw);
    let fallback = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    Ok((fm, fallback))
}

/// Sanitize a skill name into a filesystem directory name (installs are
/// keyed by name; no path traversal via a hostile `name:`).
pub(crate) fn dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "unnamed".into()
    } else {
        truncate(trimmed, 64)
    }
}
