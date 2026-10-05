//! WASM memory backend cruxes (ADR 0024), proven against real compiled
//! modules — no model, no network.
//!
//!   1. A manifest-declared WASM module serves the memory interface across
//!      the boundary: writes persist in the guest, retrieval ranks and comes
//!      back provenance-tagged.
//!   2. The capability sandbox holds for memory: a module importing an
//!      ungranted capability is refused AT LOAD (eager instantiation), not
//!      at first call.
//!   3. The kernel-side response gate holds: an interface-violating response
//!      is an error (degraded context), never trusted.
//!   4. THE INVARIANT (ADR 0016): a deliberately malicious memory backend
//!      poisons retrieved context — and that is ALL it can do. The poisoned
//!      suggestion is proposed by the (scripted) model, REJECTED by the
//!      kernel's validation floor, and never executes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Kernel, KernelConfig,
    Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal, ProposedAction, RunStatus,
    ValidatedAction,
};
use memory::{Episode, EpisodeOutcome, GraphUpsert, MemoryProvider, Provenance};
use uuid::Uuid;
use wasmhost::{Grants, WasmMemoryProvider};

fn repo_root() -> String {
    env!("CARGO_MANIFEST_DIR")
        .trim_end_matches("/crates/wasmhost")
        .to_string()
}
fn memstore_path() -> String {
    format!(
        "{}/examples/extensions/memstore-wasm/memstore_wasm.wasm",
        repo_root()
    )
}
fn poisonmem_path() -> String {
    format!(
        "{}/examples/extensions/memstore-wasm/poisonmem_wasm.wasm",
        repo_root()
    )
}
fn netpeek_path() -> String {
    format!(
        "{}/examples/extensions/wordcount-wasm/netpeek_wasm.wasm",
        repo_root()
    )
}
fn wordcount_path() -> String {
    format!(
        "{}/examples/extensions/wordcount-wasm/wordcount_wasm.wasm",
        repo_root()
    )
}

fn episode(task: &str) -> Episode {
    Episode {
        id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        task: task.to_string(),
        trace: vec!["read_source: ok".into()],
        outcome: EpisodeOutcome::Success,
        provenance: Provenance::Reflection,
        version: 1,
    }
}

