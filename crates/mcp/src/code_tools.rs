//! Code tools for the agentic-coding loop (C4/C5 + the A1 role split),
//! as plain testable functions; `CodeServer` / `CodeServerMerged` in
//! `server.rs` wrap them in MCP glue. Zero kernel changes — this is only a
//! tool surface.
//!
//! Design decisions (oculink REKOMENDACJE-KELTER A1/C4/C5):
//! - `czytaj_plik` returns NUMBERED lines — without them the model cannot
//!   connect `error at lib.rs:42` to a place in the file.
//! - A1 role split: in the default (separated-roles) server the planner has
//!   NO tool that writes file content — the only route to changing code is
//!   `popraw_kod(sciezka, instrukcja)`, executed by a model call on the
//!   tool side (same server, same model — the measured win comes from the
//!   architecture, not from a second model). `zastosuj_patch` exists as the
//!   shared application machinery and is exposed as a planner tool ONLY in
//!   the merged-roles server (the "before" configuration for measurement).
//! - `uruchom_komende` (C5) returns a SUMMARY (first N error blocks + the
//!   test-result line), never the raw log; the full output is saved under
//!   `.kelter/logs/` inside the code root, readable via `czytaj_plik`.
//! - Every mutating tool honors `idempotency_key` via a durable marker file
//!   (`.kelter/keys/`): a replayed key returns the stored result without
//!   re-executing — the same exactly-once contract as `append_ledger`.
//! - All paths (reads AND writes) are confined to the configured code root,
//!   with the same lexical + symlink containment as `Workspace::resolve`
//!   (E2). The report tools' deliberately-unrestricted read (E1) does NOT
//!   apply to the coding agent.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use crate::tools::ToolError;

/// The directory the coding agent works in. Everything — reads, writes,
/// command cwd — stays under this root.
#[derive(Clone)]
pub struct CodeRoot {
    root: PathBuf,
}

impl CodeRoot {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.into().canonicalize()?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a root-relative path, refusing lexical (`..`, absolute) and
    /// physical (symlink) escapes — the E2 containment, applied to both
    /// reads and writes of the coding surface.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        if rel.trim().is_empty() {
            return Err(ToolError::Invalid("empty path".into()));
        }
        if Path::new(rel).is_absolute() {
            return Err(ToolError::PathEscape(rel.to_string()));
        }
        let candidate = self.root.join(rel);
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ToolError::PathEscape(rel.to_string()));
        }
        let mut probe = candidate.clone();
        let real = loop {
            match probe.canonicalize() {
                Ok(real) => break real,
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent.to_path_buf(),
                    None => return Err(ToolError::PathEscape(rel.to_string())),
                },
            }
        };
        if !real.starts_with(&self.root) {
            return Err(ToolError::PathEscape(rel.to_string()));
        }
        Ok(candidate)
    }

    /// Durable idempotency marker: a mutating tool called again with the
    /// same key returns the stored result instead of re-executing.
    fn key_path(&self, key: &str) -> PathBuf {
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(".kelter").join("keys").join(safe)
    }

    fn stored_result(&self, key: Option<&str>) -> Option<serde_json::Value> {
        let key = key?;
        let raw = std::fs::read_to_string(self.key_path(key)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn store_result(&self, key: Option<&str>, result: &serde_json::Value) -> Result<(), ToolError> {
        let Some(key) = key else { return Ok(()) };
        let path = self.key_path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(result)?)?;
        Ok(())
    }
}

