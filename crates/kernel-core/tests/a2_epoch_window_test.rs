//! A2 epoch-window regression (born as a live-series diagnostic): FAILED
//! keyed attempts do not anchor an epoch; a successful keyed command does —
//! so a re-proposal identical to an earlier FAILED attempt, made after the
//! successful command, is legal and executes (the failure lies outside the
//! current epoch window).
use async_trait::async_trait;
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
    RunStore, ValidatedAction,
};
use memory::InMemoryProvider;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

struct O {
    script: Mutex<Vec<ProposedAction>>,
    calls: AtomicUsize,
}
#[async_trait]
impl PolicyOracle for O {
    async fn propose(
        &self,
        _c: &PolicyContext,
        _s: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut s = self.script.lock().await;
        let a = if s.len() > 1 {
            s.remove(0)
        } else {
            s[0].clone()
        };
        Ok(a.into())
    }
    fn describe(&self) -> String {
        "mock".into()
    }
}
struct T {
    popraw_fails: AtomicUsize,
    ex: Mutex<Vec<String>>,
}
#[async_trait]
impl ActionSource for T {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        let spec = |name: &str, props: serde_json::Value| ActionSpec {
            name: name.into(),
            description: name.into(),
            parameters: json!({"type":"object","properties":props,"additionalProperties":true}),
        };
        Ok(ActionSchema {
            actions: vec![
                spec("czytaj", json!({"plik":{"type":"string"}})),
                spec(
                    "popraw",
                    json!({"instrukcja":{"type":"string"},"idempotency_key":{"type":["string","null"]}}),
                ),
                spec(
                    "uruchom",
                    json!({"idempotency_key":{"type":["string","null"]}}),
                ),
            ],
        })
    }
    async fn execute(
        &self,
        a: &ValidatedAction,
        _c: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        self.ex.lock().await.push(a.action().into());
        match a.action() {
            "popraw" => {
                let n = self.popraw_fails.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    Err(ExecutionError::Tool("wykonawca nie zwrócił bloku".into()))
                } else {
                    Ok(json!({"zastosowane_bloki":1}))
                }
            }
            _ => Ok(json!({"ok": true})),
        }
    }
}

#[tokio::test]
async fn failed_attempt_repeat_after_keyed_command_is_allowed() {
    let store: Arc<dyn RunStore> = Arc::new(kernel_core::InMemoryRunStore::new());
    let oracle = Arc::new(O {
        script: Mutex::new(vec![
            ProposedAction {
                action: "czytaj".into(),
                arguments: json!({"plik":"lib.rs"}),
                rationale: None,
            },
            ProposedAction {
                action: "popraw".into(),
                arguments: json!({"instrukcja":"X"}),
                rationale: None,
            },
            ProposedAction {
                action: "popraw".into(),
                arguments: json!({"instrukcja":"X2"}),
                rationale: None,
            },
            ProposedAction {
                action: "popraw".into(),
                arguments: json!({"instrukcja":"X3"}),
                rationale: None,
            },
            ProposedAction {
                action: "uruchom".into(),
                arguments: json!({}),
                rationale: None,
            },
            ProposedAction {
                action: "popraw".into(),
                arguments: json!({"instrukcja":"X"}),
                rationale: None,
            }, // identical to failed #1
            ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary":"done"}),
                rationale: None,
            },
        ]),
        calls: AtomicUsize::new(0),
    });
    let tools = Arc::new(T {
        popraw_fails: AtomicUsize::new(0),
        ex: Mutex::new(vec![]),
    });
    let k = Kernel::new(
        oracle.clone(),
        tools.clone(),
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig {
            latitude: Latitude::Standard,
            ..KernelConfig::default()
        },
    );
    let run = k.run_task("probe".into()).await.unwrap();
    println!("STATUS {:?} ERR {:?}", run.status, run.error);
    for s in &run.steps {
        println!("step {}: {} ok={}", s.index, s.action, s.ok);
    }
    assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
}
