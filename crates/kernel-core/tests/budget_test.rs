//! Cost budgets (ADR 0039): dollars, tokens and wall time per run, and a
//! daily dollar ceiling per principal — each stopping the run BEFORE the
//! model call (or action) that would cross it, never after; the per-run
//! override; and an unpriced model, where the dollar limit cannot apply but
//! the token limit still does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use kernel_core::journal::{RejectionSource, RejectionSubject};
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, BudgetCheckpoint, BudgetConfig, BudgetLimitKind,
    BudgetLimits, CallBound, ExecutionContext, ExecutionError, InMemoryRunStore, JournalEntry,
    Kernel, KernelConfig, KernelError, Latitude, ModelPrice, PolicyContext, PolicyError,
    PolicyOracle, PricingTable, Proposal, ProposedAction, RunEvent, RunMode, RunOptions, RunStatus,
    RunStore, Usage, ValidatedAction,
};
use memory::InMemoryProvider;

const MODEL: &str = "mock:budget";

/// Every call reports `usage`; `bound` is what the adapter promises a call
/// can cost at worst (prompt bytes, `max_tokens`).
struct Oracle {
    script: Mutex<Vec<ProposedAction>>,
    calls: AtomicUsize,
    usage: Usage,
    bound: CallBound,
}

#[async_trait]
impl PolicyOracle for Oracle {
    async fn propose(
        &self,
        _c: &PolicyContext,
        _s: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut s = self.script.lock().await;
        if s.is_empty() {
            return Err(PolicyError::Backend("script exhausted".into()));
        }
        Ok(Proposal {
            action: s.remove(0),
            usage: Some(self.usage),
        })
    }
    fn describe(&self) -> String {
        MODEL.into()
    }
    fn call_bound(&self, _c: &PolicyContext, _s: &ActionSchema) -> CallBound {
        self.bound
    }
}

struct Tools {
    runs: AtomicUsize,
}

#[async_trait]
impl ActionSource for Tools {
    async fn action_schema(&self, _l: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: "work".into(),
                description: "do a unit of work".into(),
                parameters: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            }],
        })
    }
    async fn execute(
        &self,
        _a: &ValidatedAction,
        _c: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok": true}))
    }
}

