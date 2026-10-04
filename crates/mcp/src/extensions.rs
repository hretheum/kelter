//! The capability registry — the generalized extension model (ADR 0006 →
//! ADR 0008). An extension is a directory with a `kelter-extension.toml`
//! manifest declaring WHAT capability it provides (`tools`, `memory`; the
//! `agent` and `model-provider` axes are reserved and refuse to load until
//! implemented), which permissions it requires, and how to start it. Every
//! extension runs as an ordinary MCP server; the registry only decides
//! which kernel seam it plugs into:
//!
//! - `tools`  → the action space (`ActionSource`), gated by kernel-side
//!   action validation and the run journal (Phase 2).
//! - `memory` → the `MemoryProvider` seam via reserved MCP tools
//!   (`memory_write` / `memory_retrieve` / `memory_upsert`); responses are
//!   schema-validated by the kernel-side client before use.
//!
//! Governance is non-negotiable either way: extensions are untrusted by
//! construction, and a manifest that declares anything this runtime cannot
//! scope (unknown capability, unknown permission key) is refused loudly
//! instead of being silently ignored. Since ADR 0035 "refused" means the
//! extension is SKIPPED — logged at ERROR and reported via the API — rather
//! than aborting the whole daemon start: one bad directory must not disable
//! the agent. The refused extension still never runs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::ServerSpec;

pub const MANIFEST_NAME: &str = "kelter-extension.toml";

/// Capability axes the registry understands. `Agent` and `ModelProvider`
/// parse (forward-compatible manifests) but refuse to register until the
/// runtime implements them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Tools,
    Memory,
    Agent,
    ModelProvider,
}

impl Capability {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "tools" => Some(Self::Tools),
            "memory" => Some(Self::Memory),
            "agent" => Some(Self::Agent),
            "model-provider" => Some(Self::ModelProvider),
            _ => None,
        }
    }
}

/// Permissions an extension may request. Unknown keys are a manifest error
/// (`deny_unknown_fields`) — the registry never silently grants what it
/// cannot scope. Enforcement in this slice: `workspace_write` gates whether
/// the runtime passes `KELTER_WORKSPACE` to the server; `network` is
/// recorded and logged (transport-level enforcement is a later slice —
/// documented, not pretended).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    pub network: bool,
    pub workspace_write: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub extension: ExtensionSection,
    /// Subprocess extensions (tools, memory): how to start the MCP server.
    #[serde(default)]
    pub server: Option<ServerSection>,
    /// Remote-agent extensions: where the agent lives. The kernel dials it
    /// as an A2A client — nothing is spawned.
    #[serde(default)]
    pub agent: Option<AgentSection>,
    /// WASM extensions: the in-process, capability-sandboxed module.
    #[serde(default)]
    pub wasm: Option<WasmSection>,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmSection {
    /// Path to the `.wasm` module (relative to the extension directory).
    pub module: String,
    /// The action name the module exposes. Required for the `tools`
    /// capability; refused for `memory` (a memory module serves the fixed
    /// reserved-op protocol, not a named action).
    #[serde(default)]
    pub tool: Option<String>,
    /// Optional typed arguments for a `tools` module: a JSON Schema file
    /// (relative to the extension directory) describing a FLAT object of
    /// scalar properties. Absent = the original single-string ABI
    /// (`{"input": "..."}`). Refused for `memory`.
    #[serde(default)]
    pub parameters: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    /// The A2A Agent Card URL (…/.well-known/agent-card.json). Its host
    /// becomes the ONLY host the kernel will dial for this agent
    /// (SSRF allowlist).
    pub card_url: String,
    /// Optional name of an environment variable holding a bearer token for a
    /// remote agent whose endpoint requires auth (e.g. another Kelter's
    /// inbound A2A endpoint, ADR 0013). The token itself is NEVER in the
    /// manifest — only the env var name; the daemon resolves it at startup.
    #[serde(default)]
    pub token_env: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSection {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Exactly one capability per manifest.
    pub provides: Vec<String>,
    /// How the extension runs: `"subprocess"` (default, MCP stdio / remote
    /// A2A) or `"wasm"` (in-process, capability-sandboxed via wasmtime).
    #[serde(default = "default_mechanism")]
    pub mechanism: String,
    /// The declared trust mode (ADR 0012). `"wasm-sandboxed"` (structural
    /// containment) or `"subprocess-unsandboxed"` (an explicit, operator-
    /// gated decision to run untrusted code with OS-level egress — NOT a
    /// sandbox). Absent = no declaration; a subprocess tool/memory extension
    /// then fails to load (sandbox-by-default).
    #[serde(default)]
    pub trust: Option<String>,
}

fn default_mechanism() -> String {
    "subprocess".into()
}

/// The trust posture under which an extension runs (ADR 0012).
///
/// Honest labelling is a hard rule: `SubprocessUnsandboxed` is NOT a sandbox
/// and must never be described as one — it is an operator-trusted subprocess
/// that still has OS-level egress (ADR 0009). Only `WasmSandboxed` is
/// structural containment (ADR 0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    /// In-process WASM: the code cannot reach a capability it was not
    /// granted (structural containment).
    WasmSandboxed,
    /// A local subprocess the operator explicitly trusted. Governed and
    /// attributable, NOT contained.
    SubprocessUnsandboxed,
    /// A remote A2A agent — a separate axis, bounded by origin-pin/SSRF
    /// egress (ADR 0009), not the subprocess-trust gate.
    RemoteAgent,
}

