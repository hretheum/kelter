//! WASM extension mechanism: in-process, capability-sandboxed tools run
//! through wasmtime (ADR 0011). The kernel's second extension mechanism
//! after subprocess MCP (ADR 0006/0008).
//!
//! **Interface decision (see docs/BUILD-EVIDENCE.md):** core WASM + a
//! minimal byte ABI, NOT the component model / WASI preview 2. The byte ABI
//! is a handful of exported functions over linear memory — stable across
//! wasmtime releases and trivial to keep in the trust path — whereas the
//! component model is richer but still moving (a fast-moving surface we
//! keep out of the kernel, exactly as the A2A client decision did in 4b).
//!
//! **Capability sandbox:** a core WASM module has NO ambient authority — it
//! can only do what host functions it imports let it do. The host defines a
//! capability's host function ONLY when the manifest grants it. A module
//! that imports a capability it was not granted cannot be instantiated
//! (undefined import) — the capability is *structurally* absent, not
//! blocked at runtime. A pure-compute module imports nothing and so cannot
//! touch the network, filesystem, clock or environment at all.

use std::sync::Arc;

pub mod workspace;

use async_trait::async_trait;
use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError, Latitude,
    ValidatedAction,
};
use wasmtime::{Config, Engine, Linker, Memory, Module, Store, TypedFunc};

/// The single string input argument a WASM tool receives.
pub const INPUT_ARG: &str = "input";
/// A generous fuel budget bounding one guest call (prevents runaway loops).
const CALL_FUEL: u64 = 200_000_000;

#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("cannot read wasm module '{0}': {1}")]
    Read(String, std::io::Error),
    #[error("wasm engine/compile error: {0}")]
    Engine(String),
    #[error("wasm module '{0}' is missing required export '{1}'")]
    MissingExport(String, &'static str),
}

/// Host-side capability grants for one WASM extension. Absent grants mean
/// the corresponding host import is never defined → structurally unreachable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Grants {
    pub network: bool,
    /// `kelter:fs.kelter_ws_write` — write text files inside the run
    /// workspace, host-validated (see [`workspace`]). Tools only; requires a
    /// workspace to be attached ([`WasmToolSource::with_workspace`]).
    pub workspace_write: bool,
}

/// Per-instance host state (kept minimal; carries nothing ambient).
struct HostState;

/// A WASM tool exposed to the kernel as an `ActionSource`.
pub struct WasmToolSource {
    engine: Engine,
    module: Module,
    grants: Grants,
    name: String,
    tool: String,
    description: String,
    /// Typed argument schema (flat object) declared by the manifest. `None`
    /// = the original single-string ABI (`{"input": "..."}`).
    parameters: Option<serde_json::Value>,
    /// Backing for the `workspace_write` grant.
    workspace: Option<Arc<workspace::WorkspaceWriter>>,
}

impl WasmToolSource {
    /// Load a WASM tool from a module file. `tool` is the action name it
    /// exposes; `grants` are the capabilities the manifest granted.
    pub fn load(
        name: impl Into<String>,
        module_path: &str,
        tool: impl Into<String>,
        description: impl Into<String>,
        grants: Grants,
    ) -> Result<Self, WasmError> {
        let name = name.into();
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| WasmError::Engine(e.to_string()))?;
        let bytes =
            std::fs::read(module_path).map_err(|e| WasmError::Read(module_path.to_string(), e))?;
        let module = Module::new(&engine, &bytes).map_err(|e| WasmError::Engine(e.to_string()))?;

        // Verify the required ABI exports exist up front.
        let has = |n: &str| module.exports().any(|e| e.name() == n);
        if !has("run") {
            return Err(WasmError::MissingExport(name, "run"));
        }
        if !has("alloc") {
            return Err(WasmError::MissingExport(name, "alloc"));
        }

        Ok(Self {
            engine,
            module,
            grants,
            name,
            tool: tool.into(),
            description: description.into(),
            parameters: None,
            workspace: None,
        })
    }

    /// Expose a typed, flat argument object instead of the single `input`
    /// string. The guest then receives the validated arguments object,
    /// serialized as JSON, as its input. The schema must already have passed
    /// the registry's flatness check (`mcp::extensions`).
    pub fn with_parameters(mut self, parameters: serde_json::Value) -> Self {
        self.parameters = Some(parameters);
        self
    }

    /// Attach the workspace directory backing the `workspace_write` grant.
    pub fn with_workspace(mut self, root: impl AsRef<std::path::Path>) -> Result<Self, WasmError> {
        let w = workspace::WorkspaceWriter::new(root.as_ref()).map_err(|e| {
            WasmError::Engine(format!(
                "workspace '{}' unusable: {e}",
                root.as_ref().display()
            ))
        })?;
        self.workspace = Some(Arc::new(w));
        Ok(self)
    }
}

