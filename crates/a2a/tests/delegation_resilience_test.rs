//! Governed resilience (ADR 0023, Part 4): retry, sanctioned fallback, and
//! the circuit-breaker — real `Kernel` + real mock A2A HTTP servers (axum).
//! The one non-negotiable under test throughout: every retry/fallback
//! attempt re-enters `Kernel::authorize_delegation` (P2: budget/depth/
//! cycle) — proven by `retry_counts_toward_p2_budget` — and, when
//! configured, the `DelegationGate` (P3: trust) — proven by the
//! untrusted-fallback half of `fallback_only_to_sanctioned_agent_still_governed`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use a2a::{
    action_name, A2aActionSource, AgentDelegationPolicy, AgentRegistry, RegistryDelegationGate,
    RemoteAgent,
};
use kernel_core::{
    ActionSchema, DelegationLimits, InMemoryRunStore, Kernel, KernelConfig, PolicyContext,
    PolicyError, PolicyOracle, Proposal, ProposedAction, ResilienceConfig, RunEvent, RunStatus,
};
use memory::InMemoryProvider;

fn tmp() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kelter-delegation-res-{}", uuid_ish()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_ish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as O};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, O::Relaxed)
    )
}

/// A mock agent whose failure behavior can be toggled AFTER the server has
/// already started (needed for the circuit-recovery test: trip it, then
/// heal it, without spawning a second server on a second port).
#[derive(Default)]
struct MockAgentState {
    name: String,
    send_calls: AtomicUsize,
    always_fail: std::sync::atomic::AtomicBool,
    endpoint: Mutex<String>,
}

async fn card(State(st): State<Arc<MockAgentState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": st.name,
        "description": format!("mock agent '{}'", st.name),
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
    }))
}

async fn rpc(State(st): State<Arc<MockAgentState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    if method != "SendMessage" {
        return Json(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "?"}}),
        );
    }
    st.send_calls.fetch_add(1, Ordering::SeqCst);
    if st.always_fail.load(Ordering::SeqCst) {
        Json(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "mock failure"}}),
        )
    } else {
        Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"message": {"parts": [{"text": "ok"}]}}
        }))
    }
}

async fn spawn_agent(name: &str, always_fail: bool) -> (String, Arc<MockAgentState>) {
    let state = Arc::new(MockAgentState {
        name: name.to_string(),
        always_fail: std::sync::atomic::AtomicBool::new(always_fail),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    *state.endpoint.lock().unwrap() = format!("{base}/rpc");
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state)
}

/// Proposes ONE fixed `delegate_to_*` action fresh, `finish` on every
/// subsequent decision point — the kernel's own resilience loop (not the
/// model) drives every retry/fallback attempt.
struct DelegateOnceOracle {
    action: String,
}

#[async_trait]
impl PolicyOracle for DelegateOnceOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        if context.steps.is_empty() && context.last_error.is_none() {
            return Ok(ProposedAction {
                action: self.action.clone(),
                arguments: json!({"task": "do work"}),
                rationale: None,
            }
            .into());
        }
        Ok(ProposedAction {
            action: "finish".into(),
            arguments: json!({"summary": "done"}),
            rationale: None,
        }
        .into())
    }
    fn describe(&self) -> String {
        "mock:delegate-once".into()
    }
}

async fn registered_agent(
    registry: &AgentRegistry,
    source: &A2aActionSource,
    name: &str,
    always_fail: bool,
    trust: &str,
) -> (Arc<MockAgentState>, String) {
    let (base, state) = spawn_agent(name, always_fail).await;
    let manifest = registry
        .register(&base, None, "discovery", trust)
        .await
        .unwrap();
    if trust == "trusted" {
        registry.set_trust(&manifest.name, "trusted").unwrap();
    }
    source
        .register_runtime(&RemoteAgent {
            name: manifest.name.clone(),
            card_url: base,
            token: None,
        })
        .unwrap();
    (state, action_name(&manifest.name))
}

// ---- (4) retry counts toward the P2 budget ---------------------------------