impl TrustMode {
    /// The label recorded in the run journal (honest, never "sandbox" for
    /// a subprocess).
    pub fn label(self) -> &'static str {
        match self {
            TrustMode::WasmSandboxed => "wasm-sandboxed",
            TrustMode::SubprocessUnsandboxed => "subprocess-unsandboxed",
            TrustMode::RemoteAgent => "remote-agent",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Command to start the extension's MCP server (stdio). Relative
    /// commands resolve against the extension directory.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// How an accepted extension is reached.
#[derive(Debug, Clone)]
pub enum ExtensionKind {
    /// A spawned MCP server over stdio (tools, memory).
    Server(ServerSpec),
    /// A remote A2A agent the kernel dials as a client (never spawned).
    RemoteAgent(RemoteAgentEndpoint),
    /// An in-process, capability-sandboxed WASM module (ADR 0011).
    Wasm(WasmEndpoint),
}

#[derive(Debug, Clone)]
pub struct RemoteAgentEndpoint {
    pub card_url: String,
    /// Name of an env var holding the bearer token (secrets stay out of the
    /// tree). `None` for an open endpoint.
    pub token_env: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WasmEndpoint {
    /// Absolute path to the `.wasm` module.
    pub module_path: String,
    /// The action name the module exposes — `Some` for a `tools` module
    /// (validated at load), `None` for a `memory` module (fixed op protocol).
    pub tool: Option<String>,
    pub description: String,
    /// The typed argument schema, loaded and checked flat at registration
    /// (`[wasm] parameters`). `None` = single-string ABI.
    pub parameters: Option<serde_json::Value>,
}

/// A manifest the registry accepted, routed by capability.
#[derive(Debug, Clone)]
pub struct RegisteredExtension {
    pub name: String,
    /// Path of the manifest this extension was loaded from.
    pub manifest: String,
    pub capability: Capability,
    pub permissions: Permissions,
    pub trust: TrustMode,
    pub kind: ExtensionKind,
}

impl RegisteredExtension {
    /// The operator-gate check (ADR 0012): a `SubprocessUnsandboxed`
    /// extension requires the operator to have listed it by name. The
    /// manifest declaring the trust mode is necessary but NOT sufficient —
    /// the manifest is untrusted input and cannot authorise its own escape.
    pub fn requires_operator_consent(&self) -> bool {
        self.trust == TrustMode::SubprocessUnsandboxed
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("invalid manifest {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error("extension '{0}' declares unknown capability {1:?} — refusing to load")]
    UnknownCapability(String, String),
    #[error(
        "extension '{0}' declares capability '{1}', which this runtime does not implement yet — \
         refusing to load rather than silently ignoring it"
    )]
    UnimplementedCapability(String, &'static str),
    #[error("extension '{0}' must declare exactly one capability (got {1:?})")]
    OneCapability(String, Vec<String>),
    #[error(
        "extension '{0}' (capability '{1}') needs a [{2}] section in its manifest — refusing to load"
    )]
    MissingSection(String, &'static str, &'static str),
    #[error(
        "agent extension '{0}' does not request the `network` permission — a remote agent \
         cannot be dialed without it; declare [permissions] network = true"
    )]
    NetworkRequired(String),
    #[error("agent extension '{0}' card_url must be http(s), got '{1}'")]
    BadCardUrl(String, String),
    #[error(
        "extension '{0}' declares unknown mechanism '{1}' (expected \"subprocess\" or \"wasm\")"
    )]
    UnknownMechanism(String, String),
    #[error(
        "extension '{0}' capability '{1}' does not support mechanism '{2}' in this runtime slice — \
         WASM currently supports the `tools` and `memory` capabilities"
    )]
    UnsupportedMechanism(String, &'static str, &'static str),
    #[error("wasm tools extension '{0}' must name the action it exposes ([wasm] tool = \"…\")")]
    WasmToolNameRequired(String),
    #[error(
        "wasm memory extension '{0}' must not declare a [wasm] tool name — a memory module \
         serves the fixed reserved-op protocol (memory_write/retrieve/upsert), not a named action"
    )]
    WasmToolNameForbidden(String),
    #[error(
        "wasm extension '{0}' has an unusable [wasm] parameters schema: {1} — it must be a JSON \
         object schema whose properties are all string/boolean/integer/number (flat, no nesting)"
    )]
    BadWasmParameters(String, String),
    #[error(
        "extension '{0}' is a subprocess and did not declare a trust mode — refusing to load \
         (sandbox-by-default). Either use a sandboxed mechanism (mechanism = \"wasm\") or, to run \
         it as an untrusted subprocess with OS-level egress, declare `trust = \
         \"subprocess-unsandboxed\"` AND have the operator allow it"
    )]
    TrustNotDeclared(String),
    #[error(
        "extension '{0}' declares unknown trust mode '{1}' (expected \"wasm-sandboxed\" or \
         \"subprocess-unsandboxed\")"
    )]
    UnknownTrust(String, String),
    #[error(
        "extension '{0}' (WASM, structurally sandboxed) declares trust = \"{1}\" — a WASM \
         extension is sandboxed by construction and must not declare a subprocess trust mode"
    )]
    WasmTrustMismatch(String, String),
    #[error(
        "extension '{0}' is a subprocess opted-in as `subprocess-unsandboxed`, but the operator \
         policy does not allow it — it is not started (a manifest cannot authorise its own escape)"
    )]
    OperatorConsentRequired(String),
}