/// Build the capability-scoped linker: a capability's host function is
/// defined ONLY if granted. This is the sandbox — an ungranted capability
/// is simply absent, so a module importing it cannot be instantiated.
/// Generic over the host state so the tool host (`HostState`) and the
/// memory host (`MemHostState`, which carries store limits) share ONE gate.
fn linker_for<T: 'static>(
    engine: &Engine,
    grants: Grants,
    workspace: Option<Arc<workspace::WorkspaceWriter>>,
) -> Result<Linker<T>, WasmError> {
    let mut linker = Linker::new(engine);
    if grants.workspace_write {
        // Granted but nothing to write into is a wiring bug — refuse loudly
        // rather than define an import that can only fail.
        let ws = workspace.ok_or_else(|| {
            WasmError::Engine("workspace_write granted but no workspace attached".into())
        })?;
        linker
            .func_wrap(
                workspace::FS_IMPORT_MODULE,
                workspace::FS_WRITE_FN,
                move |mut caller: wasmtime::Caller<'_, T>,
                      path_ptr: i32,
                      path_len: i32,
                      data_ptr: i32,
                      data_len: i32,
                      flags: i32|
                      -> i32 {
                    let Some(mem) = caller
                        .get_export("memory")
                        .and_then(wasmtime::Extern::into_memory)
                    else {
                        return workspace::code::BAD_ARGS;
                    };
                    let data = mem.data(&caller);
                    // Guest-supplied offsets are untrusted: bounds-check as
                    // unsigned, never index with a raw negative.
                    let slice = |ptr: i32, len: i32| {
                        let (p, l) = (ptr as u32 as usize, len as u32 as usize);
                        data.get(p..p.checked_add(l)?)
                    };
                    let (Some(path), Some(content)) =
                        (slice(path_ptr, path_len), slice(data_ptr, data_len))
                    else {
                        return workspace::code::BAD_ARGS;
                    };
                    let Ok(path) = std::str::from_utf8(path) else {
                        return workspace::code::INVALID_PATH;
                    };
                    let overwrite = flags & workspace::FLAG_OVERWRITE != 0;
                    match ws.write(path, content, overwrite) {
                        Ok(workspace::WriteOutcome::Created) => workspace::code::CREATED,
                        Ok(workspace::WriteOutcome::Overwritten) => workspace::code::OVERWRITTEN,
                        Err(e) => {
                            tracing::info!(path, "wasm workspace write refused: {e}");
                            e.code()
                        }
                    }
                },
            )
            .map_err(|e| WasmError::Engine(e.to_string()))?;
    }
    if grants.network {
        // A real network capability would live here. For this slice it is a
        // stub — the point is the *gate*: it exists only when granted.
        linker
            .func_wrap(
                "kelter:net",
                "kelter_net_fetch",
                |_caller: wasmtime::Caller<'_, T>, _ptr: i32, _len: i32| -> i32 { -1 },
            )
            .map_err(|e| WasmError::Engine(e.to_string()))?;
    }
    Ok(linker)
}

