//! The bundled reference MCP tool server (read → extract → report), usable
//! over stdio (child process) or an in-process duplex stream. Either way it
//! is a normal MCP server — the kernel client speaks the same protocol.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

use crate::tools::{self, Workspace};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSourceParams {
    /// URL (http/https) or local file path to read.
    pub input: String,
    /// Workspace-relative file name to store the raw content under.
    pub save_as: String,
    /// Set by the runtime on resume; this tool is a deterministic
    /// overwrite, so replaying with the same arguments is already safe.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExtractParams {
    /// Workspace-relative file previously stored by read_source.
    pub source: String,
    /// Workspace-relative file name for the extracted JSON structure.
    pub save_as: String,
    /// Set by the runtime on resume; deterministic overwrite — replay-safe.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportParams {
    /// Workspace-relative extraction JSON produced by extract_structure.
    pub source: String,
    /// Workspace-relative output file for the final report.
    pub output: String,
    /// Report format: "markdown" or "json".
    pub format: String,
    /// Set by the runtime on resume; deterministic overwrite — replay-safe.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LedgerParams {
    /// Text to append as one ledger line.
    pub entry: String,
    /// Workspace-relative ledger file. Default: "ledger.txt".
    pub ledger: Option<String>,
    /// Set by the runtime; a repeated call with the same key is a no-op
    /// (the appended line carries the key — effect and dedupe marker are
    /// one atomic write).
    pub idempotency_key: Option<String>,
}

#[derive(Clone)]
pub struct ToolServer {
    ws: Workspace,
    ledger_index: tools::LedgerIndex,
}

impl ToolServer {
    pub fn new(workspace: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            ws: Workspace::new(workspace)?,
            ledger_index: Default::default(),
        })
    }
}

fn ok(value: serde_json::Value) -> CallToolResult {
    CallToolResult::structured(value)
}

fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": e.to_string() }))
}

#[tool_router(server_handler)]
impl ToolServer {
    #[tool(
        description = "Read a source document (URL or local file path) and store its raw content in the workspace under save_as."
    )]
    async fn read_source(&self, Parameters(p): Parameters<ReadSourceParams>) -> CallToolResult {
        match tools::read_source(&self.ws, &p.input, &p.save_as).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Extract structured information (title, headings, links, word count) from a stored source file and save it as JSON under save_as."
    )]
    async fn extract_structure(&self, Parameters(p): Parameters<ExtractParams>) -> CallToolResult {
        match tools::extract_structure(&self.ws, &p.source, &p.save_as).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Render the extracted structure JSON into a final report file (markdown or json) at output."
    )]
    async fn write_report(&self, Parameters(p): Parameters<ReportParams>) -> CallToolResult {
        match tools::write_report(&self.ws, &p.source, &p.output, &p.format).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Append one entry line to a ledger file in the workspace (a durable, observable side effect)."
    )]
    async fn append_ledger(&self, Parameters(p): Parameters<LedgerParams>) -> CallToolResult {
        let ledger = p.ledger.as_deref().unwrap_or("ledger.txt");
        match tools::append_ledger(
            &self.ws,
            &p.entry,
            ledger,
            p.idempotency_key.as_deref(),
            &self.ledger_index,
        )
        .await
        {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }
}

