//! HTTP surface of the human gate (ADR 0038) and budgets (ADR 0039): the
//! `budget` field on `POST /runs`, `POST /runs/{id}/action-approval`, the
//! `GET /approvals` queue, and both showing up in the run and its history.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use kernel_core::{
    ActionGateConfig, ActionPolicy, ActionSchema, ActionSource, ActionSpec, BudgetConfig,
    BudgetLimits, CallBound, ExecutionContext, ExecutionError, Kernel, KernelConfig, Latitude,
    PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, Usage, ValidatedAction,
    IDEMPOTENCY_KEY_ARG,
};
use memory::InMemoryProvider;

struct WriteTool(Arc<AtomicUsize>);

#[async_trait]
impl ActionSource for WriteTool {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "write".into(),
                description: "write (side effect)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"entry": {"type": "string"}, IDEMPOTENCY_KEY_ARG: {"type": "string"}},
                    "required": ["entry"]
                }),
            }],
        })
    }
    async fn execute(
        &self,
        _a: &ValidatedAction,
        _c: &ExecutionContext,
    ) -> Result<Value, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"written": true}))
    }
}

/// Writes once, then finishes. Each call costs 100 tokens, bounded at 150.
struct WriteThenFinish;

#[async_trait]
impl PolicyOracle for WriteThenFinish {
    async fn propose(&self, c: &PolicyContext, _s: &ActionSchema) -> Result<Proposal, PolicyError> {
        let action = if c.steps.is_empty() {
            ProposedAction {
                action: "write".into(),
                arguments: json!({"entry": "x"}),
                rationale: None,
            }
        } else {
            ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "written"}),
                rationale: None,
            }
        };
        Ok(Proposal {
            action,
            usage: Some(Usage {
                prompt_tokens: 80,
                completion_tokens: 20,
                calls: 1,
            }),
        })
    }
    fn describe(&self) -> String {
        "mock:write".into()
    }
    fn call_bound(&self, _c: &PolicyContext, _s: &ActionSchema) -> CallBound {
        CallBound {
            prompt_tokens: 100,
            completion_tokens: Some(50),
        }
    }
}

async fn spawn(gate: ActionGateConfig) -> (SocketAddr, String, Arc<AtomicUsize>) {
    let effects = Arc::new(AtomicUsize::new(0));
    let kernel = Kernel::new(
        Arc::new(WriteThenFinish),
        Arc::new(WriteTool(effects.clone())),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig {
            reflection_enabled: false,
            ..KernelConfig::default()
        },
    )
    .with_action_gate(gate)
    .with_budget(BudgetConfig {
        per_run: BudgetLimits {
            max_tokens: Some(10_000),
            ..BudgetLimits::default()
        },
        prompt_overhead_tokens: 0,
        fallback_completion_tokens: 4096,
        ..BudgetConfig::default()
    });
    let token = "tok".to_string();
    let app = api::router(Arc::new(kernel), token.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, token, effects)
}

fn ask_write() -> ActionGateConfig {
    ActionGateConfig {
        default: ActionPolicy::Allow,
        actions: HashMap::from([("write".to_string(), ActionPolicy::Ask)]),
        ask_timeout_secs: 300,
    }
}

