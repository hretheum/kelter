//! `zapisz_plik` — a Kelter WASM tool that writes a text file into the run
//! workspace (K12 acceptance finding N1: the default tool set had no plain
//! "write this content to a file", so a model reached for `append_ledger`).
//!
//! Arguments (typed, flat — declared in `schema.json` next to the manifest):
//!
//!   {"sciezka": "raporty/wynik.md", "tresc": "...", "nadpisz": false}
//!
//! The module imports exactly ONE host function, `kelter:fs.kelter_ws_write`,
//! which the host defines only when the manifest grants
//! `[permissions] workspace_write = true`. Without the grant the module
//! cannot even be instantiated. It has no network, no clock, no environment.
//!
//! The checks below are the guest's own, for clear messages and a tighter
//! size limit. They are NOT the security boundary: the host re-validates
//! every path (relative, no `..`, no symlink escape, no-clobber unless
//! overwrite) and enforces its own hard size cap, whatever the guest sends.

use serde_json::{json, Value};

/// This tool's own content limit (the host's hard cap is larger, 1 MiB).
pub const MAX_TRESC_BYTES: usize = 256 * 1024;
/// Path length limit (matches the host's).
pub const MAX_SCIEZKA_BYTES: usize = 512;

const FLAG_OVERWRITE: i32 = 1;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "kelter:fs")]
extern "C" {
    /// Host capability; undefined unless `workspace_write` is granted.
    /// Returns 0 created, 1 overwritten, negative = refusal code.
    fn kelter_ws_write(
        path_ptr: i32,
        path_len: i32,
        data_ptr: i32,
        data_len: i32,
        flags: i32,
    ) -> i32;
}

/// Host-target stand-in so the pure logic can be unit-tested with plain
/// `cargo test`; never part of the `.wasm`.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn kelter_ws_write(_: i32, _: i32, _: i32, _: i32, _: i32) -> i32 {
    0
}

/// A validated request.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub sciezka: String,
    pub tresc: String,
    pub nadpisz: bool,
}

/// Parse and check the arguments object. Errors are user-facing (Polish,
/// like the tool) because they go back to the model as the step's error.
pub fn parse(input: &str) -> Result<Request, String> {
    let v: Value =
        serde_json::from_str(input).map_err(|e| format!("argumenty nie są poprawnym JSON: {e}"))?;
    let sciezka = v
        .get("sciezka")
        .and_then(Value::as_str)
        .ok_or("brak argumentu 'sciezka' (tekst)")?;
    let tresc = v
        .get("tresc")
        .and_then(Value::as_str)
        .ok_or("brak argumentu 'tresc' (tekst)")?;
    let nadpisz = match v.get("nadpisz") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("argument 'nadpisz' musi być true albo false".into()),
    };
    check_path(sciezka)?;
    if tresc.len() > MAX_TRESC_BYTES {
        return Err(format!(
            "treść za duża: {} bajtów (limit {MAX_TRESC_BYTES})",
            tresc.len()
        ));
    }
    Ok(Request {
        sciezka: sciezka.to_string(),
        tresc: tresc.to_string(),
        nadpisz,
    })
}

/// Relative, '/'-separated, only normal components.
pub fn check_path(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("ścieżka jest pusta".into());
    }
    if p.len() > MAX_SCIEZKA_BYTES {
        return Err(format!(
            "ścieżka za długa (limit {MAX_SCIEZKA_BYTES} bajtów)"
        ));
    }
    if p.contains('\0') || p.contains('\\') {
        return Err("ścieżka zawiera niedozwolony znak (NUL lub '\\')".into());
    }
    if p.starts_with('/') || p.as_bytes().get(1) == Some(&b':') {
        return Err("ścieżka musi być względna — wewnątrz workspace".into());
    }
    for part in p.split('/') {
        match part {
            "" => return Err("ścieżka zawiera pusty segment ('//' lub '/' na końcu)".into()),
            "." | ".." => return Err("ścieżka nie może zawierać '.' ani '..'".into()),
            _ => {}
        }
    }
    Ok(())
}

/// Human-readable meaning of a host return code.
pub fn host_error(code: i32) -> &'static str {
    match code {
        -1 => "niedozwolona ścieżka (odrzucona przez hosta)",
        -2 => "plik już istnieje — ustaw nadpisz=true, aby go zastąpić",
        -3 => "treść przekracza limit hosta",
        -4 => "ścieżka wychodzi poza workspace (dowiązanie symboliczne)",
        -6 => "pod tą ścieżką jest katalog, nie plik",
        -7 => "błędne argumenty wywołania hosta",
        _ => "błąd wejścia/wyjścia przy zapisie",
    }
}

