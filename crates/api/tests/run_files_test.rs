//! Run-produced-files contract (ADR 0029): bearer-gated, journal-scoped,
//! path-traversal-safe. Proves the brief's two security gates empirically:
//! anonymous → 401, and a `..` path → refused (400), plus the allowlist
//! property that a workspace file some OTHER run produced is a 404.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, ValidatedAction,
};
use memory::InMemoryProvider;

/// A tool that REALLY writes `report.md` into the workspace and reports it
/// under the `output` key — the same receipt shape as the bundled
/// `write_report` tool.
struct ReportTools {
    workspace: PathBuf,
}

#[async_trait]
impl ActionSource for ReportTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "make_report".into(),
                description: "Write a report file".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"output": {"type": "string"}},
                    "required": ["output"],
                    "additionalProperties": false
                }),
            }],
        })
    }
    async fn execute(
        &self,
        a: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        let name = a.arguments()["output"].as_str().unwrap();
        std::fs::write(
            self.workspace.join(name),
            "# Report\n\nProduced by the run.\n",
        )
        .unwrap();
        Ok(json!({"output": name, "bytes": 33}))
    }
}

struct ReportOracle;

#[async_trait]
impl PolicyOracle for ReportOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        if context.steps.is_empty() {
            Ok(ProposedAction {
                action: "make_report".into(),
                arguments: json!({"output": "report.md"}),
                rationale: None,
            }
            .into())
        } else {
            Ok(ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "Wrote report.md with the findings."}),
                rationale: None,
            }
            .into())
        }
    }
    fn describe(&self) -> String {
        "mock:report".into()
    }
}

fn temp_workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-files-api-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

async fn spawn_server(workspace: &PathBuf, with_files: bool) -> (SocketAddr, String) {
    let kernel = Arc::new(Kernel::new(
        Arc::new(ReportOracle),
        Arc::new(ReportTools {
            workspace: workspace.clone(),
        }),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    ));
    let token = "test-token-123".to_string();
    let files = with_files.then(|| Arc::new(api::RunFilesService::new(workspace).unwrap()));
    let app = api::router_full(
        kernel,
        api::Principals::single(token.clone()),
        None,
        None,
        None,
        files,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, token)
}

/// Create a run and poll it to completion; returns its id.
async fn complete_run(addr: SocketAddr, token: &str) -> String {
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(token)
        .json(&json!({"task": "produce the report"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    let run_id = res.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    for _ in 0..100 {
        let run: serde_json::Value = client
            .get(format!("http://{addr}/api/v1/runs/{run_id}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if run["status"] == "completed" {
            return run_id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("run never completed");
}

#[tokio::test]
async fn anonymous_requests_are_401() {
    let ws = temp_workspace();
    let (addr, token) = spawn_server(&ws, true).await;
    let run_id = complete_run(addr, &token).await;
    let client = reqwest::Client::new();
    for path in [
        format!("/api/v1/runs/{run_id}/files"),
        format!("/api/v1/runs/{run_id}/files/report.md"),
    ] {
        let res = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "anon GET {path} must be 401");
        let res = client
            .get(format!("http://{addr}{path}"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "bad-token GET {path} must be 401");
    }
}

#[tokio::test]
async fn lists_and_serves_only_this_runs_files() {
    let ws = temp_workspace();
    let (addr, token) = spawn_server(&ws, true).await;
    // A file in the SAME workspace this run never produced (another run's).
    std::fs::write(ws.join("other-runs.md"), "not yours").unwrap();
    let run_id = complete_run(addr, &token).await;
    let client = reqwest::Client::new();

    let listing: serde_json::Value = client
        .get(format!("http://{addr}/api/v1/runs/{run_id}/files"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let files = listing["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "exactly the one produced file: {listing}");
    assert_eq!(files[0]["name"], "report.md");

    let res = client
        .get(format!(
            "http://{addr}/api/v1/runs/{run_id}/files/report.md"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"], "# Report\n\nProduced by the run.\n");

    // The co-resident file is a 404 — indistinguishable from nonexistence.
    let res = client
        .get(format!(
            "http://{addr}/api/v1/runs/{run_id}/files/other-runs.md"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404, "another run's file must be invisible");
}

#[tokio::test]
async fn traversal_and_absolute_paths_are_refused() {
    let ws = temp_workspace();
    let (addr, token) = spawn_server(&ws, true).await;
    let run_id = complete_run(addr, &token).await;
    let client = reqwest::Client::new();
    // Percent-encoded so the traversal survives URL normalization and
    // actually reaches the handler as `../…` / an absolute path.
    for (encoded, expect) in [
        ("..%2Fsecret.md", 400),
        ("a%2F..%2F..%2Fsecret.md", 400),
        ("%2Fetc%2Fpasswd", 400),
        ("..%2F..%2F..%2Fetc%2Fpasswd", 400),
    ] {
        let res = client
            .get(format!(
                "http://{addr}/api/v1/runs/{run_id}/files/{encoded}"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), expect, "GET files/{encoded}");
    }
}

#[tokio::test]
async fn unknown_run_is_404_and_unwired_service_is_404() {
    let ws = temp_workspace();
    let (addr, token) = spawn_server(&ws, true).await;
    let client = reqwest::Client::new();
    let ghost = uuid::Uuid::new_v4();
    let res = client
        .get(format!("http://{addr}/api/v1/runs/{ghost}/files"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // A router without the files service: the whole surface is 404 (the
    // skills/agents capability-flag convention).
    let ws2 = temp_workspace();
    let (addr2, token2) = spawn_server(&ws2, false).await;
    let run_id = complete_run(addr2, &token2).await;
    let res = client
        .get(format!("http://{addr2}/api/v1/runs/{run_id}/files"))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}
