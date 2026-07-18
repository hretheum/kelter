//! The reference tool set for the demo slice, as plain testable functions.
//! MCP server glue wraps these; the logic itself has no protocol deps.
//!
//! Pipeline shape: read a source (URL or local file) → extract structured
//! information → write a report file. Deterministic, so the same task can be
//! compared across policy engines.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(String),
    #[error(
        "invalid path '{0}': use a plain workspace-relative file name (like the value returned \
         in saved_as, e.g. \"raw.md\"), not an absolute path"
    )]
    PathEscape(String),
    #[error("{0}")]
    Invalid(String),
}

/// All tool outputs land inside this directory; relative paths are resolved
/// against it and traversal outside it is rejected.
#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root: PathBuf = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a path for READING: workspace-relative names resolve inside
    /// the workspace; absolute paths to existing files are allowed (reads
    /// are unrestricted, consistent with read_source; only writes are
    /// confined).
    pub fn resolve_read(&self, path: &str) -> Result<PathBuf, ToolError> {
        let p = Path::new(path);
        if p.is_absolute() {
            if p.is_file() {
                return Ok(p.to_path_buf());
            }
            return Err(ToolError::Invalid(format!("file not found: {path}")));
        }
        // Fall back to workspace-relative resolution (no escapes).
        let candidate = self.root.join(path);
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ToolError::PathEscape(path.to_string()));
        }
        Ok(candidate)
    }

    /// Resolve a workspace-relative path for WRITING, refusing escapes.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        if rel.trim().is_empty() {
            return Err(ToolError::Invalid("empty path".into()));
        }
        let candidate = self.root.join(rel);
        // Normalize without requiring existence: reject any `..` component.
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
            || Path::new(rel).is_absolute()
        {
            return Err(ToolError::PathEscape(rel.to_string()));
        }
        Ok(candidate)
    }
}

fn preview(s: &str, max: usize) -> String {
    let mut cut = s.len().min(max);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Tool A: fetch a URL or read a local file, store the raw content in the
/// workspace under `save_as`.
pub async fn read_source(
    ws: &Workspace,
    input: &str,
    save_as: &str,
) -> Result<serde_json::Value, ToolError> {
    let content = if input.starts_with("http://") || input.starts_with("https://") {
        let res = reqwest::get(input)
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        if !res.status().is_success() {
            return Err(ToolError::Http(format!("status {}", res.status())));
        }
        res.text()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?
    } else {
        // Local input files may live outside the workspace (read-only);
        // outputs never do.
        tokio::fs::read_to_string(input).await?
    };

    let dest = ws.resolve(save_as)?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&dest, &content).await?;

    Ok(serde_json::json!({
        "saved_as": save_as,
        "bytes": content.len(),
        "preview": preview(&content, 200),
    }))
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Structure {
    pub title: Option<String>,
    pub headings: Vec<String>,
    pub links: Vec<String>,
    pub word_count: usize,
}

/// Deterministic structure extraction for markdown-ish and HTML-ish text.
pub fn extract(content: &str) -> Structure {
    let mut s = Structure {
        word_count: content.split_whitespace().count(),
        ..Default::default()
    };

    // HTML title
    if let Some(start) = content.find("<title>") {
        if let Some(len) = content[start + 7..].find("</title>") {
            s.title = Some(content[start + 7..start + 7 + len].trim().to_string());
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(h) = trimmed.strip_prefix('#') {
            let text = h.trim_start_matches('#').trim();
            if !text.is_empty() {
                if s.title.is_none() {
                    s.title = Some(text.to_string());
                }
                s.headings.push(text.to_string());
            }
        }
    }

    // HTML headings <h1>-<h3>
    for tag in ["h1", "h2", "h3"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut rest = content;
        while let Some(start) = rest.find(&open) {
            let after = &rest[start..];
            if let (Some(gt), Some(end)) = (after.find('>'), after.find(&close)) {
                if gt < end {
                    let text = after[gt + 1..end].trim();
                    if !text.is_empty() {
                        s.headings.push(text.to_string());
                        if s.title.is_none() {
                            s.title = Some(text.to_string());
                        }
                    }
                }
                rest = &after[end + close.len()..];
            } else {
                break;
            }
        }
    }

    // Markdown links [x](url) and HTML href="url"
    let mut rest = content;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find(')') {
            let url = after[..end].trim();
            if !url.is_empty() {
                s.links.push(url.to_string());
            }
            rest = &after[end..];
        } else {
            break;
        }
    }
    let mut rest = content;
    while let Some(start) = rest.find("href=\"") {
        let after = &rest[start + 6..];
        if let Some(end) = after.find('"') {
            let url = &after[..end];
            if !url.is_empty() && !url.starts_with('#') {
                s.links.push(url.to_string());
            }
            rest = &after[end..];
        } else {
            break;
        }
    }

    s
}

/// Tool B: read a stored source from the workspace, extract structure,
/// save it as JSON under `save_as`.
pub async fn extract_structure(
    ws: &Workspace,
    source: &str,
    save_as: &str,
) -> Result<serde_json::Value, ToolError> {
    let src = ws.resolve_read(source)?;
    let content = tokio::fs::read_to_string(&src).await.map_err(|e| {
        ToolError::Invalid(format!(
            "cannot read '{source}' (use the saved_as name from read_source, e.g. \"raw.md\"): {e}"
        ))
    })?;
    let structure = extract(&content);
    let dest = ws.resolve(save_as)?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&dest, serde_json::to_vec_pretty(&structure)?).await?;

    Ok(serde_json::json!({
        "saved_as": save_as,
        "title": structure.title,
        "headings": structure.headings.len(),
        "links": structure.links.len(),
        "word_count": structure.word_count,
    }))
}