/// Load a WASM tool's typed argument schema and hold it to the shallow-grammar
/// rule (ADR 0011: nested object grammars are pathologically slow for small
/// models under constrained decoding): an object whose properties are all
/// scalars. `additionalProperties` is forced to `false` — a module never
/// receives keys its schema did not declare.
pub fn load_flat_parameters(path: &Path) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut schema: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let obj = schema
        .as_object_mut()
        .ok_or("schema is not a JSON object")?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("object") {
        return Err("top-level \"type\" must be \"object\"".into());
    }
    let props = obj
        .get("properties")
        .and_then(|p| p.as_object())
        .filter(|p| !p.is_empty())
        .ok_or("\"properties\" must be a non-empty object")?;
    for (k, v) in props {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if !matches!(ty, "string" | "boolean" | "integer" | "number") {
            return Err(format!("property '{k}' has type '{ty}', not a scalar"));
        }
    }
    if let Some(req) = obj.get("required") {
        let req = req.as_array().ok_or("\"required\" must be an array")?;
        for r in req {
            let r = r.as_str().ok_or("\"required\" entries must be strings")?;
            if !props.contains_key(r) {
                return Err(format!("required '{r}' is not a declared property"));
            }
        }
    }
    obj.insert(
        "additionalProperties".into(),
        serde_json::Value::Bool(false),
    );
    // The kernel validates every proposal against this schema; make sure it
    // compiles now (loud at startup), not on the first call.
    jsonschema::validator_for(&schema).map_err(|e| format!("not a valid JSON Schema: {e}"))?;
    Ok(schema)
}

