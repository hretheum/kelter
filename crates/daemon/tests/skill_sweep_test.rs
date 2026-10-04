//! The `ask`-activation timeout sweeper (ADR 0021): a paused run must never
//! hang indefinitely. `daemon::skill_sweep::sweep_timed_out_skill_activations`
//! is extracted from `main()` specifically so this guarantee has a real,
//! deterministic unit test rather than only being provable by inspection or
//! a slow wall-clock integration run.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use daemon::skill_sweep::sweep_timed_out_skill_activations;
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
    ValidatedAction,
};
use memory::InMemoryProvider;
use serde_json::json;
use skills::{
    AskFallback, ModelActivationPolicy, RegistrySkillGate, SkillContextOracle, SkillRegistry,
};

fn tmp() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kelter-sweep-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

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

struct SelfActivatingOracle {
    skill: String,
}

#[async_trait]
impl PolicyOracle for SelfActivatingOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        if context.steps.is_empty() && context.last_error.is_none() {
            return Ok(ProposedAction {
                action: kernel_core::ACTIVATE_SKILL_ACTION.into(),
                arguments: json!({"name": self.skill}),
                rationale: Some("self-activating".into()),
            }
            .into());
        }
        let urged = context.guidance.as_deref().and_then(|g| {
            g.lines()
                .find_map(|l| l.trim().strip_prefix("USE ACTION: "))
                .map(str::trim)
        });
        let already_ran = urged.is_some_and(|a| context.steps.iter().any(|s| s.action == a));
        match urged {
            Some(action) if !already_ran && context.last_error.is_none() => Ok(ProposedAction {
                action: action.to_string(),
                arguments: json!({"message": "from self-activated skill"}),
                rationale: Some("following the self-activated skill".into()),
            }
            .into()),
            _ => Ok(ProposedAction {
                action: "finish".into(),
                arguments: json!({"summary": "done"}),
                rationale: None,
            }
            .into()),
        }
    }
    fn describe(&self) -> String {
        "mock:self-activating".into()
    }
}

#[tokio::test]
async fn timeout_sweeper_denies_and_resumes() {
    let root = tmp();
    let reg = Arc::new(SkillRegistry::open(root.join("store")).unwrap());
    let src = root.join("s");
    write_skill(&src, "urge-echo", "urges echo", "USE ACTION: echo");
    reg.install_from_dir(&src).unwrap();

    let oracle = Arc::new(SelfActivatingOracle {
        skill: "urge-echo".into(),
    });
    let ctx_oracle = Arc::new(SkillContextOracle::new(oracle, reg.clone()));
    let gate: Arc<dyn kernel_core::SkillGate> = Arc::new(
        RegistrySkillGate::new(reg.clone(), ModelActivationPolicy::Ask)
            .with_ask_fallback(AskFallback::Queue),
    );
    let kernel = Arc::new(
        Kernel::new(
            ctx_oracle,
            Arc::new(EchoTools),
            Arc::new(InMemoryProvider::new()),
            Arc::new(kernel_core::InMemoryRunStore::new()),
            KernelConfig::default(),
        )
        .with_skills(gate),
    );

    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::AwaitingSkillDecision);
    let requested_at = run.pending_skill_activation.unwrap().requested_at;

    // Before the timeout elapses: the sweep must not act.
    let acted = sweep_timed_out_skill_activations(&kernel, 300, requested_at + 10).await;
    assert!(
        acted.is_empty(),
        "must not act before the configured timeout has elapsed"
    );
    let run = kernel.store().get(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::AwaitingSkillDecision);

    // After the timeout: force-denies (principal system:timeout) and reports
    // the run id for the caller to resume.
    let acted = sweep_timed_out_skill_activations(&kernel, 300, requested_at + 301).await;
    assert_eq!(acted, vec![run_id]);

    // The caller (main.rs) re-invokes execute_run for each returned id —
    // mirrored here directly.
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(
        run.status,
        RunStatus::Incomplete,
        "a timed-out ask resolves to Deny and the run finishes without the skill — never hangs \
         (and, having done nothing effective, never as Completed: ADR 0033)"
    );
    assert!(!run.active_skills.contains(&"urge-echo".to_string()));
}
