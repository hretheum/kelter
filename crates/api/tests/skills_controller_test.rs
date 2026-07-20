//! Skills management router contract (ADR 0021 addendum): a mock
//! `SkillsController` exercised over real HTTP, plus the capability-flag
//! contract the frontend depends on — every `/api/v1/skills*` route 404s
//! when no controller is wired.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use api::{SkillInstallReport, SkillView, SkillsController, SkillsPolicyUpdate, SkillsPolicyView};
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, ValidatedAction,
};
use memory::InMemoryProvider;

struct EchoTools;

#[async_trait]
impl ActionSource for EchoTools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "Echo".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
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
        Ok(json!({"echoed": a.arguments()["message"]}))
    }
}

struct OneShotOracle;

#[async_trait]
impl PolicyOracle for OneShotOracle {
    async fn propose(
        &self,
        _context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        Ok(ProposedAction {
            action: "finish".into(),
            arguments: json!({"summary": "done"}),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:oneshot".into()
    }
}

fn kernel() -> Arc<Kernel> {
    Arc::new(Kernel::new(
        Arc::new(OneShotOracle),
        Arc::new(EchoTools),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    ))
}

/// An in-memory mock — proves the ROUTER's contract (status codes, body
/// shapes, wiring) without touching a real `SkillRegistry`/filesystem.
struct MockSkillsController {
    skills: Mutex<Vec<SkillView>>,
    policy: Mutex<SkillsPolicyView>,
    fail_install: bool,
}

impl MockSkillsController {
    fn new() -> Self {
        Self {
            skills: Mutex::new(Vec::new()),
            policy: Mutex::new(SkillsPolicyView {
                model_activation: "trusted".into(),
                ask_fallback: "deny".into(),
                ask_timeout_secs: 300,
            }),
            fail_install: false,
        }
    }
}

#[async_trait]
impl SkillsController for MockSkillsController {
    async fn list(&self) -> Vec<SkillView> {
        self.skills.lock().unwrap().clone()
    }

    async fn install_from_url(&self, url: String) -> Result<SkillInstallReport, String> {
        if self.fail_install {
            return Err(format!("cannot fetch {url}"));
        }
        let mut skills = self.skills.lock().unwrap();
        skills.push(SkillView {
            name: "installed-skill".into(),
            description: "d".into(),
            trust: "untrusted".into(),
            provenance_source_kind: "url".into(),
            provenance_source: url,
            active: false,
            has_scripts: false,
            has_extension_manifest: false,
        });
        Ok(SkillInstallReport {
            installed: vec!["installed-skill".into()],
            skipped: vec![],
        })
    }

    async fn remove(&self, name: &str) -> Result<(), String> {
        let mut skills = self.skills.lock().unwrap();
        let before = skills.len();
        skills.retain(|s| s.name != name);
        if skills.len() == before {
            return Err(format!("no such skill '{name}'"));
        }
        Ok(())
    }

    async fn activate(&self, name: &str) -> Result<(), String> {
        let mut skills = self.skills.lock().unwrap();
        let Some(s) = skills.iter_mut().find(|s| s.name == name) else {
            return Err(format!("no such skill '{name}'"));
        };
        s.active = true;
        Ok(())
    }

    async fn deactivate(&self, name: &str) -> Result<(), String> {
        let mut skills = self.skills.lock().unwrap();
        let Some(s) = skills.iter_mut().find(|s| s.name == name) else {
            return Err(format!("no such skill '{name}'"));
        };
        s.active = false;
        Ok(())
    }

    async fn get_policy(&self) -> SkillsPolicyView {
        self.policy.lock().unwrap().clone()
    }

    async fn set_policy(&self, update: SkillsPolicyUpdate) -> Result<SkillsPolicyView, String> {
        let mut policy = self.policy.lock().unwrap();
        policy.model_activation = update.model_activation;
        if let Some(f) = update.ask_fallback {
            policy.ask_fallback = f;
        }
        if let Some(t) = update.ask_timeout_secs {
            policy.ask_timeout_secs = t;
        }
        Ok(policy.clone())
    }
}

async fn spawn_with_skills(skills: Option<Arc<dyn SkillsController>>) -> (SocketAddr, String) {
    let token = "test-token-123".to_string();
    let app = api::router_with_principals_and_skills(
        kernel(),
        api::Principals::single(token.clone()),
        None,
        skills,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, token)
}

#[tokio::test]
async fn every_skills_route_404s_when_no_controller_is_wired() {
    let (addr, token) = spawn_with_skills(None).await;
    let client = reqwest::Client::new();

    let get_list = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(get_list.status(), 404);

    let install = client
        .post(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .json(&json!({"url": "http://example.com/x.tar.gz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(install.status(), 404);

    let remove = client
        .delete(format!("http://{addr}/api/v1/skills/some-skill"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(remove.status(), 404);

    let activate = client
        .post(format!("http://{addr}/api/v1/skills/some-skill/activate"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(activate.status(), 404);

    let deactivate = client
        .post(format!("http://{addr}/api/v1/skills/some-skill/deactivate"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(deactivate.status(), 404);

    let get_policy = client
        .get(format!("http://{addr}/api/v1/skills/policy"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(get_policy.status(), 404);

    let set_policy = client
        .post(format!("http://{addr}/api/v1/skills/policy"))
        .bearer_auth(&token)
        .json(&json!({"model_activation": "all"}))
        .send()
        .await
        .unwrap();
    assert_eq!(set_policy.status(), 404);
}

#[tokio::test]
async fn skills_routes_are_bearer_gated() {
    let controller: Arc<dyn SkillsController> = Arc::new(MockSkillsController::new());
    let (addr, _token) = spawn_with_skills(Some(controller)).await;
    let res = reqwest::get(format!("http://{addr}/api/v1/skills"))
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "no bearer token → 401, even wired");
}

#[tokio::test]
async fn list_install_remove_round_trip() {
    let controller: Arc<dyn SkillsController> = Arc::new(MockSkillsController::new());
    let (addr, token) = spawn_with_skills(Some(controller)).await;
    let client = reqwest::Client::new();

    // Empty at first.
    let list: Vec<SkillView> = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list.is_empty());

    // Install.
    let res = client
        .post(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .json(&json!({"url": "http://example.com/x.tar.gz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let report: SkillInstallReport = res.json().await.unwrap();
    assert_eq!(report.installed, vec!["installed-skill".to_string()]);

    // Now populated.
    let list: Vec<SkillView> = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "installed-skill");
    assert!(!list[0].active);

    // Activate → reflected in a subsequent list.
    let res = client
        .post(format!(
            "http://{addr}/api/v1/skills/installed-skill/activate"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let list: Vec<SkillView> = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list[0].active);

    // Deactivate → reflected too.
    let res = client
        .post(format!(
            "http://{addr}/api/v1/skills/installed-skill/deactivate"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let list: Vec<SkillView> = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!list[0].active);

    // Remove.
    let res = client
        .delete(format!("http://{addr}/api/v1/skills/installed-skill"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let list: Vec<SkillView> = client
        .get(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list.is_empty());

    // Remove of an unknown skill fails cleanly.
    let res = client
        .delete(format!("http://{addr}/api/v1/skills/nope"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
}

#[tokio::test]
async fn policy_get_set_round_trip() {
    let controller: Arc<dyn SkillsController> = Arc::new(MockSkillsController::new());
    let (addr, token) = spawn_with_skills(Some(controller)).await;
    let client = reqwest::Client::new();

    let before: SkillsPolicyView = client
        .get(format!("http://{addr}/api/v1/skills/policy"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before.model_activation, "trusted");

    let updated: SkillsPolicyView = client
        .post(format!("http://{addr}/api/v1/skills/policy"))
        .bearer_auth(&token)
        .json(&json!({"model_activation": "ask", "ask_fallback": "queue", "ask_timeout_secs": 120}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated.model_activation, "ask");
    assert_eq!(updated.ask_fallback, "queue");
    assert_eq!(updated.ask_timeout_secs, 120);

    // Echoed back on a subsequent GET.
    let after: SkillsPolicyView = client
        .get(format!("http://{addr}/api/v1/skills/policy"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after.model_activation, "ask");
    assert_eq!(after.ask_fallback, "queue");
    assert_eq!(after.ask_timeout_secs, 120);
}

#[tokio::test]
async fn install_failure_reports_422() {
    let controller = Arc::new(MockSkillsController {
        skills: Mutex::new(Vec::new()),
        policy: Mutex::new(SkillsPolicyView {
            model_activation: "trusted".into(),
            ask_fallback: "deny".into(),
            ask_timeout_secs: 300,
        }),
        fail_install: true,
    });
    let controller: Arc<dyn SkillsController> = controller;
    let (addr, token) = spawn_with_skills(Some(controller)).await;
    let client = reqwest::Client::new();

    let res = client
        .post(format!("http://{addr}/api/v1/skills"))
        .bearer_auth(&token)
        .json(&json!({"url": "http://example.com/x.tar.gz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
}
