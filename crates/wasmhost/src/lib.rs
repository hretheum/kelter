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
        })
    }
}

/// Build the capability-scoped linker: a capability's host function is
/// defined ONLY if granted. This is the sandbox — an ungranted capability
/// is simply absent, so a module importing it cannot be instantiated.
fn linker_for(engine: &Engine, grants: Grants) -> Result<Linker<HostState>, WasmError> {
    let mut linker = Linker::new(engine);
    if grants.network {
        // A real network capability would live here. For this slice it is a
        // stub — the point is the *gate*: it exists only when granted.
        linker
            .func_wrap(
                "kelter:net",
                "kelter_net_fetch",
                |_caller: wasmtime::Caller<'_, HostState>, _ptr: i32, _len: i32| -> i32 { -1 },
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
    name: &str,
    input: &str,
) -> Result<serde_json::Value, ExecutionError> {
    let mut store = Store::new(engine, HostState);
    store
        .set_fuel(CALL_FUEL)
        .map_err(|e| ExecutionError::Tool(format!("set_fuel: {e}")))?;

    let linker =
        linker_for(engine, grants).map_err(|e| ExecutionError::Substrate(e.to_string()))?;

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
    serde_json::from_str(text)
        .map_err(|e| ExecutionError::Tool(format!("wasm result not valid JSON: {e}: {text}")))
}

#[async_trait]
impl ActionSource for WasmToolSource {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        // Deliberately shallow: one string input (a nested/object grammar is
        // pathologically slow for a small model, per Phase 4c).
        Ok(ActionSchema {
            actions: vec![ActionSpec {
                name: self.tool.clone(),
                description: self.description.clone(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { INPUT_ARG: { "type": "string" } },
                    "required": [INPUT_ARG],
                    "additionalProperties": false
                }),
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
        let input = action.arguments()[INPUT_ARG]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // wasmtime is synchronous; run the guest on a blocking thread so we
        // never stall the async runtime. Engine/Module/Grants are cheap
        // (Arc-backed) to move into the closure.
        let engine = self.engine.clone();
        let module = self.module.clone();
        let grants = self.grants;
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || run_guest(&engine, &module, grants, &name, &input))
            .await
            .map_err(|e| ExecutionError::Tool(format!("wasm task join: {e}")))?
    }
}

pub fn arc(source: WasmToolSource) -> Arc<dyn ActionSource> {
    Arc::new(source)
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
            Grants { network: false },
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
            Grants { network: true },
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