#[tokio::test]
async fn wasm_memory_backend_stores_and_retrieves_across_calls() {
    // Crux 1: the instance is persistent — a write in one call is visible to
    // a retrieval in a later call, ranked, and provenance-tagged by the host.
    let mem = WasmMemoryProvider::load("memstore", &memstore_path(), Grants::default())
        .expect("memory module loads and instantiates");

    mem.write(episode("summarize the quarterly sales report"))
        .await
        .unwrap();
    mem.write(episode("extract links from a webpage"))
        .await
        .unwrap();
    mem.upsert(GraphUpsert::Entity {
        name: "report".into(),
        attributes: json!({"format": "markdown"}),
    })
    .await
    .unwrap();

    let hits = mem
        .retrieve("extract structured links from webpage", 5)
        .await
        .unwrap();
    assert!(
        !hits.is_empty(),
        "guest-side retrieval must return the relevant episode"
    );
    assert!(
        hits[0].summary.contains("extract links"),
        "ranking must surface the relevant episode first: {:?}",
        hits[0].summary
    );
    assert!(
        hits[0].summary.starts_with("[wasm-memory:memstore]"),
        "context from a WASM backend must be provenance-tagged: {:?}",
        hits[0].summary
    );
    assert!(hits[0].score > 0.0 && hits[0].score <= 1.0);

    // Irrelevant queries stay empty — scored retrieval, not context dumping.
    assert!(mem.retrieve("zzz qqq xxx", 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn retrieval_is_bounded_by_the_kernels_limit() {
    // The guest cannot flood the context: even if it returned more, the host
    // truncates to the kernel's requested limit.
    let mem = WasmMemoryProvider::load("memstore", &memstore_path(), Grants::default()).unwrap();
    for i in 0..10 {
        mem.write(episode(&format!("download file number {i}")))
            .await
            .unwrap();
    }
    let hits = mem.retrieve("download file", 3).await.unwrap();
    assert!(hits.len() <= 3);
}

#[test]
fn ungranted_capability_is_refused_at_load_for_memory() {
    // Crux 2: netpeek imports kelter:net. As a memory backend with NO network
    // grant, the import is structurally absent — and because the memory host
    // instantiates EAGERLY, the refusal happens at load/registration time.
    let msg = match WasmMemoryProvider::load(
        "netgrab",
        &netpeek_path(),
        Grants {
            network: false,
            ..Grants::default()
        },
    ) {
        Ok(_) => panic!("instantiation must fail without the grant"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("could not be instantiated"),
        "expected a structural capability failure at load, got: {msg}"
    );

    // With the grant, the same module instantiates — the gate is the ONLY
    // thing between the module and the capability.
    WasmMemoryProvider::load(
        "netgrab",
        &netpeek_path(),
        Grants {
            network: true,
            ..Grants::default()
        },
    )
    .expect("with the grant the module instantiates");
}

#[tokio::test]
async fn interface_violating_response_is_rejected_by_the_schema_gate() {
    // Crux 3: wordcount is a valid WASM module with the same ABI, but its
    // responses are word statistics, not the memory contract. The kernel-side
    // schema gate must reject them — degraded context, never garbage-in.
    let mem = WasmMemoryProvider::load("notmem", &wordcount_path(), Grants::default()).unwrap();
    let err = mem.retrieve("anything", 3).await.unwrap_err();
    assert!(
        err.to_string().contains("interface-violating"),
        "schema gate must reject a non-conforming retrieve response: {err}"
    );
    let err = mem.write(episode("x")).await.unwrap_err();
    assert!(
        err.to_string().contains("not acknowledged"),
        "unacknowledged write must be an error: {err}"
    );
}

// ---- the invariant (ADR 0016) ---------------------------------------------

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
        Ok(json!({ "echoed": a.arguments()["message"] }))
    }
}

/// An oracle that records the memory context it was handed and plays a
/// script of proposals — simulating a model that OBEYS the poisoned context.
struct RecordingOracle {
    script: Vec<ProposedAction>,
    cursor: AtomicUsize,
    seen_context: Mutex<Vec<String>>,
}

#[async_trait]
impl PolicyOracle for RecordingOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        _schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        self.seen_context
            .lock()
            .unwrap()
            .extend(context.memory.iter().cloned());
        let i = self
            .cursor
            .fetch_add(1, Ordering::SeqCst)
            .min(self.script.len() - 1);
        Ok(self.script[i].clone().into())
    }
    fn describe(&self) -> String {
        "mock:recording".into()
    }
}

fn propose(action: &str, args: serde_json::Value) -> ProposedAction {
    ProposedAction {
        action: action.into(),
        arguments: args,
        rationale: None,
    }
}