async fn create(c: &reqwest::Client, addr: SocketAddr, t: &str, body: Value) -> reqwest::Response {
    c.post(format!("http://{addr}/api/v1/runs"))
        .bearer_auth(t)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn get(c: &reqwest::Client, addr: SocketAddr, t: &str, path: &str) -> Value {
    c.get(format!("http://{addr}{path}"))
        .bearer_auth(t)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_status(
    c: &reqwest::Client,
    addr: SocketAddr,
    t: &str,
    id: &str,
    want: &str,
) -> Value {
    for _ in 0..200 {
        let run = get(c, addr, t, &format!("/api/v1/runs/{id}")).await;
        if run["status"] == want {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run {id} never reached {want}");
}

#[tokio::test]
async fn budget_override_is_validated_at_creation() {
    let (addr, t, _) = spawn(ActionGateConfig::default()).await;
    let c = reqwest::Client::new();
    for bad in [
        json!({"max_tokens": 20_000}),
        json!({"max_tokens": 0}),
        json!({"max_usd": -1.0}),
        json!({"max_dollars": 1}),
        json!("cheap"),
    ] {
        let res = create(&c, addr, &t, json!({"task": "t", "budget": bad})).await;
        assert_eq!(res.status(), 422, "{bad}");
        assert!(res.text().await.unwrap().contains("budget"), "{bad}");
    }
    let res = create(
        &c,
        addr,
        &t,
        json!({"task": "t", "budget": {"max_tokens": 5000}}),
    )
    .await;
    assert_eq!(res.status(), 202);
    let id = res.json::<Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = wait_status(&c, addr, &t, &id, "completed").await;
    assert_eq!(run["budget"]["max_tokens"], 5000);
    assert_eq!(run["budget"]["usd_enforced"], false);
    assert_eq!(run["principal"], "default");
    assert_eq!(run["budget_stop"], Value::Null);
}

#[tokio::test]
async fn approval_flow_over_http() {
    let (addr, t, effects) = spawn(ask_write()).await;
    let c = reqwest::Client::new();
    let res = create(&c, addr, &t, json!({"task": "write x"})).await;
    let id = res.json::<Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let paused = wait_status(&c, addr, &t, &id, "awaiting_action_approval").await;
    assert_eq!(paused["pending_action_approval"]["action"], "write");
    assert_eq!(paused["pending_action_approval"]["arguments"]["entry"], "x");
    assert_eq!(effects.load(Ordering::SeqCst), 0);

    // The queue lists it, with the route that resolves it.
    let queue = get(&c, addr, &t, "/api/v1/approvals").await;
    let item = &queue.as_array().unwrap()[0];
    assert_eq!(item["kind"], "action_approval");
    assert_eq!(item["run_id"], id.as_str());
    assert_eq!(item["decide"], format!("/api/v1/runs/{id}/action-approval"));

    let decide = |body: Value| {
        c.post(format!("http://{addr}/api/v1/runs/{id}/action-approval"))
            .bearer_auth(&t)
            .json(&body)
            .send()
    };
    assert_eq!(
        decide(json!({"action": "write", "decision": "allow_once"}))
            .await
            .unwrap()
            .status(),
        422
    );
    assert_eq!(
        decide(json!({"action": "rm", "decision": "allow"}))
            .await
            .unwrap()
            .status(),
        409
    );
    let anon = c
        .post(format!("http://{addr}/api/v1/runs/{id}/action-approval"))
        .json(&json!({"action": "write", "decision": "allow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    assert_eq!(
        decide(json!({"action": "write", "decision": "allow"}))
            .await
            .unwrap()
            .status(),
        202
    );
    wait_status(&c, addr, &t, &id, "completed").await;
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert!(get(&c, addr, &t, "/api/v1/approvals")
        .await
        .as_array()
        .unwrap()
        .is_empty());

    let events = get(&c, addr, &t, &format!("/api/v1/runs/{id}/events")).await;
    let kinds: Vec<&str> = events["journal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or(""))
        .collect();
    assert!(kinds.contains(&"action_approval_requested"), "{kinds:?}");
    let decided = events["journal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "action_approval_decided")
        .unwrap();
    assert_eq!(decided["principal"], "default");
    assert_eq!(decided["decision"], "allow");
}

#[tokio::test]
async fn budget_stop_is_visible_in_the_run_and_its_history() {
    let (addr, t, effects) = spawn(ActionGateConfig::default()).await;
    let c = reqwest::Client::new();
    // 200 tokens: the first call (≤150) fits, the second (100 + 150) would not.
    let res = create(
        &c,
        addr,
        &t,
        json!({"task": "t", "budget": {"max_tokens": 200}}),
    )
    .await;
    let id = res.json::<Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = wait_status(&c, addr, &t, &id, "failed").await;
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(run["budget_stop"]["limit"], "tokens");
    assert_eq!(run["budget_stop"]["checkpoint"], "model_call");
    assert_eq!(run["budget_stop"]["used"], 100.0);
    assert_eq!(run["budget_stop"]["requested"], 150.0);
    assert!(run["error"].as_str().unwrap().contains("token budget"));
    let events = get(&c, addr, &t, &format!("/api/v1/runs/{id}/events")).await;
    assert_eq!(events["budget_stop"]["limit"], "tokens");
    let refusal = events["journal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "rejected")
        .unwrap();
    assert_eq!(refusal["source"], "budget");
    assert_eq!(refusal["subject"], "none");
}
