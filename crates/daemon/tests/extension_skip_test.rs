//! ADR 0035 end to end: a refused extension is SKIPPED, the daemon still
//! starts. These tests spawn the real `kelter` binary against a temp config
//! and read `GET /api/v1/tools` — the operator-visible surface.
//!
//! Memory is served by the checked-in WASM memstore so no sibling binary
//! (`memory-server`) is needed for the skip test. The consent test needs the
//! `tools-server` binary next to `kelter` as a real subprocess extension;
//! `cargo test --workspace` (what CI runs) builds it, because the `mcp`
//! crate's own integration tests do.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "extension-skip-test-token";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tmp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-skip-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn copy_example(name: &str, dest_root: &Path) {
    let src = repo_root().join("examples/extensions").join(name);
    let dest = dest_root.join(name);
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let p = entry.unwrap().path();
        std::fs::copy(&p, dest.join(p.file_name().unwrap())).unwrap();
    }
}

fn write_manifest(root: &Path, dir: &str, body: &str) {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("kelter-extension.toml"), body).unwrap();
}

/// A tools subprocess opted into `subprocess-unsandboxed`.
fn untrusted_subprocess(name: &str, command: &str, workspace: &Path) -> String {
    format!(
        "[extension]\nname = \"{name}\"\nprovides = [\"tools\"]\ntrust = \"subprocess-unsandboxed\"\n\n\
         [server]\ncommand = \"{command}\"\nargs = [\"--workspace\", \"{}\"]\n",
        workspace.display()
    )
}

fn write_config(work: &Path, port: u16, ext_dir: &Path, trust: &str) -> PathBuf {
    let cfg = format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntoken = \"{TOKEN}\"\n\
         [policy]\nengine = \"local:qwen2.5-0.5b-instruct\"\n\
         [storage]\nbackend = \"memory\"\n\
         {trust}\n\
         [tools]\nworkspace = \"{}\"\nbundled_tools = false\nextension_dirs = [\"{}\"]\n",
        work.join("ws").display(),
        ext_dir.display()
    );
    let path = work.join("kelter.toml");
    std::fs::write(&path, cfg).unwrap();
    path
}

/// The spawned daemon; killed on drop so a failing assert never leaks it.
struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn log(&self) -> String {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let _ = f.read_to_string(&mut s);
        }
        s
    }
}

async fn start(work: &Path, config: &Path, port: u16) -> Daemon {
    let log = work.join("daemon.log");
    let out = std::fs::File::create(&log).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_kelter"))
        .arg("--config")
        .arg(config)
        .current_dir(work)
        .env("RUST_LOG", "info")
        .env_remove("OPENROUTER_API_KEY")
        .stdout(Stdio::from(out.try_clone().unwrap()))
        .stderr(Stdio::from(out))
        .spawn()
        .expect("spawn kelter");
    let mut daemon = Daemon { child, log };
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            panic!(
                "daemon exited ({status}) instead of starting — log:\n{}",
                daemon.log()
            );
        }
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return daemon;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never became healthy — log:\n{}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn get_tools(port: u16) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/api/v1/tools"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn tool_names(tools: &serde_json::Value) -> Vec<String> {
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// The K12 acceptance case: a WASM tool, an un-allowed unsandboxed
/// subprocess and an unparseable manifest. The daemon starts, the WASM tool
/// is offered, both refusals are reported via the API and logged at ERROR,
/// and the refused subprocess never ran.
#[tokio::test]
async fn daemon_starts_with_untrusted_subprocess_and_broken_manifest_skipped() {
    let work = tmp();
    let ext = work.join("ext");
    copy_example("wordcount-wasm", &ext);
    copy_example("memstore-wasm", &ext);
    // If this subprocess were ever started it would create its marker file.
    let marker = work.join("wordstats-ran");
    let script = work.join("wordstats.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    write_manifest(
        &ext,
        "wordstats",
        &format!(
            "[extension]\nname = \"wordstats\"\nprovides = [\"tools\"]\n\
             trust = \"subprocess-unsandboxed\"\n\n\
             [server]\ncommand = \"/bin/sh\"\nargs = [\"{}\"]\n",
            script.display()
        ),
    );
    write_manifest(&ext, "broken", "[extension\nname = \"broken\"\n");

    let port = free_port();
    let config = write_config(&work, port, &ext, "");
    let daemon = start(&work, &config, port).await;

    let tools = get_tools(port).await;
    assert!(
        tool_names(&tools).contains(&"word_count".to_string()),
        "WASM tool must still be offered: {tools}"
    );

    let skipped = tools["skipped_extensions"]
        .as_array()
        .expect("skipped_extensions");
    assert_eq!(skipped.len(), 2, "{tools}");
    let untrusted = skipped
        .iter()
        .find(|s| s["reason"] == "operator_consent_required")
        .expect("untrusted subprocess reported");
    assert_eq!(untrusted["name"], "wordstats");
    assert!(untrusted["remedy"]
        .as_str()
        .unwrap()
        .contains("[trust] allow_unsandboxed_subprocess"));
    let broken = skipped
        .iter()
        .find(|s| s["reason"] == "invalid_manifest")
        .expect("broken manifest reported");
    assert!(broken["name"].is_null());
    assert!(broken["manifest"]
        .as_str()
        .unwrap()
        .ends_with("broken/kelter-extension.toml"));

    let log = daemon.log();
    let error_lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("ERROR") && l.contains("SKIPPED"))
        .collect();
    assert_eq!(
        error_lines.len(),
        2,
        "two ERROR skip lines expected:\n{log}"
    );
    assert!(
        error_lines
            .iter()
            .any(|l| l.contains("wordstats") && l.contains("[trust] allow_unsandboxed_subprocess")),
        "{log}"
    );
    assert!(!log.contains("operator-trusted subprocess"), "{log}");
    assert!(
        !marker.exists(),
        "the refused subprocess must never be started"
    );
}

/// The operator gate still opens: with `wordstats` listed in `[trust]
/// allow_unsandboxed_subprocess`, the same subprocess registers and its
/// tools join the action space; nothing is skipped.
#[tokio::test]
async fn operator_consent_still_enables_the_subprocess() {
    let tools_server = PathBuf::from(env!("CARGO_BIN_EXE_kelter"))
        .parent()
        .unwrap()
        .join("tools-server");
    assert!(
        tools_server.is_file(),
        "{} not built — run `cargo test --workspace` (or `cargo build -p mcp --bins` first)",
        tools_server.display()
    );

    let work = tmp();
    let ext = work.join("ext");
    copy_example("memstore-wasm", &ext);
    write_manifest(
        &ext,
        "wordstats",
        &untrusted_subprocess(
            "wordstats",
            &tools_server.to_string_lossy(),
            &work.join("ws"),
        ),
    );

    let port = free_port();
    let config = write_config(
        &work,
        port,
        &ext,
        "[trust]\nallow_unsandboxed_subprocess = [\"wordstats\"]",
    );
    let daemon = start(&work, &config, port).await;

    let tools = get_tools(port).await;
    assert_eq!(
        tools["skipped_extensions"],
        serde_json::json!([]),
        "{tools}"
    );
    assert!(
        !tool_names(&tools).is_empty(),
        "the allowed subprocess's tools must be offered: {tools}"
    );
    let log = daemon.log();
    assert!(log.contains("operator-trusted subprocess"), "{log}");
    assert!(!log.contains("SKIPPED"), "{log}");
}
