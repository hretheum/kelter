//! A Kelter WASM **memory backend** (ADR 0024): the memory capability served
//! by a sandboxed module instead of a compiled-in provider or a subprocess.
//!
//! Same byte ABI as WASM tools (`alloc`/`run`/`memory`), but the instance is
//! PERSISTENT — the host keeps one Store alive, so `static` state here
//! survives across calls. The op is multiplexed in a JSON envelope mirroring
//! the reserved MCP memory tools (ADR 0008):
//!
//!   {"op":"memory_write",    "episode": {...}}          -> {"ok": true}
//!   {"op":"memory_retrieve", "task": "...", "limit": N} -> {"results": [...]}
//!   {"op":"memory_upsert",   "item": {...}}             -> {"ok": true}
//!
//! This module imports NOTHING: no network, no disk, no clock. Its "storage"
//! is its own linear memory — as durable as the instance, which is exactly
//! the honest posture for a sandboxed demo backend.
//!
//! Buffer discipline: unlike the one-shot tool guests (which leak and are
//! torn down per call), a persistent guest must not leak per call — input
//! and output live in two reusable static buffers.

use std::sync::Mutex;

use serde_json::{json, Value};

struct Stored {
    id: String,
    task: String,
    summary: String,
}

static EPISODES: Mutex<Vec<Stored>> = Mutex::new(Vec::new());
static ENTITIES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Hand the host a reusable input buffer of `len` bytes (no per-call leak —
/// the instance is persistent). Calls are serialized host-side.
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut buf = INPUT.lock().unwrap();
    buf.clear();
    buf.resize(len.max(0) as usize, 0);
    buf.as_ptr() as i32
}

#[no_mangle]
pub extern "C" fn run(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let response = match serde_json::from_slice::<Value>(input) {
        Ok(envelope) => dispatch(&envelope),
        Err(e) => json!({ "ok": false, "error": format!("bad envelope: {e}") }),
    };

    let mut out = OUTPUT.lock().unwrap();
    *out = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    // The pointer stays valid after the guard drops: the buffer is only
    // mutated by the NEXT call, and the host reads before issuing it.
    ((out.as_ptr() as i64) << 32) | (out.len() as i64)
}

fn dispatch(envelope: &Value) -> Value {
    match envelope["op"].as_str() {
        Some("memory_write") => {
            let ep = &envelope["episode"];
            let task = ep["task"].as_str().unwrap_or("").to_string();
            let steps = ep["trace"].as_array().map(|t| t.len()).unwrap_or(0);
            let outcome = ep["outcome"].as_str().unwrap_or("unknown").to_string();
            EPISODES.lock().unwrap().push(Stored {
                id: ep["id"].as_str().unwrap_or("").to_string(),
                summary: format!("task: {task} | outcome: {outcome} | steps: {steps}"),
                task,
            });
            json!({ "ok": true })
        }
        Some("memory_retrieve") => {
            let task = envelope["task"].as_str().unwrap_or("");
            let limit = envelope["limit"].as_u64().unwrap_or(3) as usize;
            let query: Vec<String> = tokens(task);
            let store = EPISODES.lock().unwrap();
            let mut scored: Vec<(f64, &Stored)> = store
                .iter()
                .map(|s| (overlap(&query, &tokens(&s.task)), s))
                .filter(|(score, _)| *score > 0.0)
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let results: Vec<Value> = scored
                .into_iter()
                .take(limit)
                .map(|(score, s)| {
                    json!({ "episode_id": s.id, "summary": s.summary, "score": score })
                })
                .collect();
            json!({ "results": results })
        }
        Some("memory_upsert") => {
            let item = &envelope["item"];
            ENTITIES
                .lock()
                .unwrap()
                .push((item["kind"].as_str().unwrap_or("?").into(), item.to_string()));
            json!({ "ok": true })
        }
        other => json!({ "ok": false, "error": format!("unknown op: {other:?}") }),
    }
}

fn tokens(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() > 2)
        .collect()
}

/// Token-overlap relevance in [0, 1]: |query ∩ doc| / |query|.
fn overlap(query: &[String], doc: &[String]) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let hits = query.iter().filter(|q| doc.contains(q)).count();
    hits as f64 / query.len() as f64
}