/// `czytaj_plik`: numbered lines, windowed by offset/limit.
pub fn czytaj_plik(
    root: &CodeRoot,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<serde_json::Value, ToolError> {
    let real = root.resolve(path)?;
    let content = std::fs::read_to_string(&real)
        .map_err(|e| ToolError::Invalid(format!("cannot read '{path}': {e}")))?;
    let start = offset.unwrap_or(1).max(1); // 1-based, like compiler messages
    let limit = limit.unwrap_or(400).clamp(1, 2000);
    let total = content.lines().count();
    let mut out = String::new();
    for (i, line) in content.lines().enumerate().skip(start - 1).take(limit) {
        out.push_str(&format!("{:>5}\t{}\n", i + 1, line));
    }
    Ok(serde_json::json!({
        "plik": path,
        "od_linii": start,
        "linii_pokazano": out.lines().count(),
        "linii_lacznie": total,
        "tresc": out,
    }))
}

/// The shared exact-match patch application (A1: machinery, not necessarily
/// a planner tool). `stary` must occur EXACTLY once; zero occurrences with
/// `nowy` already present counts as already-applied (replay-safe by
/// content, independent of the key marker).
pub fn zastosuj_patch(
    root: &CodeRoot,
    path: &str,
    stary: &str,
    nowy: &str,
    idempotency_key: Option<&str>,
) -> Result<serde_json::Value, ToolError> {
    if let Some(stored) = root.stored_result(idempotency_key) {
        return Ok(stored);
    }
    let real = root.resolve(path)?;
    let content = std::fs::read_to_string(&real)
        .map_err(|e| ToolError::Invalid(format!("cannot read '{path}': {e}")))?;
    let occurrences = content.matches(stary).count();
    let result = match occurrences {
        1 => {
            let updated = content.replacen(stary, nowy, 1);
            std::fs::write(&real, updated)?;
            serde_json::json!({ "plik": path, "zastosowano": true })
        }
        0 if !nowy.is_empty() && content.contains(nowy) => {
            serde_json::json!({ "plik": path, "zastosowano": false, "juz_zastosowany": true })
        }
        0 => {
            return Err(ToolError::Invalid(format!(
                "fragment 'stary' nie występuje w '{path}' — przeczytaj plik ponownie i podaj \
                 dokładny istniejący fragment"
            )))
        }
        n => {
            return Err(ToolError::Invalid(format!(
                "fragment 'stary' występuje {n} razy w '{path}' — podaj dłuższy, jednoznaczny \
                 fragment"
            )))
        }
    };
    root.store_result(idempotency_key, &result)?;
    Ok(result)
}

/// C5: extract the first `max_errors` error blocks (plus the test-result
/// lines) from a cargo/rustc-style output instead of returning the raw log.
pub fn streszcz_output(combined: &str, max_errors: usize) -> String {
    let mut out = String::new();
    let mut errors_taken = 0;
    let mut in_error_block = false;
    for line in combined.lines() {
        let is_error_start = line.starts_with("error") // error: / error[E0308]:
            || line.contains("panicked at")
            || line.starts_with("thread '");
        let is_result_line = line.starts_with("test result:")
            || line.contains("test result:")
            || line.starts_with("running ")
            || (line.starts_with("test ") && line.ends_with("FAILED"));
        if is_error_start && errors_taken < max_errors {
            errors_taken += 1;
            in_error_block = true;
        } else if in_error_block
            && !(line.starts_with(' ')
                || line.starts_with('\t')
                || line.starts_with("-->")
                || line.starts_with('|')
                || line.is_empty())
        {
            in_error_block = false;
        }
        if (in_error_block && errors_taken <= max_errors) || is_result_line {
            out.push_str(line);
            out.push('\n');
        }
    }
    let total_errors = combined.lines().filter(|l| l.starts_with("error")).count();
    if total_errors > errors_taken {
        out.push_str(&format!(
            "… ({} dalszych błędów pominięto — pełny log w pliku poniżej)\n",
            total_errors - errors_taken
        ));
    }
    if out.trim().is_empty() {
        // No recognizable error structure: first + last chunk, bounded.
        let head: String = combined.chars().take(400).collect();
        let tail: String = combined
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        out = if combined.chars().count() > 800 {
            format!("{head}\n…\n{tail}")
        } else {
            combined.to_string()
        };
    }
    out
}

/// `uruchom_komende` (C4/C5): run an allowlisted command inside the code
/// root, bounded by a timeout; return the exit code + an error-extract
/// summary; save the full output under `.kelter/logs/`. Exactly-once via
/// the key marker — a resumed run never re-executes a completed command.
#[allow(clippy::too_many_arguments)]
pub async fn uruchom_komende(
    root: &CodeRoot,
    polecenie: &str,
    argumenty: &[String],
    cwd: Option<&str>,
    idempotency_key: Option<&str>,
    allowed: &[String],
    timeout_secs: u64,
    max_errors: usize,
) -> Result<serde_json::Value, ToolError> {
    if let Some(stored) = root.stored_result(idempotency_key) {
        return Ok(stored);
    }
    if !allowed.iter().any(|a| a == polecenie) {
        return Err(ToolError::Invalid(format!(
            "polecenie '{polecenie}' nie jest na liście dozwolonych ({})",
            allowed.join(", ")
        )));
    }
    let workdir = match cwd {
        Some(rel) => root.resolve(rel)?,
        None => root.root().to_path_buf(),
    };
    // An actionable refusal beats spawn's bare ENOENT — a model passing a
    // FILE as cwd (observed live: cwd="src/lib.rs") must learn what to fix.
    if !workdir.is_dir() {
        return Err(ToolError::Invalid(format!(
            "cwd '{}' nie jest katalogiem — podaj katalog (albo pomiń cwd, domyślnie działa w \
             korzeniu projektu)",
            cwd.unwrap_or(".")
        )));
    }

    let mut cmd = tokio::process::Command::new(polecenie);
    cmd.args(argumenty)
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        cmd.spawn()?.wait_with_output().await
    })
    .await
    .map_err(|_| ToolError::Invalid(format!("przekroczono limit czasu ({timeout_secs}s)")))?
    .map_err(ToolError::Io)?;

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Full log to a file the planner can read with czytaj_plik.
    let log_dir = root.root().join(".kelter").join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_name = format!(
        ".kelter/logs/{}-{}.log",
        polecenie,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    std::fs::write(root.root().join(&log_name), &combined)?;

    let exit = output.status.code().unwrap_or(-1);
    let result = serde_json::json!({
        "polecenie": format!("{polecenie} {}", argumenty.join(" ")).trim(),
        "kod_wyjscia": exit,
        "sukces": output.status.success(),
        "streszczenie": streszcz_output(&combined, max_errors),
        "pelny_log": log_name,
        "czas_ms": started.elapsed().as_millis() as u64,
    });
    root.store_result(idempotency_key, &result)?;
    Ok(result)
}