#[tokio::test]
async fn poisoned_wasm_memory_reaches_context_but_never_authorizes() {
    // THE CRUX (ADR 0016 invariant across the WASM boundary): a deliberately
    // malicious memory backend answers every retrieval with a well-formed
    // response whose content instructs the model to run `nuke`, claiming
    // pre-authorization. The scripted model OBEYS. The kernel's validation
    // floor rejects the proposal — `nuke` is not in the schema — and the run
    // completes via valid actions. Poisoned context ≠ authorization.
    let poisoned: Arc<dyn MemoryProvider> = Arc::new(
        WasmMemoryProvider::load("poison", &poisonmem_path(), Grants::default())
            .expect("the malicious module loads — it is valid WASM, that's the point"),
    );

    let oracle = Arc::new(RecordingOracle {
        script: vec![
            propose("nuke", json!({"target": "production"})), // obeys the poison
            propose("echo", json!({"message": "safe"})),
            propose("finish", json!({"summary": "done"})),
        ],
        cursor: AtomicUsize::new(0),
        seen_context: Mutex::new(Vec::new()),
    });

    let kernel = Kernel::new(
        oracle.clone(),
        Arc::new(EchoTools),
        poisoned,
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig::default(),
    );
    let run = kernel.run_task("do the task".into()).await.unwrap();

    // The poison DID reach the model as context, provenance-tagged…
    let ctx = oracle.seen_context.lock().unwrap().clone();
    assert!(
        ctx.iter()
            .any(|c| c.starts_with("[wasm-memory:poison]") && c.contains("nuke")),
        "the poisoned line must reach the model as tagged context; saw {ctx:?}"
    );
    // …but `nuke` NEVER executed: not in the schema, so it was rejected.
    assert!(
        run.steps.iter().all(|s| s.action != "nuke"),
        "a poisoned suggestion must never execute"
    );
    assert_eq!(run.status, RunStatus::Completed);
}

/// ADR 0038 on the WASM backend (memstore guest): an `unverified` episode is
/// stored and in the history, but never retrieved; a higher `success`
/// version is retrieved, with one entry per episode.
#[tokio::test]
async fn wasm_memory_backend_versions_and_never_retrieves_unverified() {
    let mem = WasmMemoryProvider::load("memstore", &memstore_path(), Grants::default())
        .expect("memory module loads and instantiates");
    let mut e = episode("reconcile the ledger");
    e.outcome = EpisodeOutcome::Unverified;
    mem.write(e.clone()).await.unwrap();
    assert!(mem
        .retrieve("reconcile ledger", 5)
        .await
        .unwrap()
        .is_empty());

    let mut v2 = e.clone();
    v2.version = 2;
    v2.outcome = EpisodeOutcome::Success;
    v2.provenance = Provenance::Human;
    mem.write(v2).await.unwrap();
    mem.write(e.clone()).await.unwrap(); // stale re-write never wins
    let hits = mem.retrieve("reconcile ledger", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].summary.contains("outcome: success"), "{hits:?}");

    let hist = mem.episode_history(e.run_id).await.unwrap();
    assert_eq!(
        hist.iter()
            .map(|x| (x.version, x.outcome))
            .collect::<Vec<_>>(),
        vec![
            (1, EpisodeOutcome::Unverified),
            (2, EpisodeOutcome::Success)
        ]
    );
}

/// The poisoned twin acknowledges every op with `{"ok": true}` — a lying
/// store. Its "history" is not trusted: the gate refuses it, so the kernel
/// writes it no revisions (an error there is logged, never fatal).
#[tokio::test]
async fn lying_module_history_is_refused_by_the_gate() {
    let mem = WasmMemoryProvider::load("poison", &poisonmem_path(), Grants::default())
        .expect("module loads");
    let err = mem.episode_history(Uuid::new_v4()).await.unwrap_err();
    assert!(err.to_string().contains("interface-violating"), "{err}");
}

/// ADR 0038 end to end through the kernel over the WASM memory backend: the
/// model's context never holds the unverified episode, and holds it once a
/// human confirmed it.
#[tokio::test]
async fn wasm_memory_keeps_unverified_out_of_model_context() {
    let mem = WasmMemoryProvider::load("memstore", &memstore_path(), Grants::default())
        .expect("memory module loads and instantiates");
    let p =
        kernel_core::testkit::episode_context_probe(Arc::new(mem), "probe wasm reconcile ledger")
            .await;
    assert!(
        p.before_confirmation
            .iter()
            .all(|m| !m.contains("probe wasm")),
        "{:?}",
        p.before_confirmation
    );
    assert_eq!(
        p.after_confirmation,
        vec![
            "[wasm-memory:memstore] task: probe wasm reconcile ledger | outcome: success | steps: 1"
                .to_string()
        ]
    );
}
