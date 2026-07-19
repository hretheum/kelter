//! Redaction, proven — not claimed (Phase 9, ADR 0017). A tracing subscriber
//! captures ALL log output while code handles a real secret; the assertion is
//! that the secret value never appears in the captured logs, only its masked
//! fingerprint.

use std::io::Write;
use std::sync::{Arc, Mutex};

use kernel_core::redact::redact_secret;

/// A `MakeWriter` that appends every log line to a shared buffer.
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn a_secret_never_appears_in_captured_logs() {
    let secret = "sk-or-THIS-IS-A-SECRET-abcdef123456";
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(buf.clone()))
        .with_ansi(false)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        // The exact pattern the daemon uses: reference a secret in a log line
        // ONLY through its masked fingerprint.
        tracing::info!(
            "auth: bearer token active (fingerprint {})",
            redact_secret(secret)
        );
        // And a defensive scrub of a payload that carries the secret.
        tracing::info!(
            "inbound: {}",
            kernel_core::redact::scrub_secrets(&format!("Authorization: Bearer {secret}"))
        );
    });

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "expected some captured log output");
    assert!(!logs.contains(secret), "SECRET LEAKED into logs:\n{logs}");
    assert!(
        !logs.contains("SECRET-abcdef123456"),
        "secret tail leaked:\n{logs}"
    );
    // The masked fingerprint IS present (so the token is still identifiable).
    assert!(logs.contains("…redacted"), "no redaction marker:\n{logs}");
}