#[tokio::test]
async fn retry_counts_toward_p2_budget() {
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
    let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
    let (state, action) =
        registered_agent(&registry, &source, "flaky-primary", true, "trusted").await;

    let gate = Arc::new(RegistryDelegationGate::new(
        registry,
        AgentDelegationPolicy::TrustedOnly,
    ));
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let kernel = Arc::new(
        Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: action.clone(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig {
                delegation: DelegationLimits {
                    max_delegations_per_run: 3,
                    max_depth: 3,
                },
                resilience: ResilienceConfig {
                    max_retries_per_delegation: 5, // more than the budget allows
                    fallback: Default::default(),
                },
                ..KernelConfig::default()
            },
        )
        .with_delegation_gate(delegation_gate),
    );

    let mut events = kernel.subscribe();
    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();

    assert_eq!(
        run.status,
        // Every dial failed or was refused: no effective action (ADR 0033).
        RunStatus::Incomplete,
        "the model falls through to finish"
    );
    assert_eq!(
        state.send_calls.load(Ordering::SeqCst),
        3,
        "exactly budget-many dials: 1 fresh + 2 retries, THEN the budget-exhausted refusal — \
         never a 4th, silently-continuing dial despite max_retries_per_delegation=5"
    );
    let mut saw_budget_refusal = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Rejected { reason, .. } = &env.event {
            if reason.contains("budget exhausted") {
                saw_budget_refusal = true;
            }
        }
    }
    assert!(
        saw_budget_refusal,
        "a budget-exhausted stop must be a governed, loud refusal — never a silent stop"
    );
}

// ---- (5) fallback only to a SANCTIONED agent, still governed ---------------

#[tokio::test]
async fn fallback_only_to_sanctioned_agent_still_governed() {
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
    let source = Arc::new(A2aActionSource::new(vec![]).unwrap());

    let (primary_state, primary_action) =
        registered_agent(&registry, &source, "primary", true, "trusted").await;
    let (sanctioned_state, sanctioned_action) =
        registered_agent(&registry, &source, "sanctioned-fallback", false, "trusted").await;
    let (unsanctioned_state, _unsanctioned_action) =
        registered_agent(&registry, &source, "unsanctioned-agent", false, "trusted").await;

    let gate = Arc::new(RegistryDelegationGate::new(
        registry.clone(),
        AgentDelegationPolicy::TrustedOnly,
    ));
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let mut fallback = std::collections::HashMap::new();
    fallback.insert(primary_action.clone(), vec![sanctioned_action.clone()]);

    let kernel = Arc::new(
        Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: primary_action.clone(),
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig {
                resilience: ResilienceConfig {
                    max_retries_per_delegation: 0, // straight to fallback on first failure
                    fallback,
                },
                ..KernelConfig::default()
            },
        )
        .with_delegation_gate(delegation_gate),
    );

    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        primary_state.send_calls.load(Ordering::SeqCst),
        1,
        "primary dialed once, failed"
    );
    assert_eq!(
        sanctioned_state.send_calls.load(Ordering::SeqCst),
        1,
        "the SANCTIONED fallback is dialed and succeeds"
    );
    assert_eq!(
        unsanctioned_state.send_calls.load(Ordering::SeqCst),
        0,
        "an unconfigured alternative is NEVER tried automatically"
    );
    let final_step = run.steps.last().expect("a step was recorded");
    assert!(
        final_step.ok,
        "the run's outcome reflects the successful sanctioned fallback"
    );
}

/// The other half of "fallback only to a sanctioned agent, still governed"
/// (design doc test 5): a configured fallback that is itself UNTRUSTED
/// (under TrustedOnly) is refused, not silently skipped-past — kept as its
/// own test function for a clean, independent fixture rather than a second
/// scenario crammed into the test above.
#[tokio::test]
async fn fallback_to_untrusted_agent_is_refused_not_skipped() {
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
    let source = Arc::new(A2aActionSource::new(vec![]).unwrap());

    let (primary_state, primary_action) =
        registered_agent(&registry, &source, "primary-b", true, "trusted").await;
    let (untrusted_fb_state, untrusted_fb_action) =
        registered_agent(&registry, &source, "untrusted-fallback", false, "untrusted").await;

    let gate = Arc::new(RegistryDelegationGate::new(
        registry,
        AgentDelegationPolicy::TrustedOnly,
    ));
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let mut fallback = std::collections::HashMap::new();
    fallback.insert(primary_action.clone(), vec![untrusted_fb_action]);

    let kernel = Arc::new(
        Kernel::new(
            Arc::new(DelegateOnceOracle {
                action: primary_action,
            }),
            source,
            Arc::new(InMemoryProvider::new()),
            Arc::new(InMemoryRunStore::new()),
            KernelConfig {
                resilience: ResilienceConfig {
                    max_retries_per_delegation: 0,
                    fallback,
                },
                ..KernelConfig::default()
            },
        )
        .with_delegation_gate(delegation_gate),
    );

    let mut events = kernel.subscribe();
    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();

    // No successful action behind `finish` → Incomplete, not Completed (ADR 0033).
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(primary_state.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        untrusted_fb_state.send_calls.load(Ordering::SeqCst),
        0,
        "an untrusted sanctioned fallback is refused by P3 — never dialed, never silently skipped \
         to a THIRD candidate"
    );
    let mut saw_trust_refusal = false;
    while let Ok(env) = events.try_recv() {
        if let RunEvent::Rejected { reason, .. } = &env.event {
            if reason.contains("untrusted-fallback") {
                saw_trust_refusal = true;
            }
        }
    }
    assert!(saw_trust_refusal);
}