fn p(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

/// Three work steps then finish — four model calls if nothing stops it.
fn long_script() -> Vec<ProposedAction> {
    vec![
        p("work", json!({"n": 1})),
        p("work", json!({"n": 2})),
        p("work", json!({"n": 3})),
        p("finish", json!({"summary": "done"})),
    ]
}

struct H {
    kernel: Kernel,
    store: Arc<InMemoryRunStore>,
    oracle: Arc<Oracle>,
    tools: Arc<Tools>,
}

fn usage(p: u64, c: u64) -> Usage {
    Usage {
        prompt_tokens: p,
        completion_tokens: c,
        calls: 1,
    }
}

fn harness(
    script: Vec<ProposedAction>,
    mode: RunMode,
    budget: BudgetConfig,
    priced: bool,
    store: Option<Arc<InMemoryRunStore>>,
) -> H {
    let store = store.unwrap_or_else(|| Arc::new(InMemoryRunStore::new()));
    let oracle = Arc::new(Oracle {
        script: Mutex::new(script),
        calls: AtomicUsize::new(0),
        // Each call really costs 100 tokens …
        usage: usage(80, 20),
        // … and the adapter bounds each at 150.
        bound: CallBound {
            prompt_tokens: 100,
            completion_tokens: Some(50),
        },
    });
    let tools = Arc::new(Tools {
        runs: AtomicUsize::new(0),
    });
    let pricing = if priced {
        // $1 per 1k tokens — easy arithmetic: 100 tokens = $0.10.
        PricingTable::new(HashMap::from([(
            MODEL.to_string(),
            ModelPrice {
                input_per_mtok: 1000.0,
                output_per_mtok: 1000.0,
            },
        )]))
    } else {
        PricingTable::default()
    };
    let kernel = Kernel::new(
        oracle.clone(),
        tools.clone(),
        Arc::new(InMemoryProvider::new()),
        store.clone(),
        KernelConfig {
            max_steps: 8,
            latitude: Latitude::Wide,
            reflection_enabled: false,
            mode,
            pricing,
            ..KernelConfig::default()
        },
    )
    .with_budget(budget);
    H {
        kernel,
        store,
        oracle,
        tools,
    }
}

fn budget(limits: BudgetLimits) -> BudgetConfig {
    BudgetConfig {
        per_run: limits,
        // The harness oracle's bound is exact; no hidden overhead.
        prompt_overhead_tokens: 0,
        fallback_completion_tokens: 4096,
        ..BudgetConfig::default()
    }
}

fn budget_rejections(journal: &[JournalEntry]) -> Vec<RejectionSubject> {
    journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Rejected {
                source: RejectionSource::Budget,
                subject,
                ..
            } => Some(*subject),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn no_budget_means_no_limit_and_no_budget_field() {
    let h = harness(
        long_script(),
        RunMode::Step,
        BudgetConfig::default(),
        true,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(run.budget.is_none() && run.budget_stop.is_none());
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn token_limit_stops_before_the_call_that_would_cross_it() {
    // 350 tokens: calls 1–2 use 200; call 3 needs ≤150 → 350 fits; call 4
    // would need 300 + 150 = 450 > 350 → refused BEFORE it is made.
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_tokens: Some(350),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let mut rx = h.kernel.subscribe();
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        h.oracle.calls.load(Ordering::SeqCst),
        3,
        "call 4 never made"
    );
    assert!(
        run.usage.prompt_tokens + run.usage.completion_tokens <= 350,
        "the limit was never crossed: {:?}",
        run.usage
    );
    let stop = run.budget_stop.clone().unwrap();
    assert_eq!(stop.limit, BudgetLimitKind::Tokens);
    assert_eq!(stop.checkpoint, BudgetCheckpoint::ModelCall);
    assert_eq!((stop.used, stop.requested, stop.max), (300.0, 150.0, 350.0));
    assert!(
        run.error.as_deref().unwrap().contains("token budget"),
        "{:?}",
        run.error
    );
    let journal = h.store.journal(run.id).await.unwrap();
    assert_eq!(budget_rejections(&journal), vec![RejectionSubject::None]);
    assert!(journal.iter().any(|e| matches!(
        e,
        JournalEntry::Terminal {
            status: RunStatus::Failed,
            ..
        }
    )));
    let mut saw = false;
    while let Ok(env) = rx.try_recv() {
        if let RunEvent::BudgetExceeded { limit, .. } = env.event {
            saw = limit == BudgetLimitKind::Tokens;
        }
    }
    assert!(saw, "budget_exceeded on the live stream");
}

#[tokio::test]
async fn refused_proposals_count_against_the_token_budget() {
    // Schema-invalid proposals (unknown action) still cost tokens: after one
    // refused call (100 used), a second (worst case 150) would cross 200. A
    // looping model cannot burn the budget uncounted.
    let h = harness(
        vec![
            p("nope", json!({})),
            p("nope", json!({})),
            p("nope", json!({})),
        ],
        RunMode::Step,
        budget(BudgetLimits {
            max_tokens: Some(200),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.usage.calls, 1, "{:?}", run.usage);
    assert_eq!(run.budget_stop.unwrap().limit, BudgetLimitKind::Tokens);
}

#[tokio::test]
async fn dollar_limit_stops_a_priced_model() {
    // $1/1k tokens: call 4 would need $0.30 + $0.15 > $0.36.
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_usd: Some(0.36),
            ..BudgetLimits::default()
        }),
        true,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.budget.unwrap().usd_enforced);
    assert_eq!(run.budget_stop.unwrap().limit, BudgetLimitKind::Usd);
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 3);
    assert!(run.cost_usd.unwrap() <= 0.36);
}

#[tokio::test]
async fn unpriced_model_disables_the_dollar_limit_but_not_the_token_limit() {
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_usd: Some(0.000_001),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert!(!run.budget.unwrap().usd_enforced);
    assert_eq!(run.cost_usd, None);

    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_usd: Some(0.000_001),
            max_tokens: Some(200),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.budget_stop.unwrap().limit, BudgetLimitKind::Tokens);
}

#[tokio::test]
async fn wall_clock_limit_stops_before_the_next_call_and_excludes_human_pauses() {
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_wall_secs: Some(10),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    // A run created 100 s ago is already past its 10 s.
    let id = h.kernel.create_run("t".into()).await.unwrap();
    let mut run = h.store.get(id).await.unwrap();
    run.created_at -= 100;
    h.store.put(run).await.unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 0);
    let stop = run.budget_stop.unwrap();
    assert_eq!(stop.limit, BudgetLimitKind::WallTime);
    assert!(stop.used >= 100.0);

    // The same age, but 95 s of it spent waiting on a human: within budget.
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_wall_secs: Some(10),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    let mut run = h.store.get(id).await.unwrap();
    run.created_at -= 100;
    run.paused_secs = 95;
    h.store.put(run).await.unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
}