// ---- code tools (A1/C4/C5, oculink agentic-coding) ------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CzytajPlikParams {
    /// Ścieżka względem katalogu kodu.
    pub sciezka: String,
    /// Pierwsza linia do pokazania (1-based, jak w komunikatach kompilatora).
    pub od_linii: Option<usize>,
    /// Maksymalna liczba linii (domyślnie 400).
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PoprawKodParams {
    /// Ścieżka pliku do zmiany, względem katalogu kodu.
    pub sciezka: String,
    /// Słowny opis zmiany do wykonania (co i dlaczego, nie treść pliku).
    pub instrukcja: String,
    /// Set by the runtime on resume; a repeated call with the same key
    /// returns the stored result without re-editing.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ZastosujPatchParams {
    /// Ścieżka pliku do zmiany, względem katalogu kodu.
    pub sciezka: String,
    /// Dokładny istniejący fragment (musi wystąpić dokładnie raz).
    pub stary: String,
    /// Fragment zastępujący.
    pub nowy: String,
    /// Set by the runtime on resume; same-key replay is a no-op.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UruchomKomendeParams {
    /// Polecenie z listy dozwolonych (np. "cargo", "git").
    pub polecenie: String,
    /// Argumenty polecenia.
    pub argumenty: Option<Vec<String>>,
    /// Katalog roboczy względem katalogu kodu (domyślnie jego korzeń).
    pub cwd: Option<String>,
    /// Set by the runtime on resume; same-key replay returns the stored
    /// result without re-running the command.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GitParams {
    /// Opcjonalna ścieżka/plik zawężający wynik.
    pub sciezka: Option<String>,
}

/// Shared state + config of both code-server variants.
#[derive(Clone)]
pub struct CodeToolsCore {
    root: crate::code_tools::CodeRoot,
    executor: Option<crate::code_tools::ExecutorConfig>,
    allowed: Vec<String>,
    timeout_secs: u64,
    max_errors: usize,
}

impl CodeToolsCore {
    pub fn new(
        root: impl Into<std::path::PathBuf>,
        executor: Option<crate::code_tools::ExecutorConfig>,
        allowed: Vec<String>,
        timeout_secs: u64,
        max_errors: usize,
    ) -> std::io::Result<Self> {
        Ok(Self {
            root: crate::code_tools::CodeRoot::new(root)?,
            executor,
            allowed,
            timeout_secs,
            max_errors,
        })
    }