/// Tool C: render the extracted JSON into a final report file.
pub async fn write_report(
    ws: &Workspace,
    source: &str,
    output: &str,
    format: &str,
) -> Result<serde_json::Value, ToolError> {
    let src = ws.resolve_read(source)?;
    let raw = tokio::fs::read_to_string(&src).await.map_err(|e| {
        ToolError::Invalid(format!(
            "cannot read '{source}' (use the saved_as name from extract_structure, e.g. \
             \"structure.json\"): {e}"
        ))
    })?;
    let structure: Structure = serde_json::from_str(&raw)
        .map_err(|e| ToolError::Invalid(format!("'{source}' is not an extraction result: {e}")))?;

    let rendered = match format {
        "json" => raw,
        "markdown" => {
            let mut md = String::new();
            md.push_str(&format!(
                "# Report: {}\n\n",
                structure.title.as_deref().unwrap_or("(untitled)")
            ));
            md.push_str(&format!("- Words: {}\n", structure.word_count));
            md.push_str(&format!("- Headings: {}\n", structure.headings.len()));
            md.push_str(&format!("- Links: {}\n\n", structure.links.len()));
            if !structure.headings.is_empty() {
                md.push_str("## Outline\n\n");
                for h in &structure.headings {
                    md.push_str(&format!("- {h}\n"));
                }
                md.push('\n');
            }
            if !structure.links.is_empty() {
                md.push_str("## Links\n\n");
                for l in &structure.links {
                    md.push_str(&format!("- <{l}>\n"));
                }
            }
            md
        }
        other => {
            return Err(ToolError::Invalid(format!(
                "unknown format '{other}', use \"json\" or \"markdown\""
            )))
        }
    };

    let dest = ws.resolve(output)?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&dest, &rendered).await?;

    Ok(serde_json::json!({
        "output": output,
        "bytes": rendered.len(),
        "format": format,
    }))
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Invalid(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MD: &str = "# Demo Page\n\nIntro text here.\n\n## Section One\n\nSee [docs](https://example.org/docs) and [home](https://example.org).\n\n## Section Two\n\nMore words.\n";

    fn temp_ws() -> Workspace {
        let dir = std::env::temp_dir().join(format!("ak-tools-{}", uuid::Uuid::new_v4()));
        Workspace::new(dir).unwrap()
    }

    #[test]
    fn extract_markdown_structure() {
        let s = extract(SAMPLE_MD);
        assert_eq!(s.title.as_deref(), Some("Demo Page"));
        assert_eq!(s.headings, vec!["Demo Page", "Section One", "Section Two"]);
        assert_eq!(
            s.links,
            vec!["https://example.org/docs", "https://example.org"]
        );
        assert!(s.word_count > 10);
    }

    #[test]
    fn extract_html_structure() {
        let html = "<html><head><title>My Site</title></head><body><h1>Welcome</h1>\
                    <a href=\"https://a.example\">a</a><a href=\"#skip\">skip</a></body></html>";
        let s = extract(html);
        assert_eq!(s.title.as_deref(), Some("My Site"));
        assert_eq!(s.headings, vec!["Welcome"]);
        assert_eq!(s.links, vec!["https://a.example"]);
    }

    #[tokio::test]
    async fn full_pipeline_on_local_file() {
        let ws = temp_ws();
        let input = ws.root().join("input.md");
        tokio::fs::write(&input, SAMPLE_MD).await.unwrap();

        let r1 = read_source(&ws, input.to_str().unwrap(), "raw.md")
            .await
            .unwrap();
        assert_eq!(r1["saved_as"], "raw.md");

        let r2 = extract_structure(&ws, "raw.md", "structure.json")
            .await
            .unwrap();
        assert_eq!(r2["headings"], 3);
        assert_eq!(r2["links"], 2);

        let r3 = write_report(&ws, "structure.json", "report.md", "markdown")
            .await
            .unwrap();
        assert_eq!(r3["output"], "report.md");

        let report = tokio::fs::read_to_string(ws.root().join("report.md"))
            .await
            .unwrap();
        assert!(report.contains("# Report: Demo Page"));
        assert!(report.contains("<https://example.org/docs>"));
    }

    #[tokio::test]
    async fn workspace_rejects_write_escapes() {
        let ws = temp_ws();
        assert!(matches!(
            ws.resolve("../evil.txt"),
            Err(ToolError::PathEscape(_))
        ));
        assert!(matches!(
            ws.resolve("/etc/passwd"),
            Err(ToolError::PathEscape(_))
        ));
        assert!(extract_structure(&ws, "missing.md", "out.json")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reads_allow_absolute_paths_but_writes_stay_confined() {
        let ws = temp_ws();
        // An absolute source path (outside the workspace) is readable...
        let outside = std::env::temp_dir().join(format!("ak-out-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&outside, SAMPLE_MD).unwrap();
        let r = extract_structure(&ws, outside.to_str().unwrap(), "structure.json")
            .await
            .unwrap();
        assert_eq!(r["links"], 2);
        // ...but the output still cannot escape the workspace.
        assert!(
            extract_structure(&ws, outside.to_str().unwrap(), "/tmp/evil.json")
                .await
                .is_err()
        );
        // Relative reads refuse traversal.
        assert!(matches!(
            ws.resolve_read("../evil.md"),
            Err(ToolError::PathEscape(_))
        ));
    }
}