/// Parse one manifest file into a registered extension.
pub fn load_manifest(manifest_path: &Path) -> Result<RegisteredExtension, ManifestError> {
    let display = manifest_path.display().to_string();
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|e| ManifestError::Io(display.clone(), e))?;
    let manifest: Manifest =
        toml::from_str(&raw).map_err(|e| ManifestError::Parse(display.clone(), e))?;

    let name = manifest.extension.name.clone();
    if manifest.extension.provides.len() != 1 {
        return Err(ManifestError::OneCapability(
            name,
            manifest.extension.provides,
        ));
    }
    let declared = &manifest.extension.provides[0];
    let capability = Capability::parse(declared)
        .ok_or_else(|| ManifestError::UnknownCapability(name.clone(), declared.clone()))?;

    let mechanism = manifest.extension.mechanism.as_str();
    if mechanism != "subprocess" && mechanism != "wasm" {
        return Err(ManifestError::UnknownMechanism(name, mechanism.to_string()));
    }
    // WASM supports tools (ADR 0011) and memory (ADR 0024); agent and
    // model-provider with mechanism = "wasm" still fail loudly (deferred).
    if mechanism == "wasm" && !matches!(capability, Capability::Tools | Capability::Memory) {
        let cap = match capability {
            Capability::Agent => "agent",
            Capability::ModelProvider => "model-provider",
            Capability::Tools | Capability::Memory => unreachable!(),
        };
        return Err(ManifestError::UnsupportedMechanism(name, cap, "wasm"));
    }

    let kind = match capability {
        Capability::ModelProvider => {
            return Err(ManifestError::UnimplementedCapability(
                name,
                "model-provider",
            ));
        }
        Capability::Agent => {
            let agent = manifest
                .agent
                .ok_or_else(|| ManifestError::MissingSection(name.clone(), "agent", "agent"))?;
            if !manifest.permissions.network {
                return Err(ManifestError::NetworkRequired(name));
            }
            if !agent.card_url.starts_with("http://") && !agent.card_url.starts_with("https://") {
                return Err(ManifestError::BadCardUrl(name, agent.card_url));
            }
            ExtensionKind::RemoteAgent(RemoteAgentEndpoint {
                card_url: agent.card_url,
                token_env: agent.token_env,
            })
        }
        Capability::Tools | Capability::Memory if manifest.extension.mechanism == "wasm" => {
            let wasm = manifest.wasm.ok_or_else(|| {
                ManifestError::MissingSection(
                    name.clone(),
                    if capability == Capability::Tools {
                        "tools"
                    } else {
                        "memory"
                    },
                    "wasm",
                )
            })?;
            // A tools module exposes ONE named action; a memory module serves
            // the fixed reserved-op protocol — the tool name is required for
            // the former and refused for the latter (never silently ignored).
            match (capability, &wasm.tool) {
                (Capability::Tools, None) => return Err(ManifestError::WasmToolNameRequired(name)),
                (Capability::Memory, Some(_)) => {
                    return Err(ManifestError::WasmToolNameForbidden(name))
                }
                _ => {}
            }
            let dir = manifest_path.parent().unwrap_or(Path::new("."));
            let module_path = dir.join(&wasm.module).to_string_lossy().into_owned();
            let parameters = match (&wasm.parameters, capability) {
                (None, _) => None,
                (Some(_), Capability::Memory) => {
                    return Err(ManifestError::BadWasmParameters(
                        name,
                        "a memory module serves the fixed op protocol and takes no parameters"
                            .into(),
                    ))
                }
                (Some(file), _) => Some(
                    load_flat_parameters(&dir.join(file))
                        .map_err(|e| ManifestError::BadWasmParameters(name.clone(), e))?,
                ),
            };
            ExtensionKind::Wasm(WasmEndpoint {
                module_path,
                tool: wasm.tool,
                description: manifest.extension.description.clone(),
                parameters,
            })
        }
        Capability::Tools | Capability::Memory => {
            let server = manifest.server.ok_or_else(|| {
                ManifestError::MissingSection(
                    name.clone(),
                    if capability == Capability::Tools {
                        "tools"
                    } else {
                        "memory"
                    },
                    "server",
                )
            })?;
            let dir = manifest_path.parent().unwrap_or(Path::new("."));
            // A relative command (e.g. "./run.sh" or ".venv/bin/python")
            // resolves against the extension directory; bare program names
            // ("python3") are left to PATH.
            let command = if server.command.contains('/') {
                dir.join(&server.command).to_string_lossy().into_owned()
            } else {
                server.command.clone()
            };
            ExtensionKind::Server(ServerSpec {
                name: manifest.extension.name.clone(),
                command,
                args: server.args.clone(),
                cwd: Some(dir.to_string_lossy().into_owned()),
                env: Vec::new(),
            })
        }
    };

    // Trust posture (ADR 0012). The manifest DECLARES; the operator gate is
    // applied separately by the daemon (a manifest can't authorise its own
    // escape). Here we validate the manifest side only.
    let declared_trust = manifest.extension.trust.as_deref();
    let trust = match &kind {
        ExtensionKind::Wasm(_) => {
            // Structurally sandboxed. It may omit `trust` or say
            // "wasm-sandboxed"; declaring a subprocess trust is a mistake.
            match declared_trust {
                None | Some("wasm-sandboxed") => TrustMode::WasmSandboxed,
                Some(other) => {
                    return Err(ManifestError::WasmTrustMismatch(name, other.to_string()))
                }
            }
        }
        ExtensionKind::RemoteAgent(_) => {
            // A separate axis (origin-pin/SSRF, ADR 0009); the subprocess
            // trust gate does not apply. A stray `trust` is ignored.
            TrustMode::RemoteAgent
        }
        ExtensionKind::Server(_) => match declared_trust {
            // Sandbox-by-default: a subprocess with no declaration is refused.
            None => return Err(ManifestError::TrustNotDeclared(name)),
            Some("subprocess-unsandboxed") => TrustMode::SubprocessUnsandboxed,
            Some("wasm-sandboxed") => {
                // Claiming sandbox for a subprocess is exactly the overclaim
                // ADR 0012 forbids.
                return Err(ManifestError::WasmTrustMismatch(
                    name,
                    "wasm-sandboxed".into(),
                ));
            }
            Some(other) => return Err(ManifestError::UnknownTrust(name, other.to_string())),
        },
    };

    Ok(RegisteredExtension {
        name,
        manifest: display,
        capability,
        permissions: manifest.permissions,
        trust,
        kind,
    })
}

impl ManifestError {
    /// The extension name the error is about, when the manifest got far
    /// enough to name one (an unreadable or unparseable file did not).
    pub fn extension_name(&self) -> Option<&str> {
        use ManifestError::*;
        match self {
            Io(..) | Parse(..) => None,
            UnknownCapability(n, _)
            | UnimplementedCapability(n, _)
            | OneCapability(n, _)
            | MissingSection(n, ..)
            | NetworkRequired(n)
            | BadCardUrl(n, _)
            | UnknownMechanism(n, _)
            | UnsupportedMechanism(n, ..)
            | WasmToolNameRequired(n)
            | WasmToolNameForbidden(n)
            | BadWasmParameters(n, _)
            | TrustNotDeclared(n)
            | UnknownTrust(n, _)
            | WasmTrustMismatch(n, _)
            | OperatorConsentRequired(n) => Some(n),
        }
    }
}

/// Why the registry skipped an extension (ADR 0035). Serialized in
/// `snake_case` for the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The manifest could not be read, parsed or validated.
    InvalidManifest,
    /// A valid `subprocess-unsandboxed` manifest the operator has not listed
    /// in `[trust] allow_unsandboxed_subprocess` (ADR 0012's operator gate).
    OperatorConsentRequired,
}

impl SkipReason {
    /// The `snake_case` label, identical to the serialized form.
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::InvalidManifest => "invalid_manifest",
            SkipReason::OperatorConsentRequired => "operator_consent_required",
        }
    }
}

