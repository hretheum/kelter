//! Server-side A2A: Kelter **as** an A2A agent (inbound, ADR 0013).
//!
//! This module is transport-agnostic — pure helpers that the HTTP surface in
//! `crates/api` mounts behind bearer auth. The design crux (ADR 0013): an
//! inbound A2A task **is** a durable Kelter run. The A2A `task.id` is the
//! kernel `run_id`, so `tasks/get` reads the durable run store directly and
//! survives a server crash for free (the run auto-resumes on restart), with
//! no separate task store to keep consistent.
//!
//! Method names follow A2A v1.0 (proto-derived: `SendMessage`, `GetTask`,
//! `CancelTask`); the pre-1.0 JSON-RPC spellings (`message/send`, `tasks/get`,
//! `tasks/cancel`) are accepted too so both generations of client interop.
//! Task states are the canonical `TASK_STATE_*` enum. See docs/BUILD-EVIDENCE
//! Phase 7 for the live-source log.

use serde_json::{json, Value};

use kernel_core::{Run, RunStatus};

/// The three inbound methods, resolved from either naming generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Create a task (start a run): `SendMessage` / `message/send`.
    Send,
    /// Poll a task by id: `GetTask` / `tasks/get`.
    Get,
    /// Request cancellation: `CancelTask` / `tasks/cancel`.
    Cancel,
}

/// Resolve a JSON-RPC method string to an inbound method, accepting both the
/// v1.0 proto names and the pre-1.0 JSON-RPC spellings.
pub fn normalize_method(method: &str) -> Option<Method> {
    match method {
        "SendMessage" | "message/send" => Some(Method::Send),
        "GetTask" | "tasks/get" => Some(Method::Get),
        "CancelTask" | "tasks/cancel" => Some(Method::Cancel),
        _ => None,
    }
}

/// Map a durable run status onto the canonical A2A task state.
pub fn state_for(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "TASK_STATE_SUBMITTED",
        RunStatus::Running => "TASK_STATE_WORKING",
        RunStatus::Completed => "TASK_STATE_COMPLETED",
        RunStatus::Failed => "TASK_STATE_FAILED",
        RunStatus::Canceled => "TASK_STATE_CANCELED",
        // ADR 0021: paused awaiting a human's skill-activation decision — the
        // canonical A2A state for "the agent needs additional input before
        // it can continue" fits this exactly.
        RunStatus::AwaitingSkillDecision => "TASK_STATE_INPUT_REQUIRED",
        // ADR 0023: paused awaiting a human's delegation-authorization
        // decision — the same canonical state fits for the same reason.
        RunStatus::AwaitingDelegationDecision => "TASK_STATE_INPUT_REQUIRED",
        // ADR 0026: paused awaiting a human's continuation for an interactive
        // outbound delegation — from an inbound caller's view this run still
        // "needs additional input before it can continue".
        RunStatus::AwaitingDelegationInput => "TASK_STATE_INPUT_REQUIRED",
    }
}