/// The whole tool: parse → host write → result JSON. A failure is reported
/// as `{"ok": false, "error": ...}`, which the host turns into a FAILED step.
pub fn handle(input: &str) -> Value {
    let req = match parse(input) {
        Ok(r) => r,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let flags = if req.nadpisz { FLAG_OVERWRITE } else { 0 };
    let code = unsafe {
        kelter_ws_write(
            req.sciezka.as_ptr() as i32,
            req.sciezka.len() as i32,
            req.tresc.as_ptr() as i32,
            req.tresc.len() as i32,
            flags,
        )
    };
    if code < 0 {
        return json!({ "ok": false, "error": host_error(code), "kod": code });
    }
    // `saved_as` is the key the daemon's run-files API reads from a
    // successful receipt (crates/api/src/run_files.rs) — without it the file
    // is written but `GET /api/v1/runs/{id}/files` does not list it.
    json!({
        "ok": true,
        "saved_as": req.sciezka,
        "sciezka": req.sciezka,
        "bajty": req.tresc.len(),
        "nadpisano": code == 1,
    })
}

// ---- byte ABI (ADR 0011) ----------------------------------------------------

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = std::alloc::Layout::from_size_align(len as usize, 1).unwrap();
    // Leaked on purpose: one-shot instance, torn down by the host per call.
    unsafe { std::alloc::alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn run(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = match std::str::from_utf8(input) {
        Ok(text) => handle(text),
        Err(_) => json!({ "ok": false, "error": "argumenty nie są poprawnym UTF-8" }),
    };
    let bytes = out.to_string().into_bytes();
    let out_len = bytes.len();
    let out_ptr = bytes.as_ptr() as i32;
    std::mem::forget(bytes);
    ((out_ptr as i64) << 32) | (out_len as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_request() {
        let r = parse(r#"{"sciezka":"a/b.md","tresc":"x","nadpisz":true}"#).unwrap();
        assert_eq!(
            r,
            Request {
                sciezka: "a/b.md".into(),
                tresc: "x".into(),
                nadpisz: true
            }
        );
        // nadpisz is optional and defaults to false.
        assert!(!parse(r#"{"sciezka":"a","tresc":""}"#).unwrap().nadpisz);
    }

    #[test]
    fn refuses_bad_paths() {
        for p in [
            "", "/etc/x", "../x", "a/../b", "./a", "a//b", "a/", "C:/x", "a\\b",
        ] {
            assert!(check_path(p).is_err(), "{p:?} must be refused");
        }
        assert!(check_path(&"a".repeat(MAX_SCIEZKA_BYTES + 1)).is_err());
        assert!(check_path("raporty/2026/wynik.md").is_ok());
        assert!(check_path(".ukryty").is_ok());
    }

    #[test]
    fn refuses_missing_or_mistyped_arguments() {
        assert!(parse("nie json").is_err());
        assert!(parse(r#"{"tresc":"x"}"#).is_err());
        assert!(parse(r#"{"sciezka":"a"}"#).is_err());
        assert!(parse(r#"{"sciezka":"a","tresc":"x","nadpisz":"tak"}"#).is_err());
    }

    #[test]
    fn enforces_size_limit() {
        let big = "x".repeat(MAX_TRESC_BYTES + 1);
        let input = json!({"sciezka": "a", "tresc": big}).to_string();
        assert!(parse(&input).unwrap_err().contains("za duża"));
        let max = "x".repeat(MAX_TRESC_BYTES);
        assert!(parse(&json!({"sciezka": "a", "tresc": max}).to_string()).is_ok());
    }

    #[test]
    fn success_names_the_file_under_saved_as() {
        // The host stand-in reports "created"; the run-files API needs
        // `saved_as` to serve the file.
        let out = handle(r#"{"sciezka":"a/b.md","tresc":"x"}"#);
        assert_eq!(out["ok"], true);
        assert_eq!(out["saved_as"], "a/b.md");
    }

    #[test]
    fn failure_is_reported_as_ok_false() {
        let out = handle(r#"{"sciezka":"../x","tresc":"x"}"#);
        assert_eq!(out["ok"], false);
        assert!(out["error"].as_str().unwrap().contains(".."));
    }
}
