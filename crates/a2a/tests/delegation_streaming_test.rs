//! Streamed delegation progress (ADR 0027): an authorized delegation whose
//! remote agent advertises `capabilities.streaming` is dialed with
//! `SendStreamingMessage` and its `text/event-stream` reply is consumed frame
//! by frame. Intermediate updates surface as ADVISORY `AgentDelegationProgress`
//! events (live on the timeline) AND durable journal notes — never as validated
//! actions. The final result flows through the SAME terminal handling as a
//! polled dial, so completion, attribution, and the validation floor are
//! unchanged. A mid-stream failure falls back to polling the already-created
//! task (streaming is a progress enhancement, never a durability regression),
//! and `with_streaming(false)` forces the pure poll path even for a
//! streaming-capable card.
//!
//! Real `Kernel` (step mode) + a real mock A2A server (axum, loopback SSE).
//! Fixtures duplicated per test file by this repo's convention.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};

use a2a::{action_name, A2aActionSource, RemoteAgent};
use kernel_core::{
    ActionSchema, InMemoryRunStore, JournalEntry, Kernel, KernelConfig, PolicyContext, PolicyError,
    PolicyOracle, Proposal, ProposedAction, RunEvent, RunStatus, RunStore,
};
use memory::InMemoryProvider;

#[derive(Default)]
struct MockState {
    /// Streaming dials (`SendStreamingMessage`).
    stream_calls: AtomicUsize,
    /// Plain dials (`SendMessage`) — expected 0 while streaming, >0 only on
    /// the forced-poll path.
    send_calls: AtomicUsize,
    /// `GetTask` polls — the fallback path uses these.
    get_calls: AtomicUsize,
    /// When set, the SSE reply carries the task id + one progress frame but NO
    /// final frame, forcing the client to fall back to `GetTask`.
    drop_mid_stream: AtomicBool,
    /// When set, the SSE reply carries one valid task-id-bearing event, then a
    /// multi-MiB run of bytes with NO `\n\n` separator — a hostile/broken
    /// peer that never terminates an event. Exercises the reassembly-buffer
    /// cap (audit finding 9-a): the client must fall back to polling rather
    /// than growing `buf` without bound.
    flood_after_task_id: AtomicBool,
    /// When set, the SSE reply carries far more than
    /// `MAX_PROGRESS_NOTES_PER_STEP` well-formed WORKING progress frames
    /// before a final one — exercises the per-step progress-note cap (audit
    /// finding 9-b): the journal must not grow past the cap even though the
    /// remote sent more frames than that.
    flood_progress_frames: AtomicBool,
    endpoint: Mutex<String>,
}

async fn card(State(st): State<Arc<MockState>>) -> Json<Value> {
    let endpoint = st.endpoint.lock().unwrap().clone();
    Json(json!({
        "name": "Toy Agent",
        "description": "streams its progress, then answers",
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
        "version": "1.0.0",
        "capabilities": {"streaming": true},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": []
    }))
}