/// Run the guest on `input`, returning the parsed JSON result. A fresh
/// Store/instance per call keeps the guest stateless and deterministic.
fn run_guest(
    engine: &Engine,
    module: &Module,
    grants: Grants,
    workspace: Option<Arc<workspace::WorkspaceWriter>>,
    name: &str,
    input: &str,
) -> Result<serde_json::Value, ExecutionError> {
    let mut store = Store::new(engine, HostState);
    store
        .set_fuel(CALL_FUEL)
        .map_err(|e| ExecutionError::Tool(format!("set_fuel: {e}")))?;

    let linker = linker_for(engine, grants, workspace)
        .map_err(|e| ExecutionError::Substrate(e.to_string()))?;

    // Instantiation is where the sandbox bites: a module importing an
    // ungranted capability fails here with an "unknown import" error.
    let instance = linker.instantiate(&mut store, module).map_err(|e| {
        ExecutionError::Tool(format!(
            "wasm '{name}' could not be instantiated (a required capability may not be \
                 granted): {e}"
        ))
    })?;

    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| ExecutionError::Tool(format!("wasm '{name}' exports no memory")))?;
    let alloc: TypedFunc<i32, i32> = instance
        .get_typed_func(&mut store, "alloc")
        .map_err(|e| ExecutionError::Tool(format!("bad `alloc`: {e}")))?;
    let run: TypedFunc<(i32, i32), i64> = instance
        .get_typed_func(&mut store, "run")
        .map_err(|e| ExecutionError::Tool(format!("bad `run`: {e}")))?;

    // Write the input into guest memory.
    let bytes = input.as_bytes();
    let len = bytes.len() as i32;
    let ptr = alloc
        .call(&mut store, len)
        .map_err(|e| ExecutionError::Tool(format!("alloc trap: {e}")))?;
    memory
        .write(&mut store, ptr as usize, bytes)
        .map_err(|e| ExecutionError::Tool(format!("memory write: {e}")))?;

    // Call run → packed (out_ptr<<32 | out_len).
    let packed = run
        .call(&mut store, (ptr, len))
        .map_err(|e| ExecutionError::Tool(format!("run trap: {e}")))?;
    let out_ptr = (packed >> 32) as usize;
    let out_len = (packed & 0xffff_ffff) as usize;

    let data = memory.data(&store);
    let out = data
        .get(out_ptr..out_ptr + out_len)
        .ok_or_else(|| ExecutionError::Tool("wasm result out of bounds".into()))?;
    let text = std::str::from_utf8(out)
        .map_err(|e| ExecutionError::Tool(format!("wasm result not utf-8: {e}")))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| ExecutionError::Tool(format!("wasm result not valid JSON: {e}: {text}")))?;
    guest_failure(&value).map_or(Ok(value), Err)
}

/// Guest-reported failure (ABI convention, additive): a top-level
/// `{"ok": false, "error": "..."}` result is a FAILED step, not a successful
/// one carrying an error payload. Without this, a refused write would be
/// journaled as `ok` — and count toward the success predicate (ADR 0033).
/// Results without `"ok": false` (every pre-existing module) are unchanged.
fn guest_failure(value: &serde_json::Value) -> Option<ExecutionError> {
    if value.get("ok") != Some(&serde_json::Value::Bool(false)) {
        return None;
    }
    let msg = value
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("wasm tool reported failure");
    Some(ExecutionError::Tool(msg.to_string()))
}

#[async_trait]
impl ActionSource for WasmToolSource {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        // Deliberately shallow: one string input (a nested/object grammar is
        // pathologically slow for a small model, per Phase 4c) — or a
        // manifest-declared FLAT object of scalars, held to the same rule.
        let parameters = self.parameters.clone().unwrap_or_else(|| {
            serde_json::json!({
                "type": "object",
                "properties": { INPUT_ARG: { "type": "string" } },
                "required": [INPUT_ARG],
                "additionalProperties": false
            })
        });
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: self.tool.clone(),
                description: self.description.clone(),
                parameters,
            }],
        })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        _ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        if action.action() != self.tool {
            return Err(ExecutionError::Tool(format!(
                "wasm source got unknown action '{}'",
                action.action()
            )));
        }
        // Typed tools get the whole validated arguments object as JSON; the
        // original ABI gets the bare `input` string, exactly as before.
        let input = if self.parameters.is_some() {
            action.arguments().to_string()
        } else {
            action.arguments()[INPUT_ARG]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };

        // wasmtime is synchronous; run the guest on a blocking thread so we
        // never stall the async runtime. Engine/Module/Grants are cheap
        // (Arc-backed) to move into the closure.
        let engine = self.engine.clone();
        let module = self.module.clone();
        let grants = self.grants;
        let workspace = self.workspace.clone();
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || {
            run_guest(&engine, &module, grants, workspace, &name, &input)
        })
        .await
        .map_err(|e| ExecutionError::Tool(format!("wasm task join: {e}")))?
    }
}

pub fn arc(source: WasmToolSource) -> Arc<dyn ActionSource> {
    Arc::new(source)
}