    async fn czytaj(&self, p: CzytajPlikParams) -> CallToolResult {
        match crate::code_tools::czytaj_plik(&self.root, &p.sciezka, p.od_linii, p.limit) {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    async fn komenda(&self, p: UruchomKomendeParams) -> CallToolResult {
        match crate::code_tools::uruchom_komende(
            &self.root,
            &p.polecenie,
            &p.argumenty.unwrap_or_default(),
            p.cwd.as_deref(),
            p.idempotency_key.as_deref(),
            &self.allowed,
            self.timeout_secs,
            self.max_errors,
        )
        .await
        {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    async fn git(&self, sub: &str, extra: Option<String>) -> CallToolResult {
        let mut args = vec![sub.to_string()];
        if sub == "diff" {
            args.push("--stat".into());
            args.push("-p".into());
        }
        if let Some(s) = extra {
            args.push(s);
        }
        match crate::code_tools::uruchom_komende(
            &self.root,
            "git",
            &args,
            None,
            None,
            &["git".to_string()],
            self.timeout_secs,
            self.max_errors,
        )
        .await
        {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }
}

/// The separated-roles code server (A1, the DEFAULT): the planner has NO
/// tool that writes file content — the only route to changing code is
/// `popraw_kod(sciezka, instrukcja)`, executed by a tool-side model call.
#[derive(Clone)]
pub struct CodeServer {
    core: CodeToolsCore,
}

impl CodeServer {
    pub fn new(core: CodeToolsCore) -> Self {
        Self { core }
    }
}

#[tool_router(server_handler)]
impl CodeServer {
    #[tool(
        description = "Przeczytaj plik z katalogu kodu; zwraca NUMEROWANE linie (od_linii/limit zawężają okno)."
    )]
    async fn czytaj_plik(&self, Parameters(p): Parameters<CzytajPlikParams>) -> CallToolResult {
        self.core.czytaj(p).await
    }

    #[tool(
        description = "Zleć zmianę kodu w pliku, opisaną słownie — jedyna droga do edycji. Wykonawca zastosuje zmianę; potem zweryfikuj testami."
    )]
    async fn popraw_kod(&self, Parameters(p): Parameters<PoprawKodParams>) -> CallToolResult {
        let Some(executor) = &self.core.executor else {
            return fail("popraw_kod wymaga skonfigurowanego wykonawcy (executor_url)");
        };
        match crate::code_tools::popraw_kod(
            &self.core.root,
            executor,
            &p.sciezka,
            &p.instrukcja,
            p.idempotency_key.as_deref(),
        )
        .await
        {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Uruchom dozwolone polecenie (np. cargo test) w katalogu kodu; zwraca kod wyjścia i streszczenie błędów, pełny log w pliku."
    )]
    async fn uruchom_komende(
        &self,
        Parameters(p): Parameters<UruchomKomendeParams>,
    ) -> CallToolResult {
        self.core.komenda(p).await
    }

    #[tool(description = "git status w katalogu kodu.")]
    async fn git_status(&self, Parameters(p): Parameters<GitParams>) -> CallToolResult {
        self.core.git("status", p.sciezka).await
    }

    #[tool(description = "git diff (z patchem) w katalogu kodu.")]
    async fn git_diff(&self, Parameters(p): Parameters<GitParams>) -> CallToolResult {
        self.core.git("diff", p.sciezka).await
    }
}

/// The merged-roles code server — the "before A1" configuration, kept for
/// measurement: the planner writes file content DIRECTLY via
/// `zastosuj_patch` (no executor). Opt-in (`--merged-roles`).
#[derive(Clone)]
pub struct CodeServerMerged {
    core: CodeToolsCore,
}

impl CodeServerMerged {
    pub fn new(core: CodeToolsCore) -> Self {
        Self { core }
    }
}

#[tool_router(server_handler)]
impl CodeServerMerged {
    #[tool(
        description = "Przeczytaj plik z katalogu kodu; zwraca NUMEROWANE linie (od_linii/limit zawężają okno)."
    )]
    async fn czytaj_plik(&self, Parameters(p): Parameters<CzytajPlikParams>) -> CallToolResult {
        self.core.czytaj(p).await
    }

    #[tool(
        description = "Zamień w pliku dokładny fragment 'stary' (musi wystąpić dokładnie raz) na 'nowy'."
    )]
    async fn zastosuj_patch(
        &self,
        Parameters(p): Parameters<ZastosujPatchParams>,
    ) -> CallToolResult {
        match crate::code_tools::zastosuj_patch(
            &self.core.root,
            &p.sciezka,
            &p.stary,
            &p.nowy,
            p.idempotency_key.as_deref(),
        ) {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Uruchom dozwolone polecenie (np. cargo test) w katalogu kodu; zwraca kod wyjścia i streszczenie błędów, pełny log w pliku."
    )]
    async fn uruchom_komende(
        &self,
        Parameters(p): Parameters<UruchomKomendeParams>,
    ) -> CallToolResult {
        self.core.komenda(p).await
    }

    #[tool(description = "git status w katalogu kodu.")]
    async fn git_status(&self, Parameters(p): Parameters<GitParams>) -> CallToolResult {
        self.core.git("status", p.sciezka).await
    }

    #[tool(description = "git diff (z patchem) w katalogu kodu.")]
    async fn git_diff(&self, Parameters(p): Parameters<GitParams>) -> CallToolResult {
        self.core.git("diff", p.sciezka).await
    }
}

/// A minimal server exposing ONLY the ledger tool — used by the durability
/// smoke to give a small local model a two-action space (append_ledger +
/// finish) so kill-and-resume runs are deterministic.
#[derive(Clone)]
pub struct LedgerServer {
    ws: Workspace,
    ledger_index: tools::LedgerIndex,
}

impl LedgerServer {
    pub fn new(workspace: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            ws: Workspace::new(workspace)?,
            ledger_index: Default::default(),
        })
    }
}

#[tool_router(server_handler)]
impl LedgerServer {
    #[tool(
        description = "Append one entry line to a ledger file in the workspace (a durable, observable side effect)."
    )]
    async fn append_ledger(&self, Parameters(p): Parameters<LedgerParams>) -> CallToolResult {
        let ledger = p.ledger.as_deref().unwrap_or("ledger.txt");
        match tools::append_ledger(
            &self.ws,
            &p.entry,
            ledger,
            p.idempotency_key.as_deref(),
            &self.ledger_index,
        )
        .await
        {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }
}
