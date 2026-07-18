//! Real-protocol test: spawn the bundled tool server as a child process,
//! talk MCP over stdio, and drive the whole read→extract→report pipeline
//! through the kernel's ActionSource seam (schema → validate → execute).

use kernel_core::{ActionSource, Latitude, ProposedAction};
use mcp::{McpToolSource, ServerSpec};
use serde_json::json;

const SAMPLE: &str = "# Sample Doc\n\n## Part A\n\nA [link](https://example.org/x).\n";

async fn source_with_ws(dir: &std::path::Path) -> McpToolSource {
    McpToolSource::connect(vec![ServerSpec {
        name: "reference-tools".into(),
        command: env!("CARGO_BIN_EXE_tools-server").into(),
        args: vec!["--workspace".into(), dir.to_string_lossy().into_owned()],
        cwd: None,
        env: Vec::new(),
    }])
    .await
    .expect("tool server should start and register tools")
}

#[tokio::test]
async fn full_pipeline_over_real_mcp_stdio() {
    let dir = std::env::temp_dir().join(format!("ak-mcp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("input.md");
    std::fs::write(&input, SAMPLE).unwrap();

    let source = source_with_ws(&dir).await;

    assert_eq!(
        source.tool_names(),
        vec![
            "append_ledger",
            "extract_structure",
            "read_source",
            "write_report"
        ]
    );

    let schema = source.action_schema(Latitude::Constrained).await.unwrap();
    assert_eq!(schema.actions.len(), 4);
    // Every reference tool declares the idempotency contract.
    assert!(schema.actions.iter().all(|a| a.supports_idempotency_key()));

    // Step 1: read_source — proposal must pass kernel-side validation first.
    let v = schema
        .validate(ProposedAction {
            action: "read_source".into(),
            arguments: json!({"input": input.to_string_lossy(), "save_as": "raw.md"}),
            rationale: None,
        })
        .unwrap();
    let r = source.execute(&v).await.unwrap();
    assert_eq!(r["saved_as"], "raw.md");

    // Step 2: extract_structure.
    let v = schema
        .validate(ProposedAction {
            action: "extract_structure".into(),
            arguments: json!({"source": "raw.md", "save_as": "structure.json"}),
            rationale: None,
        })
        .unwrap();
    let r = source.execute(&v).await.unwrap();
    assert_eq!(r["links"], 1);
    assert_eq!(r["title"], "Sample Doc");

    // Step 3: write_report.
    let v = schema
        .validate(ProposedAction {
            action: "write_report".into(),
            arguments: json!({"source": "structure.json", "output": "report.md", "format": "markdown"}),
            rationale: None,
        })
        .unwrap();
    let r = source.execute(&v).await.unwrap();
    assert_eq!(r["output"], "report.md");

    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(report.contains("# Report: Sample Doc"));
}

#[tokio::test]
async fn tool_error_surfaces_as_execution_error() {
    let dir = std::env::temp_dir().join(format!("ak-mcp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = source_with_ws(&dir).await;
    let schema = source.action_schema(Latitude::Standard).await.unwrap();

    // extract from a file that was never saved → structured error → Err.
    let v = schema
        .validate(ProposedAction {
            action: "extract_structure".into(),
            arguments: json!({"source": "missing.md", "save_as": "s.json"}),
            rationale: None,
        })
        .unwrap();
    let err = source.execute(&v).await.unwrap_err();
    assert!(err.to_string().contains("missing.md"));
}