/// An extension the registry refused to load. It is NOT run — skipping is
/// the refusal (ADR 0012) without taking the rest of the daemon down with
/// it (ADR 0035).
#[derive(Debug, Clone, Serialize)]
pub struct SkippedExtension {
    /// `None` when the manifest was too broken to name the extension.
    pub name: Option<String>,
    pub manifest: String,
    pub reason: SkipReason,
    /// The full refusal, as it is also logged.
    pub detail: String,
    /// What the operator can do about it.
    pub remedy: String,
}

impl SkippedExtension {
    fn invalid(manifest: &Path, err: ManifestError) -> Self {
        Self {
            name: err.extension_name().map(str::to_string),
            manifest: manifest.display().to_string(),
            reason: SkipReason::InvalidManifest,
            detail: err.to_string(),
            remedy: "fix the manifest (or remove the extension directory) and restart the daemon"
                .to_string(),
        }
    }
}

/// The registry outcome: what loads, and what was refused and why.
#[derive(Debug, Default)]
pub struct Registry {
    pub extensions: Vec<RegisteredExtension>,
    pub skipped: Vec<SkippedExtension>,
}

/// Every manifest path under `dirs`: each immediate subdirectory (or the
/// directory itself) containing `kelter-extension.toml`.
fn manifest_candidates(dirs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let root = Path::new(dir);
        let mut candidates = vec![root.join(MANIFEST_NAME)];
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    candidates.push(p.join(MANIFEST_NAME));
                }
            }
        }
        out.extend(candidates.into_iter().filter(|p| p.is_file()));
    }
    out
}

/// Scan directories for extension manifests: each immediate subdirectory
/// (or the directory itself) containing `kelter-extension.toml` is an
/// extension. Invalid manifests are reported, not silently skipped.
pub fn discover(dirs: &[String]) -> (Vec<RegisteredExtension>, Vec<ManifestError>) {
    let mut extensions = Vec::new();
    let mut errors = Vec::new();
    for manifest in manifest_candidates(dirs) {
        match load_manifest(&manifest) {
            Ok(ext) => extensions.push(ext),
            Err(e) => errors.push(e),
        }
    }
    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    (extensions, errors)
}