// ---- WASM memory backend (ADR 0024) ---------------------------------------
//
// The memory capability promoted from a compiled-in trait (ADR 0005) to a
// WASM-sandboxed, manifest-declared backend, over the SAME byte ABI as tools
// (`alloc`/`run`/`memory`) — the op is multiplexed in a JSON envelope whose
// op names mirror the reserved MCP memory tools (ADR 0008):
//
//   {"op":"memory_write",    "episode": {...}}          -> {"ok": true}
//   {"op":"memory_retrieve", "task": "...", "limit": N} -> {"results": [...]}
//   {"op":"memory_upsert",   "item": {...}}             -> {"ok": true}
//
// Two deliberate differences from the tool host:
//
// - **Persistent instance.** A memory backend must remember across calls, so
//   ONE Store/instance lives for the provider's lifetime (guest state = its
//   linear memory), serialized behind a mutex. Fuel is refilled per call and
//   linear-memory growth is capped by a store limiter — a runaway or hoarding
//   guest hits a bound, never the host.
// - **Eager instantiation.** The sandbox bites at instantiation (an ungranted
//   import is structurally absent), so `load` instantiates immediately: a
//   memory module demanding an ungranted capability is refused at startup,
//   loudly, not on first use.
//
// GOVERNANCE INVARIANT (ADR 0016, unchanged by construction): everything this
// provider returns enters the kernel ONLY as retrieved context
// (`PolicyContext.memory`). A malicious module can poison that context — it
// can never authorize an action; every proposal is still schema-validated by
// the kernel. Responses are additionally gated by the canonical interface
// schema (`memory::retrieve_response_schema`), the same gate the subprocess
// transport uses, and summaries are provenance-tagged with the module name.

use memory::{Episode, GraphUpsert, MemoryError, MemoryProvider, RankedContext};
use wasmtime::{StoreLimits, StoreLimitsBuilder};

/// Cap on the guest's linear memory (64 MiB) — the demo-scale bound that
/// keeps a hoarding/leaky memory module from growing without limit.
const MEMORY_GUEST_MAX_BYTES: usize = 64 << 20;

/// Host state for the persistent memory instance: nothing ambient, plus the
/// resource limiter that enforces the linear-memory cap.
struct MemHostState {
    limits: StoreLimits,
}

/// The persistent guest: one Store + typed ABI handles, living as long as
/// the provider. All calls are serialized through the owning mutex.
struct MemGuest {
    store: Store<MemHostState>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    run: TypedFunc<(i32, i32), i64>,
}

/// A WASM module serving the `memory` capability as a `MemoryProvider`.
pub struct WasmMemoryProvider {
    name: String,
    guest: Arc<std::sync::Mutex<MemGuest>>,
}

impl WasmMemoryProvider {
    /// Load AND instantiate a WASM memory backend. Instantiation is where
    /// the capability sandbox bites, so a module importing an ungranted
    /// capability is refused here — at registration — not at first call.
    pub fn load(
        name: impl Into<String>,
        module_path: &str,
        grants: Grants,
    ) -> Result<Self, WasmError> {
        let name = name.into();
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| WasmError::Engine(e.to_string()))?;
        let bytes =
            std::fs::read(module_path).map_err(|e| WasmError::Read(module_path.to_string(), e))?;
        let module = Module::new(&engine, &bytes).map_err(|e| WasmError::Engine(e.to_string()))?;

        let has = |n: &str| module.exports().any(|e| e.name() == n);
        if !has("run") {
            return Err(WasmError::MissingExport(name, "run"));
        }
        if !has("alloc") {
            return Err(WasmError::MissingExport(name, "alloc"));
        }

        let mut store = Store::new(
            &engine,
            MemHostState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(MEMORY_GUEST_MAX_BYTES)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(CALL_FUEL)
            .map_err(|e| WasmError::Engine(e.to_string()))?;

        // Memory backends get no filesystem: `workspace_write` is a tools
        // capability, so it is never wired here (a module importing it fails
        // to instantiate, as any ungranted import does).
        let grants = Grants {
            workspace_write: false,
            ..grants
        };
        let linker = linker_for::<MemHostState>(&engine, grants, None)?;
        let instance = linker.instantiate(&mut store, &module).map_err(|e| {
            WasmError::Engine(format!(
                "wasm memory backend '{name}' could not be instantiated (a required \
                 capability may not be granted): {e}"
            ))
        })?;

        let memory: Memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(WasmError::MissingExport(name.clone(), "memory"))?;
        let alloc: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, "alloc")
            .map_err(|e| WasmError::Engine(format!("bad `alloc`: {e}")))?;
        let run: TypedFunc<(i32, i32), i64> = instance
            .get_typed_func(&mut store, "run")
            .map_err(|e| WasmError::Engine(format!("bad `run`: {e}")))?;