/// The text answer a completed run carries, for the task's artifact.
fn output_text(run: &Run) -> String {
    match &run.output {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Build the A2A `Task` object for a run — the result of `tasks/get` and the
/// initial `message/send` response. Only a terminal run carries artifacts.
pub fn task_json(run: &Run) -> Value {
    let state = state_for(run.status);
    let mut task = json!({
        "id": run.id.to_string(),
        "status": { "state": state },
        "artifacts": [],
    });
    if run.status == RunStatus::Completed {
        task["artifacts"] = json!([{
            // A stable, run-derived artifact id (no RNG needed here).
            "artifactId": format!("{}-result", run.id),
            "parts": [{ "text": output_text(run) }],
        }]);
    } else if let Some(err) = &run.error {
        // Surface a failure/cancel reason in the status message (A2A allows a
        // human-readable message alongside the state).
        task["status"]["message"] = json!({
            "role": "ROLE_AGENT",
            "parts": [{ "text": err }],
        });
    }
    task
}

/// Extract the delegated text from a `message/send` params object:
/// `{ "message": { "parts": [ { "text": "…" }, … ] } }`.
pub fn message_text(params: &Value) -> String {
    params
        .get("message")
        .and_then(|m| m.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Extract A2A delegation lineage (ADR 0022) from a `message/send` params
/// object's `message.metadata` — `(0, [])` when absent. Absence is NOT
/// evidence of anything to refuse: a non-Kelter or older-version caller
/// simply never sends this, and from this server's perspective the inbound
/// task is, correctly, the start of a fresh chain.
pub fn message_lineage(params: &Value) -> (u32, Vec<String>) {
    let metadata = params.get("message").and_then(|m| m.get("metadata"));
    let depth = metadata
        .and_then(|m| m.get("kelter_a2a_depth"))
        .and_then(|v| v.as_u64())
        .map(|d| d as u32)
        .unwrap_or(0);
    let ancestry = metadata
        .and_then(|m| m.get("kelter_a2a_ancestry"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    (depth, ancestry)
}

/// Build the Agent Card served at `/.well-known/agent-card.json`.
///
/// `base_url` is the externally reachable origin (e.g. `http://127.0.0.1:7466`)
/// derived from the request Host — the JSON-RPC interface url is
/// `{base_url}/a2a/rpc`. The card declares an HTTP **bearer** security scheme
/// and marks it globally required: the task methods reject anonymous calls
/// (ADR 0013). Card discovery itself is unauthenticated so a client can learn
/// the scheme.
///
/// Both `interfaces` (A2A v1.0) and `supportedInterfaces` (the field the
/// Kelter 4b outbound client reads) are emitted with identical content, so
/// either client generation resolves the same endpoint.
pub fn agent_card(base_url: &str, policy: &str) -> Value {
    let rpc_url = format!("{}/a2a/rpc", base_url.trim_end_matches('/'));
    let interfaces = json!([{
        "url": rpc_url,
        "protocolBinding": "JSONRPC",
        "protocolVersion": "1.0",
    }]);
    json!({
        "id": "kelter",
        "name": "Kelter",
        "description": format!(
            "Kelter runtime as an A2A agent. A delegated task becomes a governed, \
             durable kernel run: every action the model proposes is validated by the \
             kernel and journaled, and the task inherits Kelter's durability — it \
             survives a server crash and resumes without re-executing. Policy: {policy}."
        ),
        "version": "1.0.0",
        "protocolVersion": "1.0",
        "provider": { "organization": "Kelter" },
        "capabilities": { "streaming": false, "pushNotifications": false },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "run-governed-task",
            "name": "Run a governed, durable task",
            "description": "Execute a self-contained task through the Kelter kernel \
                            (validated + journaled + durable).",
            "tags": ["kernel", "governed", "durable"],
            "inputModes": ["text/plain"],
            "outputModes": ["text/plain"],
        }],
        "interfaces": interfaces,
        "supportedInterfaces": interfaces,
        "securitySchemes": {
            "bearer": {
                "type": "http",
                "scheme": "bearer",
                "description": "Bearer token required for all task methods \
                                (message/send, tasks/get, tasks/cancel). Card \
                                discovery is unauthenticated."
            }
        },
        "security": [{ "bearer": [] }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn run_with(status: RunStatus, output: Option<Value>, error: Option<String>) -> Run {
        Run {
            id: Uuid::nil(),
            task: "t".into(),
            status,
            steps: Vec::new(),
            output,
            error,
            policy: "test".into(),
            created_at: 0,
            usage: Default::default(),
            agent_usage: HashMap::new(),
            trust_posture: Vec::new(),
            cost_usd: None,
            active_skills: Vec::new(),
            pending_skill_activation: None,
            pending_delegation: None,
            pending_delegation_input: None,
            a2a_depth: 0,
            a2a_ancestry: Vec::new(),
        }
    }

    #[test]
    fn method_names_both_generations() {
        assert_eq!(normalize_method("SendMessage"), Some(Method::Send));
        assert_eq!(normalize_method("message/send"), Some(Method::Send));
        assert_eq!(normalize_method("GetTask"), Some(Method::Get));
        assert_eq!(normalize_method("tasks/get"), Some(Method::Get));
        assert_eq!(normalize_method("CancelTask"), Some(Method::Cancel));
        assert_eq!(normalize_method("tasks/cancel"), Some(Method::Cancel));
        assert_eq!(normalize_method("tasks/resubscribe"), None);
    }

    #[test]
    fn state_mapping_is_canonical() {
        assert_eq!(state_for(RunStatus::Pending), "TASK_STATE_SUBMITTED");
        assert_eq!(state_for(RunStatus::Running), "TASK_STATE_WORKING");
        assert_eq!(state_for(RunStatus::Completed), "TASK_STATE_COMPLETED");
        assert_eq!(state_for(RunStatus::Failed), "TASK_STATE_FAILED");
        assert_eq!(state_for(RunStatus::Canceled), "TASK_STATE_CANCELED");
    }

    #[test]
    fn completed_task_carries_artifact_text() {
        let run = run_with(RunStatus::Completed, Some(json!("HELLO WORLD")), None);
        let task = task_json(&run);
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(task["artifacts"][0]["parts"][0]["text"], "HELLO WORLD");
    }

    #[test]
    fn failed_task_surfaces_error_message_not_artifact() {
        let run = run_with(RunStatus::Failed, None, Some("boom".into()));
        let task = task_json(&run);
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        assert_eq!(task["status"]["message"]["parts"][0]["text"], "boom");
        assert_eq!(task["artifacts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn message_text_joins_parts() {
        let params = json!({
            "message": { "parts": [{ "text": "hello" }, { "text": "world" }] }
        });
        assert_eq!(message_text(&params), "hello world");
        assert_eq!(message_text(&json!({})), "");
    }

    #[test]
    fn agent_card_declares_bearer_and_both_interface_fields() {
        let card = agent_card("http://127.0.0.1:7466", "local:test");
        assert_eq!(card["securitySchemes"]["bearer"]["scheme"], "bearer");
        assert_eq!(card["security"][0]["bearer"], json!([]));
        let rpc = "http://127.0.0.1:7466/a2a/rpc";
        assert_eq!(card["interfaces"][0]["url"], rpc);
        assert_eq!(card["supportedInterfaces"][0]["url"], rpc);
        assert_eq!(card["interfaces"][0]["protocolBinding"], "JSONRPC");
    }
}