// ---- (6) circuit breaker trips, cools down, recovers -----------------------

#[tokio::test]
async fn circuit_breaker_trips_cools_down_and_recovers() {
    let (base, state) = spawn_agent("flaky", true).await;
    let root = tmp();
    let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
    let manifest = registry
        .register(&base, None, "discovery", "trusted")
        .await
        .unwrap();
    registry.set_trust(&manifest.name, "trusted").unwrap();
    let action = action_name(&manifest.name);

    let cooldown = Duration::from_millis(250);
    let source = Arc::new(
        A2aActionSource::new(vec![])
            .unwrap()
            .with_circuit_breaker(3, cooldown),
    );
    source
        .register_runtime(&RemoteAgent {
            name: manifest.name.clone(),
            card_url: base,
            token: None,
        })
        .unwrap();

    let gate = Arc::new(RegistryDelegationGate::new(
        registry,
        AgentDelegationPolicy::All,
    ));
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;

    let make_kernel = |source: Arc<A2aActionSource>, gate: Arc<dyn kernel_core::DelegationGate>| {
        Arc::new(
            Kernel::new(
                Arc::new(DelegateOnceOracle {
                    action: action.clone(),
                }),
                source,
                Arc::new(InMemoryProvider::new()),
                Arc::new(InMemoryRunStore::new()),
                KernelConfig {
                    resilience: ResilienceConfig {
                        max_retries_per_delegation: 0, // isolate circuit logic: one dial per run
                        fallback: Default::default(),
                    },
                    ..KernelConfig::default()
                },
            )
            .with_delegation_gate(gate),
        )
    };

    // Three consecutive failures trip the circuit.
    for _ in 0..3 {
        let kernel = make_kernel(source.clone(), delegation_gate.clone());
        let run_id = kernel.create_run("t".into()).await.unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();
        // The dial failed: no effective action (ADR 0033).
        assert_eq!(run.status, RunStatus::Incomplete);
    }
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 3);

    // A dial attempt DURING cooldown is refused WITHOUT dialing.
    let kernel = make_kernel(source.clone(), delegation_gate.clone());
    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    // Refused without dialing: no effective action (ADR 0033).
    assert_eq!(run.status, RunStatus::Incomplete);
    assert_eq!(
        state.send_calls.load(Ordering::SeqCst),
        3,
        "circuit open — the mock agent's call count must NOT increase"
    );
    let final_step = run.steps.last().unwrap();
    assert!(!final_step.ok);
    assert!(
        final_step.result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("circuit open"),
        "the refusal reason must be recognizable, got: {:?}",
        final_step.result
    );

    // After cooldown: one half-open probe is allowed. Heal the agent first
    // so that probe succeeds and CLOSES the circuit.
    tokio::time::sleep(cooldown + Duration::from_millis(100)).await;
    state.always_fail.store(false, Ordering::SeqCst);
    let kernel = make_kernel(source.clone(), delegation_gate.clone());
    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        state.send_calls.load(Ordering::SeqCst),
        4,
        "the half-open probe DID dial (a real attempt, not another refusal)"
    );
    assert!(
        run.steps.last().unwrap().ok,
        "the probe succeeded, closing the circuit"
    );

    // Confirm it is fully closed: a normal dial follows immediately.
    let kernel = make_kernel(source, delegation_gate);
    let run_id = kernel.create_run("t".into()).await.unwrap();
    let run = kernel.execute_run(run_id).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 5);
    assert!(run.steps.last().unwrap().ok);
}

// ---- (7) circuit-open skips to fallback; budget refusal aborts entirely ----