        Ok(Self {
            name,
            guest: Arc::new(std::sync::Mutex::new(MemGuest {
                store,
                memory,
                alloc,
                run,
            })),
        })
    }

    /// One envelope round-trip on the blocking pool (wasmtime is sync).
    async fn call(&self, payload: serde_json::Value) -> Result<serde_json::Value, MemoryError> {
        let guest = self.guest.clone();
        let name = self.name.clone();
        let input = payload.to_string();
        tokio::task::spawn_blocking(move || {
            let mut g = guest.lock().expect("wasm memory guest mutex");
            call_mem_guest(&mut g, &name, &input)
        })
        .await
        .map_err(|e| MemoryError::Backend(format!("wasm memory task join: {e}")))?
    }
}

/// Drive one guest call: refuel, write the envelope in, `run`, read JSON out.
fn call_mem_guest(
    g: &mut MemGuest,
    name: &str,
    input: &str,
) -> Result<serde_json::Value, MemoryError> {
    let backend = |m: String| MemoryError::Backend(m);
    g.store
        .set_fuel(CALL_FUEL)
        .map_err(|e| backend(format!("set_fuel: {e}")))?;

    let bytes = input.as_bytes();
    let len = bytes.len() as i32;
    let ptr = g
        .alloc
        .call(&mut g.store, len)
        .map_err(|e| backend(format!("wasm memory '{name}' alloc trap: {e}")))?;
    g.memory
        .write(&mut g.store, ptr as usize, bytes)
        .map_err(|e| backend(format!("wasm memory '{name}' write: {e}")))?;

    let packed = g
        .run
        .call(&mut g.store, (ptr, len))
        .map_err(|e| backend(format!("wasm memory '{name}' run trap: {e}")))?;
    let out_ptr = (packed >> 32) as usize;
    let out_len = (packed & 0xffff_ffff) as usize;

    let data = g.memory.data(&g.store);
    let out = data
        .get(out_ptr..out_ptr + out_len)
        .ok_or_else(|| backend(format!("wasm memory '{name}' result out of bounds")))?;
    let text = std::str::from_utf8(out)
        .map_err(|e| backend(format!("wasm memory '{name}' result not utf-8: {e}")))?;
    serde_json::from_str(text).map_err(|e| {
        backend(format!(
            "wasm memory '{name}' result not valid JSON: {e}: {text}"
        ))
    })
}

