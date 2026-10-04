//! The `zapisz_plik` WASM tool (K12 finding N1) against the real module in
//! `deploy/extensions/zapisz-plik/`, through the real host: the
//! `workspace_write` grant, host-side path validation, the typed argument
//! schema, and the `{"ok": false}` → failed-step convention — ending in a
//! scripted-oracle kernel run, where a refused write must not count as an
//! effective action (ADR 0033).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use kernel_core::{
    ActionSchema, ActionSource, ExecutionContext, Kernel, KernelConfig, Latitude, PolicyContext,
    PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
};
use memory::InMemoryProvider;
use serde_json::json;
use wasmhost::{Grants, WasmToolSource};

fn ext_dir() -> String {
    format!(
        "{}/deploy/extensions/zapisz-plik",
        env!("CARGO_MANIFEST_DIR").trim_end_matches("/crates/wasmhost")
    )
}

fn workspace() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("kelter-zp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn schema() -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{}/schema.json", ext_dir())).unwrap())
        .unwrap()
}

fn tool(grant: bool, ws: &std::path::Path) -> WasmToolSource {
    let src = WasmToolSource::load(
        "zapisz-plik",
        &format!("{}/zapisz_plik.wasm", ext_dir()),
        "zapisz_plik",
        "write a file",
        Grants {
            network: false,
            workspace_write: grant,
        },
    )
    .expect("module loads")
    .with_parameters(schema());
    if grant {
        src.with_workspace(ws).unwrap()
    } else {
        src
    }
}

async fn call(
    src: &WasmToolSource,
    args: serde_json::Value,
) -> Result<serde_json::Value, kernel_core::ExecutionError> {
    let schema = src.action_schema(Latitude::Standard).await.unwrap();
    let action = schema
        .validate(ProposedAction {
            action: "zapisz_plik".into(),
            arguments: args,
            rationale: None,
        })
        .expect("arguments match the declared schema");
    let ctx = ExecutionContext::new(
        uuid::Uuid::new_v4(),
        0,
        Arc::new(kernel_core::InMemoryRunStore::new()),
        Default::default(),
    );
    src.execute(&action, &ctx).await
}

#[tokio::test]
async fn typed_schema_is_offered_to_the_model() {
    let ws = workspace();
    let schema = tool(true, &ws)
        .action_schema(Latitude::Standard)
        .await
        .unwrap();
    let p = &schema.actions[0].parameters;
    assert_eq!(p["required"], json!(["sciezka", "tresc"]));
    assert_eq!(p["properties"]["nadpisz"]["type"], "boolean");
    // Undeclared keys never reach the module.
    assert!(schema
        .validate(ProposedAction {
            action: "zapisz_plik".into(),
            arguments: json!({"sciezka": "a", "tresc": "b", "extra": 1}),
            rationale: None,
        })
        .is_err());
}

