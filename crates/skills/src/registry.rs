//! The skill registry: open installation (any skill, from anywhere), listing,
//! removal, activation state, and the two context products —
//! the always-on catalog (name+description) and the activated instructions —
//! both under explicit budgets (progressive disclosure is mandatory).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    dir_name, parse_skill_md, read_skill_dir, truncate, Provenance, SkillError, SkillManifest,
    SIDECAR_NAME,
};

const ACTIVE_FILE: &str = "active.toml";
/// Per-skill instruction budget injected on activation.
pub const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;
/// Total activated-instructions budget across all active skills.
pub const MAX_TOTAL_ACTIVE_BYTES: usize = 48 * 1024;
/// Catalog budget (name+description lines for every installed skill).
pub const MAX_CATALOG_BYTES: usize = 8 * 1024;

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct ActiveSet {
    active: Vec<String>,
}

#[derive(Debug)]
pub struct InstallReport {
    pub installed: Vec<String>,
    pub skipped: Vec<(String, String)>, // (dir, why)
}

/// Cached view of the registry root, invalidated by mtime — so `skill
/// activate` from the CLI takes effect in a running daemon without restart
/// (the state lives in files; the oracle wrapper re-reads through this).
struct Cache {
    skills: Vec<SkillManifest>,
    active: Vec<String>,
    stamp: Option<SystemTime>,
}