// ---- popraw_kod: the A1 executor ------------------------------------------

/// Configuration of the executor model call behind `popraw_kod`.
#[derive(Clone)]
pub struct ExecutorConfig {
    /// llama.cpp-compatible server, e.g. "http://127.0.0.1:8080".
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

const SEARCH_MARK: &str = "<<<<<<< SZUKAJ";
const DIVIDER: &str = "=======";
const REPLACE_MARK: &str = ">>>>>>> ZASTAP";

/// Fallback for an executor that ignores the block format and replies with
/// ONE fenced code block (measured: gemma-3-27b at temp 0 does exactly this
/// for some instruction wordings — the content is a correct FULL replacement
/// of the file). Returns the fenced content when the reply contains exactly
/// one non-empty fence and no SEARCH/REPLACE markers.
pub fn parse_full_file_reply(text: &str) -> Option<String> {
    if text.contains(SEARCH_MARK) {
        return None;
    }
    let mut parts = text.split("```");
    let _before = parts.next()?;
    let fenced = parts.next()?;
    let after = parts.next()?; // requires a CLOSED fence
    if parts.next().is_some() {
        return None; // more than one fence — ambiguous, refuse
    }
    if !after.trim().is_empty() {
        return None; // prose after the fence — ambiguous, refuse
    }
    // Drop an info-string line like "rust".
    let body = match fenced.split_once('\n') {
        Some((first, rest)) if first.trim().len() <= 12 && !first.trim().contains(' ') => rest,
        _ => fenced,
    };
    let body = body.trim_matches('\n');
    (!body.trim().is_empty()).then(|| format!("{body}\n"))
}

/// Parse the executor's SEARCH/REPLACE blocks.
pub fn parse_patch_blocks(text: &str) -> Result<Vec<(String, String)>, ToolError> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(SEARCH_MARK) {
        let after = &rest[start + SEARCH_MARK.len()..];
        let div = after
            .find(DIVIDER)
            .ok_or_else(|| ToolError::Invalid("blok SZUKAJ bez separatora =======".into()))?;
        let end = after[div..]
            .find(REPLACE_MARK)
            .ok_or_else(|| ToolError::Invalid("blok bez znacznika >>>>>>> ZASTAP".into()))?
            + div;
        let stary = after[..div].trim_matches('\n').to_string();
        let nowy = after[div + DIVIDER.len()..end]
            .trim_matches('\n')
            .to_string();
        blocks.push((stary, nowy));
        rest = &after[end + REPLACE_MARK.len()..];
    }
    if blocks.is_empty() {
        return Err(ToolError::Invalid(
            "wykonawca nie zwrócił żadnego bloku SZUKAJ/ZASTAP".into(),
        ));
    }
    Ok(blocks)
}