#[tokio::test]
async fn wall_clock_is_checked_before_an_action_too() {
    // A replayed decision needs no model call; the action itself is checked.
    let h = harness(
        vec![p("finish", json!({"summary": "x"}))],
        RunMode::Step,
        budget(BudgetLimits {
            max_wall_secs: Some(10),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let id = h.kernel.create_run("t".into()).await.unwrap();
    h.store
        .append_journal(
            id,
            JournalEntry::Decision {
                step: 0,
                action: "work".into(),
                arguments: json!({"n": 1}),
                usage: None,
            },
        )
        .await
        .unwrap();
    let mut run = h.store.get(id).await.unwrap();
    run.created_at -= 100;
    h.store.put(run).await.unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        h.tools.runs.load(Ordering::SeqCst),
        0,
        "the action never ran"
    );
    let stop = run.budget_stop.unwrap();
    assert_eq!(
        (stop.limit, stop.checkpoint),
        (BudgetLimitKind::WallTime, BudgetCheckpoint::Action)
    );
    let journal = h.store.journal(id).await.unwrap();
    assert_eq!(
        budget_rejections(&journal),
        vec![RejectionSubject::Decision]
    );
}

#[tokio::test]
async fn per_run_override_tightens_and_cannot_loosen() {
    let h = harness(
        long_script(),
        RunMode::Step,
        budget(BudgetLimits {
            max_tokens: Some(10_000),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let too_big = h
        .kernel
        .create_run_with_options(
            "t".into(),
            RunOptions {
                budget: Some(BudgetLimits {
                    max_tokens: Some(20_000),
                    ..BudgetLimits::default()
                }),
                ..RunOptions::default()
            },
        )
        .await;
    assert!(matches!(too_big, Err(KernelError::InvalidRequest(m)) if m.contains("ceiling")));

    let id = h
        .kernel
        .create_run_with_options(
            "t".into(),
            RunOptions {
                budget: Some(BudgetLimits {
                    max_tokens: Some(150),
                    ..BudgetLimits::default()
                }),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.budget.unwrap().limits.max_tokens, Some(150));
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn override_sets_a_limit_the_config_leaves_open() {
    let h = harness(
        long_script(),
        RunMode::Step,
        BudgetConfig::default(),
        false,
        None,
    );
    let id = h
        .kernel
        .create_run_with_options(
            "t".into(),
            RunOptions {
                budget: Some(BudgetLimits {
                    max_tokens: Some(200),
                    ..BudgetLimits::default()
                }),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
    let run = h.kernel.execute_run(id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.budget_stop.unwrap().limit, BudgetLimitKind::Tokens);
}

#[tokio::test]
async fn daily_ceiling_counts_the_principals_other_runs() {
    let store = Arc::new(InMemoryRunStore::new());
    let cfg = BudgetConfig {
        daily_usd_per_principal: Some(0.5),
        prompt_overhead_tokens: 0,
        fallback_completion_tokens: 4096,
        ..BudgetConfig::default()
    };
    let opts = |who: &str| RunOptions {
        principal: Some(who.into()),
        ..RunOptions::default()
    };
    // alice's first run spends $0.40 (four calls × $0.10).
    let h = harness(
        long_script(),
        RunMode::Step,
        cfg.clone(),
        true,
        Some(store.clone()),
    );
    let first = h
        .kernel
        .create_run_with_options("t".into(), opts("alice"))
        .await
        .unwrap();
    let first = h.kernel.execute_run(first).await.unwrap();
    assert_eq!(first.status, RunStatus::Completed);
    assert!((first.cost_usd.unwrap() - 0.4).abs() < 1e-9);

    // Her second run: $0.40 spent + $0.15 worst case > $0.50 → refused
    // before its first call.
    let h = harness(
        long_script(),
        RunMode::Step,
        cfg.clone(),
        true,
        Some(store.clone()),
    );
    let second = h
        .kernel
        .create_run_with_options("t".into(), opts("alice"))
        .await
        .unwrap();
    let second = h.kernel.execute_run(second).await.unwrap();
    assert_eq!(second.status, RunStatus::Failed);
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 0);
    let stop = second.budget_stop.unwrap();
    assert_eq!(stop.limit, BudgetLimitKind::DailyUsd);
    assert!((stop.used - 0.4).abs() < 1e-9);

    // bob is unaffected.
    let h = harness(long_script(), RunMode::Step, cfg, true, Some(store));
    let bob = h
        .kernel
        .create_run_with_options("t".into(), opts("bob"))
        .await
        .unwrap();
    assert_eq!(
        h.kernel.execute_run(bob).await.unwrap().status,
        RunStatus::Completed
    );
}

#[tokio::test]
async fn plan_mode_checks_the_proposal_and_every_node() {
    // Proposal: worst case 150 > 100 → stopped before the plan is asked for.
    let h = harness(
        vec![p(
            "propose_plan",
            json!({"nodes": [{"id": "a", "type": "tool", "action": "work", "arguments": {}}]}),
        )],
        RunMode::Plan,
        budget(BudgetLimits {
            max_tokens: Some(100),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.oracle.calls.load(Ordering::SeqCst), 0);

    // A review node is a model call: 100 (plan) + 150 (review bound) > 200,
    // so the review is refused before it is admitted; the tool node before it
    // ran.
    let h = harness(
        vec![
            p(
                "propose_plan",
                json!({"nodes": [
                    {"id": "a", "type": "tool", "action": "work", "arguments": {}},
                    {"id": "r", "type": "review", "deps": ["a"]}
                ]}),
            ),
            p("review_verdict", json!({"approve": true, "reason": "ok"})),
        ],
        RunMode::Plan,
        budget(BudgetLimits {
            max_tokens: Some(200),
            ..BudgetLimits::default()
        }),
        false,
        None,
    );
    let run = h.kernel.run_task("t".into()).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(h.tools.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.oracle.calls.load(Ordering::SeqCst),
        1,
        "review call never made"
    );
    let stop = run.budget_stop.unwrap();
    assert_eq!(stop.branch.as_deref(), Some("r"));
    let journal = h.store.journal(run.id).await.unwrap();
    assert_eq!(
        budget_rejections(&journal),
        vec![RejectionSubject::PlanNode]
    );
}
