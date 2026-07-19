//! Secret redaction for logs (Phase 9, ADR 0017).
//!
//! Kelter logs to stdout (twelve-factor) and, by audit, never logs a secret
//! value: auth headers, request/response bodies, the OpenRouter key and A2A
//! payloads are not passed to any log macro. These helpers keep that invariant
//! enforceable — a masked fingerprint when a secret must be *referenced*, and a
//! scrubber for any string that might carry one before it is logged.

/// Mask a secret for a log line: keep a short, non-reversible prefix and drop
/// the rest. `"sk-or-abcdef123"` → `"sk-o…redacted"`. An empty/short secret
/// masks to just the marker, never revealing length beyond the 4-char peek.
pub fn redact_secret(secret: &str) -> String {
    let peek: String = secret.chars().take(4).collect();
    format!("{peek}…redacted")
}

/// Scrub likely secrets out of an arbitrary string before it is logged:
/// `Bearer <token>` and `sk-…`/`key-…`-style runs are replaced with a marker.
/// Defensive — used wherever a payload/header might reach a log macro.
pub fn scrub_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_secret_start(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        // Consume the secret token: skip a leading "Bearer " if present, then
        // the run of non-whitespace secret characters.
        let (matched, tail) = consume_secret(after);
        let _ = matched;
        out.push_str("<redacted>");
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn find_secret_start(text: &str) -> Option<usize> {
    // Case-insensitive "bearer " or a "sk-"/"key-" prefix.
    let lower = text.to_ascii_lowercase();
    let candidates = ["bearer ", "sk-", "key-"];
    candidates.iter().filter_map(|c| lower.find(c)).min()
}

fn consume_secret(after: &str) -> (&str, &str) {
    let lower = after.to_ascii_lowercase();
    let start = if lower.starts_with("bearer ") { 7 } else { 0 };
    let secret = &after[start..];
    let end = secret
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .unwrap_or(secret.len());
    (&after[..start + end], &secret[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secret_masks_the_value() {
        let r = redact_secret("sk-or-supersecret-value-123");
        assert_eq!(r, "sk-o…redacted");
        assert!(!r.contains("supersecret"));
    }

    #[test]
    fn scrub_removes_bearer_and_key_runs() {
        let s = scrub_secrets("Authorization: Bearer abc123def456 and key-DEADBEEF here");
        assert!(!s.contains("abc123def456"), "{s}");
        assert!(!s.contains("DEADBEEF"), "{s}");
        assert!(s.contains("<redacted>"));
        // Non-secret text is preserved.
        assert!(s.contains("Authorization:"));
        assert!(s.contains("here"));
    }

    #[test]
    fn scrub_leaves_clean_text_untouched() {
        let s = "run completed with 3 steps";
        assert_eq!(scrub_secrets(s), s);
    }
}