#[tokio::test]
async fn writes_a_file_inside_the_workspace() {
    let ws = workspace();
    let src = tool(true, &ws);
    let out = call(
        &src,
        json!({"sciezka": "raporty/wynik.md", "tresc": "# Wynik\nzażółć gęślą jaźń\n"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(out["nadpisano"], false);
    // The key the run-files API reads (crates/api/src/run_files.rs).
    assert_eq!(out["saved_as"], "raporty/wynik.md");
    assert_eq!(
        std::fs::read_to_string(ws.join("raporty/wynik.md")).unwrap(),
        "# Wynik\nzażółć gęślą jaźń\n"
    );
}

#[tokio::test]
async fn overwrite_needs_the_explicit_flag() {
    let ws = workspace();
    let src = tool(true, &ws);
    call(&src, json!({"sciezka": "f.txt", "tresc": "one"}))
        .await
        .unwrap();

    let err = call(&src, json!({"sciezka": "f.txt", "tresc": "two"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("już istnieje"), "{err}");
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "one");

    let out = call(
        &src,
        json!({"sciezka": "f.txt", "tresc": "two", "nadpisz": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["nadpisano"], true);
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "two");
}

#[tokio::test]
async fn traversal_absolute_and_oversize_are_failed_steps() {
    let ws = workspace();
    let src = tool(true, &ws);
    for p in ["../ucieczka.txt", "/tmp/ucieczka.txt", "a/../../b.txt"] {
        let r = call(&src, json!({"sciezka": p, "tresc": "x"})).await;
        assert!(r.is_err(), "{p} must be a failed step, got {r:?}");
    }
    let big = "x".repeat(256 * 1024 + 1);
    let err = call(&src, json!({"sciezka": "big.txt", "tresc": big}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("za duża"), "{err}");
    assert!(
        std::fs::read_dir(&ws).unwrap().next().is_none(),
        "nothing written"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn host_refuses_a_symlink_escape_the_guest_cannot_see() {
    // "link/x.txt" passes the guest's own checks; only the host can see that
    // `link` points outside. This is why the host, not the guest, enforces.
    let ws = workspace();
    let outside = workspace();
    std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
    let err = call(
        &tool(true, &ws),
        json!({"sciezka": "link/x.txt", "tresc": "x"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("poza workspace"), "{err}");
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[tokio::test]
async fn without_the_grant_the_module_cannot_even_instantiate() {
    let ws = workspace();
    let err = call(&tool(false, &ws), json!({"sciezka": "a.txt", "tresc": "x"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("could not be instantiated"), "{err}");
    assert!(!ws.join("a.txt").exists());
}

#[test]
fn grant_without_a_workspace_is_refused_loudly() {
    // Wiring bug guard: the grant with nothing to write into must not
    // silently define an import that always fails.
    let src = WasmToolSource::load(
        "zapisz-plik",
        &format!("{}/zapisz_plik.wasm", ext_dir()),
        "zapisz_plik",
        "x",
        Grants {
            network: false,
            workspace_write: true,
        },
    )
    .unwrap()
    .with_parameters(schema());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(call(&src, json!({"sciezka": "a", "tresc": "b"})))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no workspace attached"), "{err}");
}

// ---- end to end through the kernel (ADR 0033 × N1) -------------------------

struct Script(Vec<ProposedAction>, AtomicUsize);

#[async_trait]
impl PolicyOracle for Script {
    async fn propose(
        &self,
        _c: &PolicyContext,
        _s: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .get(i)
            .cloned()
            .map(Proposal::from)
            .ok_or_else(|| PolicyError::Backend("script exhausted".into()))
    }
    fn describe(&self) -> String {
        "mock:scripted".into()
    }
}

fn step(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

async fn run(ws: &std::path::Path, script: Vec<ProposedAction>) -> kernel_core::Run {
    let kernel = Kernel::new(
        Arc::new(Script(script, AtomicUsize::new(0))),
        Arc::new(tool(true, ws)),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    );
    kernel.run_task("zapisz raport".into()).await.unwrap()
}

#[tokio::test]
async fn kernel_run_that_writes_the_file_completes() {
    let ws = workspace();
    let r = run(
        &ws,
        vec![
            step(
                "zapisz_plik",
                json!({"sciezka": "raport.md", "tresc": "ok"}),
            ),
            step("finish", json!({"summary": "zapisano raport.md"})),
        ],
    )
    .await;
    assert_eq!(r.status, RunStatus::Completed, "{:?}", r.error);
    assert!(r.steps[0].ok);
    assert_eq!(std::fs::read_to_string(ws.join("raport.md")).unwrap(), "ok");
}

#[tokio::test]
async fn kernel_run_whose_only_write_was_refused_is_incomplete() {
    // The guest reports `{"ok": false}`; the host turns it into a FAILED
    // step, so `finish` has nothing effective behind it.
    let ws = workspace();
    let r = run(
        &ws,
        vec![
            step(
                "zapisz_plik",
                json!({"sciezka": "../raport.md", "tresc": "x"}),
            ),
            step("finish", json!({"summary": "zapisano raport"})),
        ],
    )
    .await;
    assert!(!r.steps[0].ok);
    assert_eq!(r.status, RunStatus::Incomplete);
}