fn sse(frames: Vec<Value>) -> Response {
    let mut body = String::new();
    for f in frames {
        body.push_str(&format!("data: {f}\n\n"));
    }
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn rpc(State(st): State<Arc<MockState>>, Json(req): Json<Value>) -> Response {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});

    match method {
        "SendStreamingMessage" => {
            st.stream_calls.fetch_add(1, Ordering::SeqCst);
            if st.flood_after_task_id.load(Ordering::SeqCst) {
                return infinite_flood_body();
            }
            if st.flood_progress_frames.load(Ordering::SeqCst) {
                let n = 2 * usize::try_from(kernel_core::MAX_PROGRESS_NOTES_PER_STEP).unwrap();
                let mut frames = vec![
                    json!({"result": {"id": "task-cap", "status": {"state": "TASK_STATE_WORKING"}, "artifacts": []}}),
                ];
                for i in 0..n {
                    frames.push(json!({"result": {"taskId": "task-cap", "status": {"state": "TASK_STATE_WORKING"}, "artifact": {"parts": [{"text": format!("progress {i}")}]}}}));
                }
                frames.push(json!({"result": {"taskId": "task-cap", "final": true, "status": {"state": "TASK_STATE_COMPLETED"}, "artifacts": [{"parts": [{"text": "done after flood"}]}]}}));
                return sse(frames);
            }
            let frames = if st.drop_mid_stream.load(Ordering::SeqCst) {
                // Task created + one progress frame, then the body ENDS with no
                // final frame — the client must poll `GetTask` to finish.
                vec![
                    json!({"result": {"id": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifacts": []}}),
                    json!({"result": {"taskId": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifact": {"parts": [{"text": "warming up"}]}}}),
                ]
            } else {
                vec![
                    json!({"result": {"id": "task-s", "status": {"state": "TASK_STATE_SUBMITTED"}, "artifacts": []}}),
                    json!({"result": {"taskId": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifact": {"parts": [{"text": "thinking about it"}]}}}),
                    json!({"result": {"taskId": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifact": {"parts": [{"text": "almost there"}]}}}),
                    json!({"result": {"taskId": "task-s", "final": true, "status": {"state": "TASK_STATE_COMPLETED"}, "artifacts": [{"parts": [{"text": "the answer is 42"}]}]}}),
                ]
            };
            sse(frames)
        }
        "SendMessage" => {
            st.send_calls.fetch_add(1, Ordering::SeqCst);
            Json(reply(json!({
                "task": {"id": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifacts": []}
            })))
            .into_response()
        }
        "GetTask" => {
            st.get_calls.fetch_add(1, Ordering::SeqCst);
            Json(reply(json!({
                "id": "task-s",
                "status": {"state": "TASK_STATE_COMPLETED"},
                "artifacts": [{"artifactId": "a1", "parts": [{"text": "the answer is 42 (via poll)"}]}]
            })))
            .into_response()
        }
        other => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }))
        .into_response(),
    }
}

/// A genuinely INFINITE SSE body: one valid task-id-bearing event, then an
/// endless run of bytes with no `\n\n` separator, ever. Proves the
/// reassembly-buffer cap does the work (not incidental natural end-of-stream)
/// — without the cap, a client reading this would never see `Ok(None)` and
/// would only stop at the 120s poll deadline; the test's own tight outer
/// timeout is what makes this a real red-green proof.
fn infinite_flood_body() -> Response {
    let first = format!(
        "data: {}\n\n",
        json!({"result": {"id": "task-s", "status": {"state": "TASK_STATE_WORKING"}, "artifacts": []}})
    );
    let head = stream::once(async move { Ok::<_, std::io::Error>(first.into_bytes()) });
    let tail = stream::repeat_with(|| Ok::<_, std::io::Error>(vec![b'x'; 65_536]));
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(head.chain(tail)))
        .unwrap()
}

async fn spawn_server(state: Arc<MockState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    *state.endpoint.lock().unwrap() = format!("{base}/rpc");
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/rpc", post(rpc))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Proposes ONE fixed delegation fresh, then `finish` on every later decision.
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
                arguments: json!({"task": "solve it"}),
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

async fn source_for(base: &str, streaming: bool) -> (Arc<A2aActionSource>, String) {
    let source = Arc::new(
        A2aActionSource::new(vec![])
            .unwrap()
            .with_streaming(streaming),
    );
    source
        .register_runtime(&RemoteAgent {
            name: "toy".into(),
            card_url: base.to_string(),
            token: None,
        })
        .unwrap();
    (source, action_name("toy"))
}

fn kernel(oracle_action: &str, source: Arc<A2aActionSource>, store: Arc<dyn RunStore>) -> Kernel {
    Kernel::new(
        Arc::new(DelegateOnceOracle {
            action: oracle_action.to_string(),
        }),
        source,
        Arc::new(InMemoryProvider::new()),
        store,
        KernelConfig::default(),
    )
}

fn progress_notes(journal: &[JournalEntry]) -> Vec<Value> {
    journal
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Note { key, value, .. } if key.starts_with("a2a.progress.") => {
                Some(value.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streamed_delegation_completes_and_surfaces_progress() {
    let mock = Arc::new(MockState::default());
    let base = spawn_server(mock.clone()).await;
    let (source, action) = source_for(&base, true).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store.clone());
    let mut rx = k.subscribe();

    let run_id = k.create_run("streamy".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    // 1. It streamed exactly once, never plain-dialed, and completed with the
    //    remote's FINAL answer (the terminal handling is unchanged).
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        mock.stream_calls.load(Ordering::SeqCst),
        1,
        "dialed once via streaming"
    );
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        0,
        "streaming must not also plain-dial SendMessage"
    );
    let step = run
        .steps
        .iter()
        .find(|s| s.action == action)
        .expect("the delegate step is recorded");
    assert!(step.ok);
    assert!(
        step.result["output"]
            .as_str()
            .unwrap_or("")
            .contains("the answer is 42"),
        "final output is the remote's terminal artifact: {:?}",
        step.result
    );

    // 2. Intermediate progress reached the LIVE event stream, advisory-only,
    //    with monotonic ordinals starting at 0.
    let mut progress: Vec<(u64, String, String)> = Vec::new();
    while let Ok(env) = rx.try_recv() {
        if let RunEvent::AgentDelegationProgress {
            ordinal,
            state,
            note,
            ..
        } = env.event
        {
            assert_eq!(env.seq, 0, "progress rides seq 0, off the ordered stream");
            progress.push((ordinal, state, note));
        }
    }
    assert!(
        progress.len() >= 2,
        "at least two progress updates streamed live: {progress:?}"
    );
    assert!(
        progress
            .iter()
            .any(|(_, _, n)| n.contains("thinking about it")),
        "a partial-artifact progress line was surfaced: {progress:?}"
    );
    let ordinals: Vec<u64> = progress.iter().map(|(o, _, _)| *o).collect();
    assert_eq!(ordinals[0], 0, "ordinals start at 0");
    assert!(
        ordinals.windows(2).all(|w| w[1] == w[0] + 1),
        "ordinals are monotonic: {ordinals:?}"
    );

    // 3. Progress is DURABLE: journaled as side-notes, independent of the live
    //    broadcast.
    let journal = store.journal(run_id).await.unwrap();
    let notes = progress_notes(&journal);
    assert!(
        notes.len() >= 2,
        "progress journaled durably as notes: {notes:?}"
    );
    assert!(
        notes.iter().any(|v| v["note"]
            .as_str()
            .unwrap_or("")
            .contains("thinking about it")),
        "the journaled note carries the progress text: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .all(|v| v["task_id"] == "task-s" && v["agent"] == "toy"),
        "every progress note is attributed to the remote task + agent: {notes:?}"
    );
}

#[tokio::test]
async fn streamed_delegation_falls_back_to_polling_when_stream_drops() {
    let mock = Arc::new(MockState::default());
    mock.drop_mid_stream.store(true, Ordering::SeqCst);
    let base = spawn_server(mock.clone()).await;
    let (source, action) = source_for(&base, true).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store.clone());

    let run_id = k.create_run("streamy-drop".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    // The stream dialed once and dropped before a final frame; the client fell
    // back to polling the ALREADY-CREATED task (no re-dial) and completed.
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        mock.stream_calls.load(Ordering::SeqCst),
        1,
        "streamed dial happened once"
    );
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        0,
        "fallback polls the existing task — it never re-dials a fresh one"
    );
    assert!(
        mock.get_calls.load(Ordering::SeqCst) >= 1,
        "fell back to GetTask polling"
    );
    let step = run.steps.iter().find(|s| s.action == action).unwrap();
    assert!(
        step.result["output"]
            .as_str()
            .unwrap_or("")
            .contains("via poll"),
        "the completed result came from the poll fallback: {:?}",
        step.result
    );
    // The pre-drop progress frame was still surfaced durably.
    let journal = store.journal(run_id).await.unwrap();
    assert!(
        !progress_notes(&journal).is_empty(),
        "progress seen before the drop is still journaled"
    );
}

#[tokio::test]
async fn with_streaming_false_forces_the_poll_path_on_a_streaming_card() {
    let mock = Arc::new(MockState::default());
    let base = spawn_server(mock.clone()).await;
    // Card advertises streaming, but this source has it toggled OFF.
    let (source, action) = source_for(&base, false).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store.clone());

    let run_id = k.create_run("no-stream".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        mock.stream_calls.load(Ordering::SeqCst),
        0,
        "streaming off: SendStreamingMessage must never be sent"
    );
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        1,
        "it dialed via plain SendMessage instead"
    );
    assert!(
        mock.get_calls.load(Ordering::SeqCst) >= 1,
        "and polled GetTask"
    );
    // No progress notes on the pure poll path.
    let journal = store.journal(run_id).await.unwrap();
    assert!(
        progress_notes(&journal).is_empty(),
        "the poll path emits no streamed progress"
    );
}

/// Regression test for audit finding 9-a (docs/audit/AUDIT-2026-07-21.md):
/// `stream_to_terminal`'s SSE reassembly buffer had no size cap, so a
/// streaming peer that never sends a `\n\n` event separator could grow it
/// without bound (OOM), and a client reading it would never see the stream
/// end. `infinite_flood_body` is a genuinely never-ending, ever-separator-
/// less body — WITHOUT the cap, this would hang until the 120s poll
/// deadline; the tight 10s outer timeout below is what makes this a real
/// red-green proof of the cap firing, not an artifact of natural EOF.
#[tokio::test]
async fn streamed_delegation_caps_the_reassembly_buffer_and_falls_back_to_polling() {
    let mock = Arc::new(MockState::default());
    mock.flood_after_task_id.store(true, Ordering::SeqCst);
    let base = spawn_server(mock.clone()).await;
    let (source, action) = source_for(&base, true).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store.clone());

    let run_id = k.create_run("streamy-flood".into()).await.unwrap();
    let run = tokio::time::timeout(std::time::Duration::from_secs(10), k.execute_run(run_id))
        .await
        .expect(
            "the reassembly-buffer cap must trip well within 10s — an unbounded buffer would \
             hang until the 120s poll deadline instead",
        )
        .unwrap();

    // The flood tripped the reassembly-buffer cap; the client fell back to
    // polling the task id it already learned (and journaled) from the first
    // valid event — never re-dialing, never hanging on an unbounded buffer.
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        mock.stream_calls.load(Ordering::SeqCst),
        1,
        "streamed dial happened once"
    );
    assert_eq!(
        mock.send_calls.load(Ordering::SeqCst),
        0,
        "fallback polls the existing task — it never re-dials a fresh one"
    );
    assert!(
        mock.get_calls.load(Ordering::SeqCst) >= 1,
        "fell back to GetTask polling once the buffer cap tripped"
    );
    let step = run.steps.iter().find(|s| s.action == action).unwrap();
    assert!(
        step.result["output"]
            .as_str()
            .unwrap_or("")
            .contains("via poll"),
        "the completed result came from the poll fallback: {:?}",
        step.result
    );
}

/// Regression test for audit finding 9-b (docs/audit/AUDIT-2026-07-21.md):
/// every non-final SSE frame journals+broadcasts an advisory progress note
/// with no prior cap on how many times per step. A remote that sends far
/// more than `MAX_PROGRESS_NOTES_PER_STEP` well-formed progress frames must
/// still complete normally (the cap only drops PROGRESS notes, never the
/// terminal frame), and the journal must hold at most the cap's worth of
/// progress notes — not one per frame the remote chose to send.
#[tokio::test]
async fn streamed_delegation_caps_progress_notes_per_step() {
    let mock = Arc::new(MockState::default());
    mock.flood_progress_frames.store(true, Ordering::SeqCst);
    let base = spawn_server(mock.clone()).await;
    let (source, action) = source_for(&base, true).await;
    let store: Arc<dyn RunStore> = Arc::new(InMemoryRunStore::new());
    let k = kernel(&action, source, store.clone());

    let run_id = k.create_run("streamy-progress-flood".into()).await.unwrap();
    let run = k.execute_run(run_id).await.unwrap();

    // The remote sent 2x the cap in progress frames, THEN a final frame — the
    // final frame is never gated by the cap (record_progress is only called
    // for non-final frames), so completion is unaffected.
    assert_eq!(run.status, RunStatus::Completed);
    let step = run.steps.iter().find(|s| s.action == action).unwrap();
    assert!(
        step.result["output"]
            .as_str()
            .unwrap_or("")
            .contains("done after flood"),
        "the terminal frame's own output is never gated by the progress cap: {:?}",
        step.result
    );

    let journal = store.journal(run_id).await.unwrap();
    let notes = progress_notes(&journal);
    assert_eq!(
        notes.len(),
        usize::try_from(kernel_core::MAX_PROGRESS_NOTES_PER_STEP).unwrap(),
        "journal must hold at most the cap's worth of progress notes, not one per frame the \
         remote chose to send (it sent 2x the cap)"
    );
}