#[async_trait]
impl MemoryProvider for WasmMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        let episode =
            serde_json::to_value(&episode).map_err(|e| MemoryError::Backend(e.to_string()))?;
        let resp = self
            .call(serde_json::json!({ "op": "memory_write", "episode": episode }))
            .await?;
        if resp["ok"] != serde_json::json!(true) {
            return Err(MemoryError::Backend(format!(
                "wasm memory '{}' write not acknowledged: {resp}",
                self.name
            )));
        }
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError> {
        let resp = self
            .call(serde_json::json!({ "op": "memory_retrieve", "task": task, "limit": limit }))
            .await?;

        // The kernel-side gate (ADR 0008): the SAME canonical schema the
        // subprocess transport enforces. A malformed response is an error —
        // degraded context — never trusted into the policy context.
        let violations: Vec<String> = memory::retrieve_response_schema()
            .iter_errors(&resp)
            .map(|e| e.to_string())
            .collect();
        if !violations.is_empty() {
            return Err(MemoryError::Backend(format!(
                "wasm memory '{}' returned an interface-violating retrieve response: {}",
                self.name,
                violations.join("; ")
            )));
        }

        let mut results: Vec<RankedContext> = serde_json::from_value(resp["results"].clone())
            .map_err(|e| {
                MemoryError::Backend(format!("retrieve results failed to deserialize: {e}"))
            })?;
        // Defensive bound: the guest cannot flood the context past what the
        // kernel asked for.
        results.truncate(limit);
        // Provenance tag (ADR 0016 style, like `[learned]`): context lines
        // from a WASM backend carry their untrusted source visibly.
        for r in &mut results {
            r.summary = format!("[wasm-memory:{}] {}", self.name, r.summary);
        }
        Ok(results)
    }

    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError> {
        let item = serde_json::to_value(&item).map_err(|e| MemoryError::Backend(e.to_string()))?;
        let resp = self
            .call(serde_json::json!({ "op": "memory_upsert", "item": item }))
            .await?;
        if resp["ok"] != serde_json::json!(true) {
            return Err(MemoryError::Backend(format!(
                "wasm memory '{}' upsert not acknowledged: {resp}",
                self.name
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> String {
        env!("CARGO_MANIFEST_DIR")
            .trim_end_matches("/crates/wasmhost")
            .to_string()
    }
    fn wordcount_path() -> String {
        format!(
            "{}/examples/extensions/wordcount-wasm/wordcount_wasm.wasm",
            repo_root()
        )
    }
    fn netpeek_path() -> String {
        format!(
            "{}/examples/extensions/wordcount-wasm/netpeek_wasm.wasm",
            repo_root()
        )
    }

    #[test]
    fn guest_ok_false_is_a_failure_everything_else_is_not() {
        let err = guest_failure(&serde_json::json!({"ok": false, "error": "nope"})).unwrap();
        assert!(err.to_string().contains("nope"));
        assert!(guest_failure(&serde_json::json!({"ok": false})).is_some());
        // Pre-existing result shapes are untouched.
        assert!(guest_failure(&serde_json::json!({"ok": true})).is_none());
        assert!(guest_failure(&serde_json::json!({"words": 3})).is_none());
        assert!(guest_failure(&serde_json::json!({"ok": "false"})).is_none());
    }

    #[tokio::test]
    async fn pure_compute_wasm_tool_runs_in_process() {
        let src = WasmToolSource::load(
            "wordcount",
            &wordcount_path(),
            "word_count",
            "count words",
            Grants::default(),
        )
        .expect("module loads");

        let schema = src.action_schema(Latitude::Standard).await.unwrap();
        let action = schema
            .validate(kernel_core::ProposedAction {
                action: "word_count".into(),
                arguments: serde_json::json!({ "input": "one two three\nfour" }),
                rationale: None,
            })
            .unwrap();
        let ctx = ExecutionContext::new(
            uuid::Uuid::new_v4(),
            0,
            Arc::new(kernel_core::InMemoryRunStore::new()),
            Default::default(),
        );
        let result = src.execute(&action, &ctx).await.unwrap();
        assert_eq!(result["words"], 4);
        assert_eq!(result["lines"], 2);
    }

    #[tokio::test]
    async fn ungranted_network_capability_is_structurally_blocked() {
        // netpeek imports kelter:net::kelter_net_fetch. With NO network
        // grant, the host doesn't define it → instantiation fails.
        let src = WasmToolSource::load(
            "netpeek",
            &netpeek_path(),
            "net_peek",
            "tries the network",
            Grants {
                network: false,
                ..Grants::default()
            },
        )
        .expect("module loads (compilation is fine; instantiation is gated)");
        let schema = src.action_schema(Latitude::Standard).await.unwrap();
        let action = schema
            .validate(kernel_core::ProposedAction {
                action: "net_peek".into(),
                arguments: serde_json::json!({ "input": "x" }),
                rationale: None,
            })
            .unwrap();
        let ctx = ExecutionContext::new(
            uuid::Uuid::new_v4(),
            0,
            Arc::new(kernel_core::InMemoryRunStore::new()),
            Default::default(),
        );
        let err = src.execute(&action, &ctx).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("could not be instantiated") || msg.contains("import"),
            "expected a structural capability failure, got: {msg}"
        );
    }

    #[tokio::test]
    async fn granted_network_capability_allows_instantiation() {
        // With the grant, the import is defined → the module instantiates
        // and runs (the stub returns -1). Proves the gate is the ONLY thing
        // standing between the module and the capability.
        let src = WasmToolSource::load(
            "netpeek",
            &netpeek_path(),
            "net_peek",
            "tries the network",
            Grants {
                network: true,
                ..Grants::default()
            },
        )
        .unwrap();
        let schema = src.action_schema(Latitude::Standard).await.unwrap();
        let action = schema
            .validate(kernel_core::ProposedAction {
                action: "net_peek".into(),
                arguments: serde_json::json!({ "input": "x" }),
                rationale: None,
            })
            .unwrap();
        let ctx = ExecutionContext::new(
            uuid::Uuid::new_v4(),
            0,
            Arc::new(kernel_core::InMemoryRunStore::new()),
            Default::default(),
        );
        let result = src.execute(&action, &ctx).await.unwrap();
        assert_eq!(result["fetched"], true);
    }
}