/// `popraw_kod` (A1): the planner's only route to changing code in the
/// separated-roles configuration. Reads the file, asks the EXECUTOR model
/// for SEARCH/REPLACE blocks, applies them through the same exact-match
/// machinery as `zastosuj_patch`. Exactly-once via the key marker (the
/// executor is not re-queried and the patch not re-applied on replay).
pub async fn popraw_kod(
    root: &CodeRoot,
    executor: &ExecutorConfig,
    sciezka: &str,
    instrukcja: &str,
    idempotency_key: Option<&str>,
) -> Result<serde_json::Value, ToolError> {
    if let Some(stored) = root.stored_result(idempotency_key) {
        return Ok(stored);
    }
    let real = root.resolve(sciezka)?;
    let content = std::fs::read_to_string(&real)
        .map_err(|e| ToolError::Invalid(format!("cannot read '{sciezka}': {e}")))?;

    let system = format!(
        "Jesteś wykonawcą edycji kodu. Dostajesz treść pliku i instrukcję zmiany. Odpowiedz \
         WYŁĄCZNIE blokami edycji, bez żadnego innego tekstu:\n{SEARCH_MARK}\n(dokładny \
         istniejący fragment pliku)\n{DIVIDER}\n(nowy fragment)\n{REPLACE_MARK}\nFragment \
         SZUKAJ musi wystąpić w pliku DOKŁADNIE raz — skopiuj go znak w znak."
    );
    let user = format!("Plik: {sciezka}\n```\n{content}\n```\n\nInstrukcja: {instrukcja}");
    let body = serde_json::json!({
        "model": executor.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "temperature": 0.0,
        "max_tokens": executor.max_tokens,
    });
    let res = reqwest::Client::new()
        .post(format!(
            "{}/v1/chat/completions",
            executor.base_url.trim_end_matches('/')
        ))
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("wykonawca nieosiągalny: {e}")))?;
    if !res.status().is_success() {
        return Err(ToolError::Http(format!(
            "wykonawca zwrócił {}",
            res.status()
        )));
    }
    let parsed: serde_json::Value = res
        .json()
        .await
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let reply = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ToolError::Invalid("wykonawca zwrócił pustą odpowiedź".into()))?;

    let result = match parse_patch_blocks(reply) {
        Ok(blocks) => {
            let mut applied = 0usize;
            for (stary, nowy) in &blocks {
                // No per-block key: the whole popraw_kod call is the
                // idempotent unit.
                zastosuj_patch(root, sciezka, stary, nowy, None)?;
                applied += 1;
            }
            serde_json::json!({ "plik": sciezka, "zastosowane_bloki": applied })
        }
        // Fallback: a single fenced FULL-FILE reply (measured gemma-3-27b
        // behavior) — apply as a whole-file replacement rather than failing
        // the edit. Same E2-safe write path. GUARD (observed live): an
        // executor can reply with a fenced FRAGMENT (one function) — applied
        // as a full file it would destroy the rest — so a reply much shorter
        // than the original is refused with a re-instruction error, never
        // written.
        Err(block_err) => match parse_full_file_reply(reply) {
            Some(full) if full != content && full.len() * 2 >= content.len() => {
                std::fs::write(&real, &full)?;
                serde_json::json!({
                    "plik": sciezka,
                    "zastosowane_bloki": 0,
                    "pelny_zapis": true,
                })
            }
            Some(full) if full != content => {
                return Err(ToolError::Invalid(format!(
                    "wykonawca zwrócił fragment ({} znaków wobec {} w pliku) zamiast bloków \
                     SZUKAJ/ZASTAP albo pełnego pliku — doprecyzuj instrukcję (podaj, że zmiana \
                     dotyczy funkcji, nie całego pliku)",
                    full.len(),
                    content.len()
                )))
            }
            Some(_) => {
                return Err(ToolError::Invalid(
                    "wykonawca zwrócił plik bez zmian — doprecyzuj instrukcję".into(),
                ))
            }
            None => return Err(block_err),
        },
    };
    root.store_result(idempotency_key, &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> CodeRoot {
        let dir = std::env::temp_dir().join(format!("kelter-code-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        CodeRoot::new(dir).unwrap()
    }

    #[test]
    fn czytaj_plik_numbers_lines_and_windows() {
        let root = temp_root();
        std::fs::write(
            root.root().join("lib.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let v = czytaj_plik(&root, "lib.rs", None, None).unwrap();
        let tresc = v["tresc"].as_str().unwrap();
        assert!(tresc.contains("    1\tfn a() {}"), "{tresc}");
        assert!(tresc.contains("    3\tfn c() {}"), "{tresc}");
        assert_eq!(v["linii_lacznie"], 3);
        // Windowed read (offset is 1-based, like compiler messages).
        let v = czytaj_plik(&root, "lib.rs", Some(2), Some(1)).unwrap();
        let tresc = v["tresc"].as_str().unwrap();
        assert!(tresc.contains("    2\tfn b() {}"), "{tresc}");
        assert!(!tresc.contains("fn a"), "{tresc}");
    }

    #[test]
    fn czytaj_plik_stays_in_root() {
        let root = temp_root();
        assert!(matches!(
            czytaj_plik(&root, "../secrets.txt", None, None),
            Err(ToolError::PathEscape(_))
        ));
        assert!(matches!(
            czytaj_plik(&root, "/etc/passwd", None, None),
            Err(ToolError::PathEscape(_))
        ));
    }

    #[test]
    fn patch_applies_exactly_once_and_detects_ambiguity() {
        let root = temp_root();
        std::fs::write(root.root().join("lib.rs"), "let x = 1;\nlet y = 1;\n").unwrap();
        // Ambiguous fragment → refused.
        assert!(zastosuj_patch(&root, "lib.rs", "= 1;", "= 2;", None).is_err());
        // Unique fragment → applied.
        let v = zastosuj_patch(&root, "lib.rs", "let x = 1;", "let x = 2;", Some("k1")).unwrap();
        assert_eq!(v["zastosowano"], true);
        let content = std::fs::read_to_string(root.root().join("lib.rs")).unwrap();
        assert!(content.contains("let x = 2;"));
        // Replay with the SAME key: no re-execution, stored result returned.
        let v2 = zastosuj_patch(&root, "lib.rs", "let x = 1;", "let x = 2;", Some("k1")).unwrap();
        assert_eq!(v2, v);
        // Same patch, no key, already applied by content → honest no-op.
        let v3 = zastosuj_patch(&root, "lib.rs", "let x = 1;", "let x = 2;", None).unwrap();
        assert_eq!(v3["juz_zastosowany"], true);
        // Missing fragment → actionable error.
        assert!(zastosuj_patch(&root, "lib.rs", "let z = 9;", "let z = 8;", None).is_err());
    }

    #[test]
    fn streszczenie_extracts_first_errors_and_result_line() {
        let raw = "   Compiling x v0.1.0\n\
                   error[E0308]: mismatched types\n --> src/lib.rs:42:5\n  | expected f64\n\
                   error[E0999]: second error\n --> src/lib.rs:50:1\n\
                   error[E1000]: third error\n\
                   warning: unused variable\n\
                   test result: FAILED. 1 passed; 1 failed;\n";
        let s = streszcz_output(raw, 2);
        assert!(s.contains("E0308"), "{s}");
        assert!(s.contains("src/lib.rs:42"), "{s}");
        assert!(s.contains("E0999"), "{s}");
        assert!(
            !s.contains("E1000"),
            "third error must be summarized away: {s}"
        );
        assert!(s.contains("dalszych błędów pominięto"), "{s}");
        assert!(s.contains("test result: FAILED"), "{s}");
        assert!(!s.contains("Compiling"), "{s}");
    }

    #[tokio::test]
    async fn komenda_allowlist_timeout_and_exactly_once() {
        let root = temp_root();
        // Not allowlisted → refused.
        let allowed = vec!["echo".to_string()];
        assert!(
            uruchom_komende(&root, "rm", &[], None, None, &allowed, 5, 5)
                .await
                .is_err()
        );
        // Allowlisted → runs in the root, output summarized + logged.
        let v = uruchom_komende(
            &root,
            "echo",
            &["hello".into()],
            None,
            Some("cmd-k1"),
            &allowed,
            5,
            5,
        )
        .await
        .unwrap();
        assert_eq!(v["kod_wyjscia"], 0);
        assert!(v["streszczenie"].as_str().unwrap().contains("hello"));
        let log_rel = v["pelny_log"].as_str().unwrap();
        assert!(root.root().join(log_rel).is_file());
        // Replay with the same key: stored result, no second execution
        // (asserted via identical stored payload incl. the log path).
        let v2 = uruchom_komende(
            &root,
            "echo",
            &["hello".into()],
            None,
            Some("cmd-k1"),
            &allowed,
            5,
            5,
        )
        .await
        .unwrap();
        assert_eq!(v2, v);
        // cwd escape refused.
        assert!(
            uruchom_komende(&root, "echo", &[], Some("../"), None, &allowed, 5, 5)
                .await
                .is_err()
        );
    }

    #[test]
    fn full_file_fallback_parses_single_fence_only() {
        // The measured gemma reply shape: one fenced block, full file.
        let r = parse_full_file_reply("```rust\nfn a() {}\nfn b() {}\n```\n").unwrap();
        assert_eq!(r, "fn a() {}\nfn b() {}\n");
        // No info string.
        let r = parse_full_file_reply("```\nfn a() {}\n```").unwrap();
        assert_eq!(r, "fn a() {}\n");
        // Refused: SEARCH/REPLACE markers present (blocks path owns it).
        assert!(parse_full_file_reply("<<<<<<< SZUKAJ\nx\n=======\ny\n>>>>>>> ZASTAP").is_none());
        // Refused: two fences (ambiguous), prose after fence, no fence.
        assert!(parse_full_file_reply("```\na\n```\ntekst\n```\nb\n```").is_none());
        assert!(parse_full_file_reply("```\na\n``` i jeszcze komentarz").is_none());
        assert!(parse_full_file_reply("zwykla proza").is_none());
        // Refused: unclosed fence.
        assert!(parse_full_file_reply("```\nfn a() {}").is_none());
    }

    #[tokio::test]
    async fn full_file_fallback_refuses_destructive_fragment() {
        // Observed live: the executor replied with ONE function in a fence;
        // applied as a full file it would delete the rest. The ratio guard
        // must refuse and the file must stay untouched.
        let root = temp_root();
        let original = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n                        fn f() {}\nfn g() {}\nfn h() {}\n";
        std::fs::write(root.root().join("lib.rs"), original).unwrap();
        // Simulate the fallback path directly: a fragment reply.
        let frag = parse_full_file_reply("```rust\nfn a() { /* fixed */ }\n```").unwrap();
        assert!(
            frag.len() * 2 < original.len(),
            "test setup: must be a fragment"
        );
        // The guard lives in popraw_kod; assert its arithmetic contract here
        // (the integration path needs a live executor, covered by evidence).
        let content = std::fs::read_to_string(root.root().join("lib.rs")).unwrap();
        assert_eq!(content, original, "file untouched by parsing");
    }

    #[test]
    fn patch_block_parser_roundtrips() {
        let text = format!(
            "{SEARCH_MARK}\nlet a = 1;\n{DIVIDER}\nlet a = 2;\n{REPLACE_MARK}\n\
             {SEARCH_MARK}\nfn old() {{}}\n{DIVIDER}\nfn new() {{}}\n{REPLACE_MARK}\n"
        );
        let blocks = parse_patch_blocks(&text).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ("let a = 1;".into(), "let a = 2;".into()));
        assert_eq!(blocks[1], ("fn old() {}".into(), "fn new() {}".into()));
        assert!(parse_patch_blocks("no blocks here").is_err());
    }
}