#[tokio::test]
async fn circuit_open_skips_to_fallback_but_budget_refusal_aborts_entirely() {
    // (a) circuit-open on the primary skips straight to a sanctioned
    //     fallback rather than aborting the whole loop.
    {
        let root = tmp();
        let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
        let source = Arc::new(
            A2aActionSource::new(vec![])
                .unwrap()
                .with_circuit_breaker(2, Duration::from_secs(120)),
        );
        let (primary_state, primary_action) =
            registered_agent(&registry, &source, "circuit-primary", true, "trusted").await;
        let (fallback_state, fallback_action) =
            registered_agent(&registry, &source, "circuit-fallback", false, "trusted").await;

        let gate = Arc::new(RegistryDelegationGate::new(
            registry,
            AgentDelegationPolicy::TrustedOnly,
        ));
        let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;
        let mut fallback = std::collections::HashMap::new();
        fallback.insert(primary_action.clone(), vec![fallback_action]);

        let make_kernel = || {
            Arc::new(
                Kernel::new(
                    Arc::new(DelegateOnceOracle {
                        action: primary_action.clone(),
                    }),
                    source.clone(),
                    Arc::new(InMemoryProvider::new()),
                    Arc::new(InMemoryRunStore::new()),
                    KernelConfig {
                        resilience: ResilienceConfig {
                            max_retries_per_delegation: 0,
                            fallback: fallback.clone(),
                        },
                        ..KernelConfig::default()
                    },
                )
                .with_delegation_gate(delegation_gate.clone()),
            )
        };

        // Trip the primary's circuit (threshold 2) via two failing runs —
        // fallback also gets tried and succeeds each time, which is fine;
        // what matters is the PRIMARY's own failure streak.
        for _ in 0..2 {
            let kernel = make_kernel();
            let run_id = kernel.create_run("t".into()).await.unwrap();
            kernel.execute_run(run_id).await.unwrap();
        }
        assert_eq!(primary_state.send_calls.load(Ordering::SeqCst), 2);
        let fallback_calls_before = fallback_state.send_calls.load(Ordering::SeqCst);

        // Now the primary's circuit is open. A fresh run must skip straight
        // to fallback WITHOUT an extra dial against the tripped primary.
        let kernel = make_kernel();
        let run_id = kernel.create_run("t".into()).await.unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(
            primary_state.send_calls.load(Ordering::SeqCst),
            2,
            "circuit-open on the primary must not add a new dial against it"
        );
        assert_eq!(
            fallback_state.send_calls.load(Ordering::SeqCst),
            fallback_calls_before + 1,
            "the loop proceeds straight to the sanctioned fallback rather than aborting"
        );
        assert!(run.steps.last().unwrap().ok);
    }

    // (b) a budget/depth/cycle refusal aborts the WHOLE loop — a sanctioned
    //     fallback is configured but must NEVER be tried once P2 refuses.
    {
        let root = tmp();
        let registry = Arc::new(AgentRegistry::open(root.join("agents.toml"), &[]).unwrap());
        let source = Arc::new(A2aActionSource::new(vec![]).unwrap());
        let (primary_state, primary_action) =
            registered_agent(&registry, &source, "budget-primary", true, "trusted").await;
        let (fallback_state, fallback_action) =
            registered_agent(&registry, &source, "budget-fallback", false, "trusted").await;

        let gate = Arc::new(RegistryDelegationGate::new(
            registry,
            AgentDelegationPolicy::TrustedOnly,
        ));
        let delegation_gate: Arc<dyn kernel_core::DelegationGate> = gate;
        let mut fallback = std::collections::HashMap::new();
        fallback.insert(primary_action.clone(), vec![fallback_action]);

        let kernel = Arc::new(
            Kernel::new(
                Arc::new(DelegateOnceOracle {
                    action: primary_action,
                }),
                source,
                Arc::new(InMemoryProvider::new()),
                Arc::new(InMemoryRunStore::new()),
                KernelConfig {
                    delegation: DelegationLimits {
                        max_delegations_per_run: 1, // exhausted by the FIRST dial
                        max_depth: 3,
                    },
                    resilience: ResilienceConfig {
                        max_retries_per_delegation: 3,
                        fallback,
                    },
                    ..KernelConfig::default()
                },
            )
            .with_delegation_gate(delegation_gate),
        );

        let mut events = kernel.subscribe();
        let run_id = kernel.create_run("t".into()).await.unwrap();
        let run = kernel.execute_run(run_id).await.unwrap();

        // The one dial failed and the fallback was never tried (ADR 0033).
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(primary_state.send_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_state.send_calls.load(Ordering::SeqCst),
            0,
            "a budget refusal aborts the ENTIRE resilience loop — the sanctioned fallback must \
             NEVER be tried, unlike a circuit-open primary which skips to it"
        );
        let mut saw_budget_refusal = false;
        while let Ok(env) = events.try_recv() {
            if let RunEvent::Rejected { reason, .. } = &env.event {
                if reason.contains("budget exhausted") {
                    saw_budget_refusal = true;
                }
            }
        }
        assert!(saw_budget_refusal);
    }
}
