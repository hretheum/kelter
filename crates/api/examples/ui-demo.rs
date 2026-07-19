//! Dev harness for exercising the web UI against deterministic runs — NOT a
//! deployment target. A scripted oracle paces multi-step runs (with one
//! deliberate kernel rejection) so the timeline, auto-scroll, parallel-run
//! bucketing, Stop (D-2) and journal replay (D-1) can be verified live in a
//! browser without a model server.
//!
//!     cargo run -p api --example ui-demo
//!
//! Prints the URL + token to open. The task text drives the step count:
//! "… 12 steps …" → 12 tool steps. A task containing "hang" never finishes
//! (for testing Stop).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, ValidatedAction,
};
use memory::InMemoryProvider;
use serde_json::json;

struct DemoTools;

#[async_trait]
impl ActionSource for DemoTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "work".into(),
                description: "One unit of demo work".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"note": {"type": "string"}},
                    "required": ["note"],
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
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        Ok(json!({ "done": a.arguments()["note"] }))
    }
}

/// Scripted, paced policy: N `work` steps (N parsed from the task, default 4),
/// one deliberate off-schema proposal at step 2 (the kernel rejects it → the
/// governance moment is visible on the axis), then `finish`.
struct ScriptedOracle;

#[async_trait]
impl PolicyOracle for ScriptedOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        // Pace the stream so scroll/streaming behavior is observable.
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        if context.task.contains("hang") {
            std::future::pending::<()>().await;
        }
        let total: usize = context
            .task
            .split_whitespace()
            .find_map(|w| w.parse().ok())
            .unwrap_or(4);
        let done = context.steps.len();
        if done >= total {
            return Ok(ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": format!("{done} units of work done")}),
                rationale: Some("all planned work is complete".into()),
            }
            .into());
        }
        // Step 2, first attempt: propose an action that is NOT in the schema —
        // the kernel rejects it and the retry (last_error set) goes valid.
        if done == 2 && context.last_error.is_none() {
            return Ok(ProposedAction {
                action: "rm_rf_slash".into(),
                arguments: json!({}),
                rationale: Some("(deliberately invalid, to show validation)".into()),
            }
            .into());
        }
        Ok(ProposedAction {
            action: "work".into(),
            arguments: json!({"note": format!("unit {}", done + 1)}),
            rationale: Some(format!("progressing: {}/{}", done, total)),
        }
        .into())
    }
    fn describe(&self) -> String {
        "demo:scripted".into()
    }
}

#[tokio::main]
async fn main() {
    let kernel = Arc::new(Kernel::new(
        Arc::new(ScriptedOracle),
        Arc::new(DemoTools),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig {
            max_steps: 40,
            ..KernelConfig::default()
        },
    ));
    let token = "ui-demo-token".to_string();
    let addr: SocketAddr = "127.0.0.1:7999".parse().unwrap();
    println!("open http://{addr}/#token={token}");
    api::serve(kernel, token, addr).await.unwrap();
}