pub struct SkillRegistry {
    root: PathBuf,
    cache: Mutex<Cache>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl SkillRegistry {
    /// Open (and create) the registry at `root`. Never fails on a missing or
    /// hostile skill dir — bad entries are skipped, logged, never fatal.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SkillError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            cache: Mutex::new(Cache {
                skills: Vec::new(),
                active: Vec::new(),
                stamp: None,
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- discovery --------------------------------------------------------

    fn scan(&self) -> (Vec<SkillManifest>, Vec<String>) {
        let mut skills = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return (skills, Vec::new());
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            match self.load_installed(&dir) {
                Ok(m) => skills.push(m),
                Err(e) => tracing::warn!("skills: skipping {}: {e}", dir.display()),
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        let active = self.read_active();
        (skills, active)
    }

    fn load_installed(&self, dir: &Path) -> Result<SkillManifest, SkillError> {
        let (fm, fallback) = read_skill_dir(dir)?;
        let provenance = self.read_sidecar(dir);
        Ok(SkillManifest {
            name: fm.name.unwrap_or(fallback),
            description: fm.description.unwrap_or_default(),
            has_scripts: dir.join("scripts").is_dir(),
            has_extension_manifest: dir.join("kelter-extension.toml").is_file(),
            dir: dir.to_path_buf(),
            provenance,
        })
    }

    fn read_sidecar(&self, dir: &Path) -> Provenance {
        std::fs::read_to_string(dir.join(SIDECAR_NAME))
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_else(|| Provenance {
                source_kind: "unknown".into(),
                source: "(no install record)".into(),
                installed_at: 0,
                trust: "untrusted".into(),
            })
    }

    /// Registry state stamp: newest mtime of the root + active file. Cheap;
    /// lets the oracle wrapper stay current without a watcher.
    fn stamp(&self) -> Option<SystemTime> {
        let root = std::fs::metadata(&self.root)
            .and_then(|m| m.modified())
            .ok();
        let active = std::fs::metadata(self.root.join(ACTIVE_FILE))
            .and_then(|m| m.modified())
            .ok();
        match (root, active) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    fn with_fresh<R>(&self, f: impl FnOnce(&Vec<SkillManifest>, &Vec<String>) -> R) -> R {
        let mut cache = self.cache.lock().unwrap();
        let stamp = self.stamp();
        if cache.stamp != stamp || stamp.is_none() {
            let (skills, active) = self.scan();
            cache.skills = skills;
            cache.active = active;
            cache.stamp = stamp;
        }
        f(&cache.skills, &cache.active)
    }

    pub fn list(&self) -> Vec<SkillManifest> {
        self.with_fresh(|skills, _| skills.clone())
    }

    pub fn active_names(&self) -> Vec<String> {
        self.with_fresh(|_, active| active.clone())
    }

    // ---- install ----------------------------------------------------------

    /// Install one skill from a local directory containing SKILL.md.
    /// Open by design: ANY well-formed skill installs; nothing here inspects
    /// or privileges content — it only records provenance (untrusted).
    pub fn install_from_dir(&self, src: &Path) -> Result<SkillManifest, SkillError> {
        let (fm, fallback) = read_skill_dir(src)?;
        let name = fm.name.clone().unwrap_or(fallback);
        let dest = self.root.join(dir_name(&name));
        if dest.exists() {
            return Err(SkillError::Invalid(format!(
                "skill '{name}' is already installed (remove it first)"
            )));
        }
        copy_dir(src, &dest)?;
        self.write_sidecar(&dest, "local-dir", &src.display().to_string())?;
        self.touch();
        self.load_installed(&dest)
    }

    /// Install every skill found under `src` (a bundle: a directory whose
    /// subdirectories each contain SKILL.md — e.g. an OpenSkills checkout or
    /// a skillforge bundle). Per-skill failures are reported, never fatal.
    pub fn install_bundle(&self, src: &Path) -> Result<InstallReport, SkillError> {
        let mut report = InstallReport {
            installed: Vec::new(),
            skipped: Vec::new(),
        };
        // A bundle may nest skills under a `skills/` folder.
        let base = if src.join("skills").is_dir() && !src.join("SKILL.md").is_file() {
            src.join("skills")
        } else {
            src.to_path_buf()
        };
        if base.join("SKILL.md").is_file() {
            // A single skill, not a bundle.
            match self.install_from_dir(&base) {
                Ok(m) => report.installed.push(m.name),
                Err(e) => report
                    .skipped
                    .push((base.display().to_string(), e.to_string())),
            }
            return Ok(report);
        }
        for entry in std::fs::read_dir(&base)?.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("SKILL.md").is_file() {
                continue;
            }
            match self.install_from_dir(&dir) {
                Ok(m) => report.installed.push(m.name),
                Err(e) => report
                    .skipped
                    .push((dir.display().to_string(), e.to_string())),
            }
        }
        Ok(report)
    }

    /// Install from a URL: a `.tar.gz`/`.tgz` archive (e.g. a GitHub
    /// release/archive of an OpenSkills or anthropics/skills folder). The
    /// archive is unpacked defensively (size caps, entries jailed under the
    /// staging dir) and then treated exactly like a local bundle.
    pub async fn install_from_url(&self, url: &str) -> Result<InstallReport, SkillError> {
        const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
        let resp = reqwest::get(url)
            .await
            .map_err(|e| SkillError::Fetch(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SkillError::Fetch(format!("{} from {url}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SkillError::Fetch(e.to_string()))?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(SkillError::Fetch(format!(
                "archive exceeds the {MAX_ARCHIVE_BYTES} byte cap"
            )));
        }
        let staging = self.root.join(".staging");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let gz = flate2::read::GzDecoder::new(bytes.as_ref());
        let mut archive = tar::Archive::new(gz);
        // tar::Archive::unpack already refuses entries escaping the target.
        archive
            .unpack(&staging)
            .map_err(|e| SkillError::Fetch(format!("unpack: {e}")))?;
        // The archive may wrap everything in one top-level folder.
        let mut base = staging.clone();
        let tops: Vec<_> = std::fs::read_dir(&staging)?
            .flatten()
            .map(|e| e.path())
            .collect();
        if tops.len() == 1 && tops[0].is_dir() {
            base = tops[0].clone();
        }
        let report = self.install_bundle(&base)?;
        // Re-stamp provenance with the true source.
        for name in &report.installed {
            let dir = self.root.join(dir_name(name));
            let _ = self.write_sidecar(&dir, "url", url);
        }
        let _ = std::fs::remove_dir_all(&staging);
        self.touch();
        Ok(report)
    }

    pub fn remove(&self, name: &str) -> Result<(), SkillError> {
        let dir = self.root.join(dir_name(name));
        if !dir.is_dir() {
            return Err(SkillError::NotFound(name.into()));
        }
        std::fs::remove_dir_all(&dir)?;
        // Deactivate too.
        let mut active = self.read_active();
        active.retain(|n| n != name);
        self.write_active(&active)?;
        self.touch();
        Ok(())
    }

    fn write_sidecar(&self, dir: &Path, kind: &str, source: &str) -> Result<(), SkillError> {
        let prov = Provenance {
            source_kind: kind.into(),
            source: source.into(),
            installed_at: now_secs(),
            trust: "untrusted".into(),
        };
        let raw = toml::to_string_pretty(&prov)
            .map_err(|e| SkillError::Invalid(format!("sidecar: {e}")))?;
        std::fs::write(dir.join(SIDECAR_NAME), raw)?;
        Ok(())
    }

    /// Elevate (or otherwise change) a skill's trust label, preserving its
    /// other provenance fields. ADR 0019 says only an operator's direct edit
    /// of the sidecar file changes trust; ADR 0021 adds this as the ONE
    /// deliberate, explicitly-documented exception — reachable only through
    /// a human's bearer-gated `allow_always` decision on a specific pending
    /// activation request (never model-initiated, never a skill
    /// self-elevating via its own frontmatter, which stays ignored). Takes
    /// effect immediately (no restart) via the existing mtime-based cache
    /// invalidation (`touch`).
    pub fn set_trust(&self, name: &str, trust: &str) -> Result<(), SkillError> {
        let dir = self.root.join(dir_name(name));
        if !dir.is_dir() {
            return Err(SkillError::NotFound(name.into()));
        }
        let mut prov = self.read_sidecar(&dir);
        prov.trust = trust.to_string();
        let raw = toml::to_string_pretty(&prov)
            .map_err(|e| SkillError::Invalid(format!("sidecar: {e}")))?;
        std::fs::write(dir.join(SIDECAR_NAME), raw)?;
        self.touch();
        Ok(())
    }

    fn touch(&self) {
        // Bump the root mtime so with_fresh() reloads even on filesystems
        // with coarse timestamps.
        let _ = std::fs::File::create(self.root.join(".stamp"))
            .and_then(|f| f.set_modified(SystemTime::now()));
        if let Ok(mut cache) = self.cache.lock() {
            cache.stamp = None;
        }
    }

    // ---- activation (context, never authority) ----------------------------

    fn read_active(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join(ACTIVE_FILE))
            .ok()
            .and_then(|raw| toml::from_str::<ActiveSet>(&raw).ok())
            .map(|s| s.active)
            .unwrap_or_default()
    }

    fn write_active(&self, names: &[String]) -> Result<(), SkillError> {
        let raw = toml::to_string_pretty(&ActiveSet {
            active: names.to_vec(),
        })
        .map_err(|e| SkillError::Invalid(format!("active set: {e}")))?;
        std::fs::write(self.root.join(ACTIVE_FILE), raw)?;
        Ok(())
    }

    /// Activate = mark for full-instruction injection into the oracle's
    /// context. Nothing is executed, registered, or authorized here.
    pub fn activate(&self, name: &str) -> Result<(), SkillError> {
        let known = self.list();
        if !known.iter().any(|s| s.name == name) {
            return Err(SkillError::NotFound(name.into()));
        }
        let mut active = self.read_active();
        if !active.iter().any(|n| n == name) {
            active.push(name.to_string());
        }
        self.write_active(&active)?;
        self.touch();
        Ok(())
    }

    pub fn deactivate(&self, name: &str) -> Result<(), SkillError> {
        let mut active = self.read_active();
        active.retain(|n| n != name);
        self.write_active(&active)?;
        self.touch();
        Ok(())
    }

    // ---- the two context products (progressive disclosure) ----------------

    /// The always-available catalog: one `name — description` line per
    /// installed skill, under [`MAX_CATALOG_BYTES`]. This is the DISCOVERY
    /// tier — bodies are never included here.
    pub fn catalog(&self) -> String {
        self.with_fresh(|skills, active| {
            let mut out = String::new();
            for s in skills {
                let mark = if active.contains(&s.name) { "*" } else { "-" };
                let line = format!("{mark} {}: {}\n", s.name, truncate(&s.description, 200));
                if out.len() + line.len() > MAX_CATALOG_BYTES {
                    out.push_str("- … (catalog truncated at budget)\n");
                    break;
                }
                out.push_str(&line);
            }
            out
        })
    }

    /// Full instructions of ACTIVE skills only, each capped at
    /// [`MAX_INSTRUCTIONS_BYTES`], total capped at [`MAX_TOTAL_ACTIVE_BYTES`].
    /// Bodies are read from disk here — the ACTIVATION tier. Delegates to
    /// [`Self::active_instructions_for`] with the operator-active set.
    pub fn active_instructions(&self) -> String {
        self.with_fresh(|skills, active| self.render_instructions_for(skills, active))
    }

    /// Full instructions for an EXPLICIT set of names (e.g. the union of the
    /// operator-active set and a run's self-activated set — ADR 0020), under
    /// the same per-skill and total budgets as [`Self::active_instructions`].
    /// This is the render-time backstop: even if authorization is somehow
    /// bypassed, the rendered context can never exceed the budget.
    pub fn active_instructions_for(&self, names: &[String]) -> String {
        self.with_fresh(|skills, _active| self.render_instructions_for(skills, names))
    }

    fn render_instructions_for(&self, skills: &[SkillManifest], names: &[String]) -> String {
        let mut out = String::new();
        // De-duplicate while preserving first-seen order (a name may appear
        // in both the operator-active and run-active sets).
        let mut seen = std::collections::HashSet::new();
        for name in names {
            if !seen.insert(name.as_str()) {
                continue;
            }
            let Some(skill) = skills.iter().find(|s| &s.name == name) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(skill.dir.join("SKILL.md")) else {
                continue;
            };
            let (_fm, body) = parse_skill_md(&raw);
            let body = truncate(body.trim(), MAX_INSTRUCTIONS_BYTES);
            let block = format!("## Skill: {name}\n{body}\n\n");
            if out.len() + block.len() > MAX_TOTAL_ACTIVE_BYTES {
                tracing::warn!(
                    "skills: active-instructions budget reached; '{name}' truncated/omitted"
                );
                let remaining = MAX_TOTAL_ACTIVE_BYTES.saturating_sub(out.len());
                out.push_str(&truncate(&block, remaining));
                break;
            }
            out.push_str(&block);
        }
        out
    }

    // ---- activation-gate probes (ADR 0020) ---------------------------------

    /// Whether a skill by this exact name is installed.
    pub fn is_installed(&self, name: &str) -> bool {
        self.with_fresh(|skills, _| skills.iter().any(|s| s.name == name))
    }

    /// A skill's recorded trust label, if installed.
    pub fn trust_of(&self, name: &str) -> Option<String> {
        self.with_fresh(|skills, _| {
            skills
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.provenance.trust.clone())
        })
    }

    /// The byte size of a skill's instructions AFTER per-skill truncation —
    /// i.e. exactly what would be added to the rendered context if this skill
    /// were included. Used by the gate's budget math; `None` if not
    /// installed or its SKILL.md cannot be read.
    pub fn instruction_bytes(&self, name: &str) -> Option<usize> {
        self.with_fresh(|skills, _| {
            let skill = skills.iter().find(|s| s.name == name)?;
            let raw = std::fs::read_to_string(skill.dir.join("SKILL.md")).ok()?;
            let (_fm, body) = parse_skill_md(&raw);
            let body = truncate(body.trim(), MAX_INSTRUCTIONS_BYTES);
            Some(format!("## Skill: {name}\n{body}\n\n").len())
        })
    }

    /// Convenience probe for the activation gate: `(trust, instruction_bytes)`
    /// for one installed skill, or `None` if it does not exist.
    pub fn activation_probe(&self, name: &str) -> Option<(String, usize)> {
        let trust = self.trust_of(name)?;
        let bytes = self.instruction_bytes(name).unwrap_or(0);
        Some((trust, bytes))
    }
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), SkillError> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