/// Discover extensions AND apply the operator gate (ADR 0012, ADR 0035).
///
/// - An invalid manifest is skipped ([`SkipReason::InvalidManifest`]).
/// - A `subprocess-unsandboxed` extension whose name is NOT in
///   `allow_unsandboxed` is skipped ([`SkipReason::OperatorConsentRequired`]).
///   The manifest is untrusted input and cannot authorise its own escape; a
///   skipped extension is never started.
/// - Everything else is returned for the daemon to wire up.
///
/// Never fails: the daemon logs each skip at ERROR and keeps starting with
/// the remaining extensions. The caller's own configuration errors are not
/// this function's concern and still abort the start.
pub fn resolve(dirs: &[String], allow_unsandboxed: &[String]) -> Registry {
    let mut registry = Registry::default();
    for manifest in manifest_candidates(dirs) {
        match load_manifest(&manifest) {
            Ok(ext)
                if ext.requires_operator_consent()
                    && !allow_unsandboxed.iter().any(|n| n == &ext.name) =>
            {
                registry.skipped.push(SkippedExtension {
                    detail: ManifestError::OperatorConsentRequired(ext.name.clone()).to_string(),
                    remedy: format!(
                        "add \"{}\" to [trust] allow_unsandboxed_subprocess in the daemon config \
                         and restart — an explicit trust decision that does NOT sandbox the \
                         subprocess (ADR 0012); or ship it as a WASM extension instead",
                        ext.name
                    ),
                    name: Some(ext.name),
                    manifest: ext.manifest,
                    reason: SkipReason::OperatorConsentRequired,
                });
            }
            Ok(ext) => registry.extensions.push(ext),
            Err(e) => registry
                .skipped
                .push(SkippedExtension::invalid(&manifest, e)),
        }
    }
    registry.extensions.sort_by(|a, b| a.name.cmp(&b.name));
    registry.skipped.sort_by(|a, b| a.manifest.cmp(&b.manifest));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ext(root: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_NAME), body).unwrap();
        dir
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kelter-reg-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn discovers_tool_and_memory_extensions() {
        let root = tmp();
        let tool_dir = write_ext(
            &root,
            "tooly",
            "[extension]\nname = \"tooly\"\nprovides = [\"tools\"]\ntrust = \"subprocess-unsandboxed\"\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        write_ext(
            &root,
            "memy",
            "[extension]\nname = \"memy\"\nprovides = [\"memory\"]\ntrust = \"subprocess-unsandboxed\"\n\n[permissions]\nworkspace_write = true\n\n[server]\ncommand = \"python3\"\nargs = [\"server.py\"]\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0].capability, Capability::Memory);
        assert!(exts[0].permissions.workspace_write);
        assert_eq!(exts[0].trust, TrustMode::SubprocessUnsandboxed);
        assert_eq!(exts[1].capability, Capability::Tools);
        // relative command resolved into the extension dir; bare left to PATH
        let (mem_cmd, tool_cmd) = match (&exts[0].kind, &exts[1].kind) {
            (ExtensionKind::Server(m), ExtensionKind::Server(t)) => {
                (m.command.clone(), t.command.clone())
            }
            other => panic!("expected two server extensions, got {other:?}"),
        };
        assert_eq!(tool_cmd, tool_dir.join("./run.sh").to_string_lossy());
        assert_eq!(mem_cmd, "python3");
    }

    #[test]
    fn agent_extension_loads_with_card_url_and_network() {
        let root = tmp();
        write_ext(
            &root,
            "remote",
            "[extension]\nname = \"remote\"\nprovides = [\"agent\"]\n\n[permissions]\nnetwork = true\n\n[agent]\ncard_url = \"http://127.0.0.1:9999\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts[0].capability, Capability::Agent);
        match &exts[0].kind {
            ExtensionKind::RemoteAgent(a) => assert_eq!(a.card_url, "http://127.0.0.1:9999"),
            other => panic!("expected remote agent, got {other:?}"),
        }
    }

    #[test]
    fn wasm_tool_extension_routes_to_wasm_kind() {
        let root = tmp();
        write_ext(
            &root,
            "wc",
            "[extension]\nname = \"wc\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\ntool = \"word_count\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts.len(), 1);
        match &exts[0].kind {
            ExtensionKind::Wasm(w) => {
                assert!(w.module_path.ends_with("m.wasm"));
                assert_eq!(w.tool.as_deref(), Some("word_count"));
            }
            other => panic!("expected wasm kind, got {other:?}"),
        }
    }

    fn typed_wasm_manifest(provides: &str, schema: &str) -> (std::path::PathBuf, String) {
        let root = tmp();
        let tool_line = if provides == "tools" {
            "tool = \"t\"\n"
        } else {
            ""
        };
        let dir = write_ext(
            &root,
            "typed",
            &format!(
                "[extension]\nname = \"typed\"\nprovides = [\"{provides}\"]\nmechanism = \"wasm\"\n\n\
                 [wasm]\nmodule = \"m.wasm\"\n{tool_line}parameters = \"schema.json\"\n"
            ),
        );
        std::fs::write(dir.join("schema.json"), schema).unwrap();
        (root.clone(), root.to_string_lossy().into_owned())
    }

    #[test]
    fn wasm_tool_typed_parameters_load_flat_and_closed() {
        let (_, root) = typed_wasm_manifest(
            "tools",
            r#"{"type":"object","properties":{"sciezka":{"type":"string"},"nadpisz":{"type":"boolean"}},"required":["sciezka"]}"#,
        );
        let (exts, errors) = discover(&[root]);
        assert!(errors.is_empty(), "{errors:?}");
        match &exts[0].kind {
            ExtensionKind::Wasm(w) => {
                let p = w.parameters.as_ref().expect("parameters loaded");
                assert_eq!(p["properties"]["sciezka"]["type"], "string");
                // Forced closed even though the file did not say so.
                assert_eq!(p["additionalProperties"], false);
            }
            other => panic!("expected wasm kind, got {other:?}"),
        }
    }

    #[test]
    fn wasm_tool_nested_or_broken_parameters_are_refused() {
        for bad in [
            r#"{"type":"object","properties":{"o":{"type":"object"}}}"#,
            r#"{"type":"object","properties":{"a":{"type":"array"}}}"#,
            r#"{"type":"object","properties":{}}"#,
            r#"{"type":"string"}"#,
            r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["b"]}"#,
            "not json",
        ] {
            let (_, root) = typed_wasm_manifest("tools", bad);
            let (exts, errors) = discover(&[root]);
            assert!(exts.is_empty(), "{bad} must be refused");
            assert!(
                errors[0].to_string().contains("parameters schema"),
                "{bad}: {}",
                errors[0]
            );
        }
    }

    #[test]
    fn wasm_memory_with_parameters_is_refused() {
        let (_, root) = typed_wasm_manifest(
            "memory",
            r#"{"type":"object","properties":{"a":{"type":"string"}}}"#,
        );
        let (exts, errors) = discover(&[root]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("takes no parameters"));
    }

    /// The deployable extension shipped in-tree (deploy/extensions) must
    /// always pass the registry — it is what K12 mounts.
    #[test]
    fn shipped_zapisz_plik_extension_registers() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/extensions");
        let (exts, errors) = discover(&[root.to_string()]);
        assert!(errors.is_empty(), "{errors:?}");
        let ext = exts
            .iter()
            .find(|e| e.name == "zapisz-plik")
            .expect("zapisz-plik discovered");
        assert_eq!(ext.trust, TrustMode::WasmSandboxed);
        assert!(ext.permissions.workspace_write);
        assert!(!ext.permissions.network);
        match &ext.kind {
            ExtensionKind::Wasm(w) => {
                assert_eq!(w.tool.as_deref(), Some("zapisz_plik"));
                let p = w.parameters.as_ref().unwrap();
                assert_eq!(p["required"], serde_json::json!(["sciezka", "tresc"]));
            }
            other => panic!("expected wasm kind, got {other:?}"),
        }
    }

    #[test]
    fn subprocess_without_trust_declaration_is_refused() {
        let root = tmp();
        write_ext(
            &root,
            "untrusted",
            "[extension]\nname = \"untrusted\"\nprovides = [\"tools\"]\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0]
            .to_string()
            .contains("did not declare a trust mode"));
    }

    #[test]
    fn subprocess_cannot_claim_wasm_sandboxed() {
        // The overclaim ADR 0012 forbids: a subprocess labelling itself as
        // sandboxed.
        let root = tmp();
        write_ext(
            &root,
            "liar",
            "[extension]\nname = \"liar\"\nprovides = [\"tools\"]\ntrust = \"wasm-sandboxed\"\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("sandboxed by construction"));
    }

    #[test]
    fn unknown_trust_mode_is_refused() {
        let root = tmp();
        write_ext(
            &root,
            "weird-trust",
            "[extension]\nname = \"weird-trust\"\nprovides = [\"tools\"]\ntrust = \"totally-safe-trust-me\"\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("unknown trust mode"));
    }

    #[test]
    fn wasm_extension_is_sandboxed_by_default() {
        let root = tmp();
        write_ext(
            &root,
            "wc",
            "[extension]\nname = \"wc\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\ntool = \"t\"\n",
        );
        let (exts, _) = discover(&[root.to_string_lossy().into_owned()]);
        assert_eq!(exts[0].trust, TrustMode::WasmSandboxed);
        assert!(!exts[0].requires_operator_consent());
    }

    #[test]
    fn subprocess_opt_in_requires_operator_consent() {
        // The two-part gate: the manifest declaring opt-in makes
        // `requires_operator_consent()` true; the operator still has to allow
        // it (`resolve`, tested below). The manifest alone does NOT let it
        // in.
        let root = tmp();
        write_ext(
            &root,
            "opted-in",
            "[extension]\nname = \"opted-in\"\nprovides = [\"tools\"]\ntrust = \"subprocess-unsandboxed\"\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts[0].trust, TrustMode::SubprocessUnsandboxed);
        assert!(
            exts[0].requires_operator_consent(),
            "manifest opt-in must still require operator consent"
        );
    }

    #[test]
    fn wasm_needs_wasm_section() {
        let root = tmp();
        write_ext(
            &root,
            "nowasm",
            "[extension]\nname = \"nowasm\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("[wasm] section"));
    }

    #[test]
    fn wasm_memory_extension_routes_to_wasm_kind_sandboxed() {
        // ADR 0024: memory + wasm is a supported combination — routed to the
        // WASM kind, structurally sandboxed, no operator consent needed.
        let root = tmp();
        write_ext(
            &root,
            "wmem",
            "[extension]\nname = \"wmem\"\nprovides = [\"memory\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(exts[0].capability, Capability::Memory);
        assert_eq!(exts[0].trust, TrustMode::WasmSandboxed);
        assert!(!exts[0].requires_operator_consent());
        match &exts[0].kind {
            ExtensionKind::Wasm(w) => {
                assert!(w.module_path.ends_with("m.wasm"));
                assert_eq!(w.tool, None);
            }
            other => panic!("expected wasm kind, got {other:?}"),
        }
    }

    #[test]
    fn wasm_memory_extension_must_not_name_a_tool() {
        let root = tmp();
        write_ext(
            &root,
            "wmem-tool",
            "[extension]\nname = \"wmem-tool\"\nprovides = [\"memory\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\ntool = \"t\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("must not declare"));
    }

    #[test]
    fn wasm_tools_extension_must_name_a_tool() {
        let root = tmp();
        write_ext(
            &root,
            "wtool-anon",
            "[extension]\nname = \"wtool-anon\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"m.wasm\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("must name the action"));
    }

    #[test]
    fn wasm_still_refuses_agent_capability() {
        // The forward-compatible refusal shrank to agent/model-provider —
        // it did not disappear.
        let root = tmp();
        write_ext(
            &root,
            "wagent",
            "[extension]\nname = \"wagent\"\nprovides = [\"agent\"]\nmechanism = \"wasm\"\n\n[permissions]\nnetwork = true\n\n[wasm]\nmodule = \"m.wasm\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0]
            .to_string()
            .contains("does not support mechanism 'wasm'"));
    }

    #[test]
    fn unknown_mechanism_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "weird-mech",
            "[extension]\nname = \"weird-mech\"\nprovides = [\"tools\"]\nmechanism = \"telepathy\"\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("unknown mechanism"));
    }

    #[test]
    fn agent_without_network_permission_is_refused() {
        let root = tmp();
        write_ext(
            &root,
            "sneaky-agent",
            "[extension]\nname = \"sneaky-agent\"\nprovides = [\"agent\"]\n\n[agent]\ncard_url = \"http://127.0.0.1:9999\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("network"));
    }

    #[test]
    fn agent_needs_agent_section_not_server() {
        let root = tmp();
        write_ext(
            &root,
            "confused",
            "[extension]\nname = \"confused\"\nprovides = [\"agent\"]\n\n[permissions]\nnetwork = true\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("[agent] section"));
    }

    #[test]
    fn unknown_capability_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "weird",
            "[extension]\nname = \"weird\"\nprovides = [\"telepathy\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (exts, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(exts.is_empty());
        assert!(errors[0].to_string().contains("unknown capability"));
    }

    #[test]
    fn declared_but_unimplemented_capability_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "modelp",
            "[extension]\nname = \"modelp\"\nprovides = [\"model-provider\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("does not implement yet"));
    }

    #[test]
    fn unknown_permission_key_fails_loudly() {
        let root = tmp();
        write_ext(
            &root,
            "sneaky",
            "[extension]\nname = \"sneaky\"\nprovides = [\"tools\"]\n\n[permissions]\nsudo = true\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(matches!(errors[0], ManifestError::Parse(..)));
    }

    #[test]
    fn exactly_one_capability_required() {
        let root = tmp();
        write_ext(
            &root,
            "greedy",
            "[extension]\nname = \"greedy\"\nprovides = [\"tools\", \"memory\"]\n\n[server]\ncommand = \"x\"\n",
        );
        let (_, errors) = discover(&[root.to_string_lossy().into_owned()]);
        assert!(errors[0].to_string().contains("exactly one capability"));
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let (exts, errors) = discover(&["/nonexistent/kelter-ext".to_string()]);
        assert!(exts.is_empty());
        assert!(errors.is_empty());
    }

    const WASM_OK: &str = "[extension]\nname = \"wc\"\nprovides = [\"tools\"]\nmechanism = \"wasm\"\n\n[wasm]\nmodule = \"wc.wasm\"\ntool = \"word_count\"\n";
    const UNTRUSTED_SUB: &str = "[extension]\nname = \"wordstats\"\nprovides = [\"tools\"]\ntrust = \"subprocess-unsandboxed\"\n\n[server]\ncommand = \"./run.sh\"\n";

    /// ADR 0035: a mix of a valid WASM tool, an un-allowed unsandboxed
    /// subprocess and an unparseable manifest resolves to ONE registered
    /// extension and TWO reported skips — nothing aborts.
    #[test]
    fn resolve_skips_untrusted_subprocess_and_broken_manifest() {
        let root = tmp();
        write_ext(&root, "wc", WASM_OK);
        write_ext(&root, "wordstats", UNTRUSTED_SUB);
        write_ext(&root, "broken", "[extension\nname = ");
        let reg = resolve(&[root.to_string_lossy().into_owned()], &[]);

        assert_eq!(reg.extensions.len(), 1, "{:?}", reg.extensions);
        assert_eq!(reg.extensions[0].name, "wc");
        assert_eq!(reg.extensions[0].trust, TrustMode::WasmSandboxed);

        assert_eq!(reg.skipped.len(), 2, "{:?}", reg.skipped);
        let broken = reg
            .skipped
            .iter()
            .find(|s| s.reason == SkipReason::InvalidManifest)
            .expect("broken manifest reported");
        assert_eq!(broken.name, None);
        assert!(broken.manifest.ends_with("broken/kelter-extension.toml"));
        assert!(
            broken.detail.contains("invalid manifest"),
            "{}",
            broken.detail
        );

        let untrusted = reg
            .skipped
            .iter()
            .find(|s| s.reason == SkipReason::OperatorConsentRequired)
            .expect("untrusted subprocess reported");
        assert_eq!(untrusted.name.as_deref(), Some("wordstats"));
        assert!(untrusted
            .detail
            .contains("operator policy does not allow it"));
        assert!(
            untrusted
                .remedy
                .contains("[trust] allow_unsandboxed_subprocess"),
            "{}",
            untrusted.remedy
        );
        assert!(untrusted.remedy.contains("\"wordstats\""));

        // `label()` is what the API reports; it must match the serde form.
        for s in &reg.skipped {
            assert_eq!(serde_json::to_value(s.reason).unwrap(), s.reason.label());
        }
    }

    /// The operator gate still works the other way: listing the extension
    /// in `[trust] allow_unsandboxed_subprocess` lets it register.
    #[test]
    fn resolve_registers_subprocess_the_operator_allowed() {
        let root = tmp();
        write_ext(&root, "wordstats", UNTRUSTED_SUB);
        let dirs = [root.to_string_lossy().into_owned()];

        let reg = resolve(&dirs, &["wordstats".to_string()]);
        assert!(reg.skipped.is_empty(), "{:?}", reg.skipped);
        assert_eq!(reg.extensions.len(), 1);
        assert_eq!(reg.extensions[0].trust, TrustMode::SubprocessUnsandboxed);

        // Consent is per name: allowing a different extension is not consent.
        let reg = resolve(&dirs, &["someone-else".to_string()]);
        assert!(reg.extensions.is_empty());
        assert_eq!(reg.skipped[0].reason, SkipReason::OperatorConsentRequired);
    }

    /// A semantically invalid (but parseable) manifest is skipped too, and
    /// still names the extension.
    #[test]
    fn resolve_skips_invalid_manifest_with_its_name() {
        let root = tmp();
        write_ext(
            &root,
            "notrust",
            "[extension]\nname = \"notrust\"\nprovides = [\"tools\"]\n\n[server]\ncommand = \"./run.sh\"\n",
        );
        let reg = resolve(&[root.to_string_lossy().into_owned()], &[]);
        assert!(reg.extensions.is_empty());
        assert_eq!(reg.skipped[0].reason, SkipReason::InvalidManifest);
        assert_eq!(reg.skipped[0].name.as_deref(), Some("notrust"));
        assert!(reg.skipped[0]
            .detail
            .contains("did not declare a trust mode"));
    }
}
