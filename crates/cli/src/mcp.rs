use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_embed::reranker::Reranker;
use codesage_graph::{
    assess_risk, assess_risk_diff, export_context, find_coupling, find_references, find_symbol,
    impact_analysis, list_dependencies, recommend_tests, search,
};
use codesage_protocol::{
    ExportRequest, FindReferencesRequest, FindSymbolRequest, ImpactRequest, ImpactTarget, Language,
    ReferenceKind, SearchRequest, SymbolKind,
};
use codesage_storage::Database;
use parking_lot::Mutex;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

const PROJECT_ARG_DESC: &str = "Absolute path to the project root. Must be an onboarded CodeSage project (contains .codesage/index.db).";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindSymbolParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name or qualified name to search for")]
    pub name: String,
    #[schemars(
        description = "Filter by kind: function, method, class, trait, interface, struct, enum, constant, macro, module, namespace"
    )]
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name to find references for")]
    pub name: String,
    #[schemars(
        description = "Filter by reference kind: import, include, call, instantiation, inheritance, trait_use, type_hint"
    )]
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDependenciesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Relative file path from project root")]
    pub file_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CouplingParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to look up co-change history for")]
    pub file_path: String,
    #[schemars(description = "Max results (default 10)")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RiskParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to assess")]
    pub file_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RiskDiffParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths in the patch (typically the output of `git diff --name-only`)"
    )]
    pub file_paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TestsForParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file paths whose tests should be recommended")]
    pub file_paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImpactParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name or file path to analyze")]
    pub target: String,
    #[schemars(description = "Treat target as file path (auto-detected if path-like)")]
    pub is_file: Option<bool>,
    #[schemars(description = "Recursion depth for transitive impact (default 2)")]
    pub depth: Option<usize>,
    #[schemars(description = "Exclude test and config files from results")]
    pub source_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExportContextParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Natural language query or symbol name")]
    pub target: String,
    #[schemars(description = "Treat target as a symbol name instead of a semantic query")]
    pub is_symbol: Option<bool>,
    #[schemars(description = "Max primary results to include (default 5)")]
    pub limit: Option<usize>,
    #[schemars(description = "Include caller code in the bundle")]
    pub include_callers: Option<bool>,
    #[schemars(description = "Include callee/dependency code in the bundle")]
    pub include_callees: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Natural language query or code snippet to search for semantically similar code"
    )]
    pub query: String,
    #[schemars(description = "Maximum results to return (default 10)")]
    pub limit: Option<usize>,
    #[schemars(description = "Results offset for pagination")]
    pub offset: Option<usize>,
    #[schemars(description = "Filter by language: php, python, c, rust, javascript, typescript")]
    pub language: Option<String>,
    #[schemars(description = "Filter by file path glob patterns")]
    pub paths: Option<Vec<String>>,
}

#[derive(Clone)]
struct ProjectState {
    db_path: PathBuf,
    embedding_config: EmbeddingConfig,
}

pub struct CodeSageServer {
    projects: Mutex<HashMap<PathBuf, ProjectState>>,
    embedders: Mutex<HashMap<String, Arc<Mutex<Embedder>>>>,
    rerankers: Mutex<HashMap<String, Arc<Mutex<Reranker>>>>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for CodeSageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeSageServer").finish()
    }
}

impl Default for CodeSageServer {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeSageServer {
    pub fn new() -> Self {
        Self {
            projects: Mutex::new(HashMap::new()),
            embedders: Mutex::new(HashMap::new()),
            rerankers: Mutex::new(HashMap::new()),
            tool_router: Self::tool_router(),
        }
    }

    fn resolve_project(&self, project: &str) -> Result<ProjectState> {
        let path = PathBuf::from(project);
        if !path.is_absolute() {
            bail!(
                "`project` must be an absolute path, got `{}`. Pass the absolute project root.",
                project
            );
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("project path `{}` does not exist: {}", project, e))?;
        {
            let guard = self.projects.lock();
            if let Some(state) = guard.get(&canonical) {
                return Ok(state.clone());
            }
        }
        let codesage_dir = canonical.join(".codesage");
        let db_path = codesage_dir.join("index.db");
        if !db_path.exists() {
            bail!(
                "project `{}` is not onboarded (no .codesage/index.db). \
                Run `/codesage-onboard {}` to initialize.",
                canonical.display(),
                canonical.display()
            );
        }
        let embedding_config = load_embedding_config(&codesage_dir.join("config.toml"))?;
        let state = ProjectState {
            db_path,
            embedding_config,
        };
        let mut guard = self.projects.lock();
        guard.entry(canonical).or_insert(state.clone());
        Ok(state)
    }

    fn get_or_load_embedder(&self, config: &EmbeddingConfig) -> Result<Arc<Mutex<Embedder>>> {
        let key = format!("{}|{}", config.model, config.device);
        {
            let guard = self.embedders.lock();
            if let Some(arc) = guard.get(&key) {
                return Ok(arc.clone());
            }
        }
        let embedder = Embedder::new(config).with_context(|| {
            format!(
                "loading embedding model '{}' on device '{}'",
                config.model, config.device
            )
        })?;
        let arc = Arc::new(Mutex::new(embedder));
        let mut guard = self.embedders.lock();
        Ok(guard.entry(key).or_insert(arc).clone())
    }

    fn get_or_load_reranker(
        &self,
        reranker_model: &str,
        device: &str,
    ) -> Result<Arc<Mutex<Reranker>>> {
        let key = format!("{}|{}", reranker_model, device);
        {
            let guard = self.rerankers.lock();
            if let Some(arc) = guard.get(&key) {
                return Ok(arc.clone());
            }
        }
        let reranker = Reranker::new(reranker_model, device).with_context(|| {
            format!("loading reranker model '{reranker_model}' on device '{device}'")
        })?;
        let arc = Arc::new(Mutex::new(reranker));
        let mut guard = self.rerankers.lock();
        Ok(guard.entry(key).or_insert(arc).clone())
    }

    fn open_db_for(&self, state: &ProjectState) -> Result<Database> {
        let embedder_arc = self.get_or_load_embedder(&state.embedding_config)?;
        let embedder = embedder_arc.lock();
        Database::open_for_model(
            &state.db_path,
            &state.embedding_config.model,
            embedder.dim(),
        )
    }

    /// Resolve project, open its DB, run `f` with the DB. Error handling funnel:
    /// each handler's body lives under this so the tool dispatch stays one-liner.
    fn with_project_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_db_for(&state)?;
        f(&db)
    }

    /// Same as `with_project_db` but also acquires the project's embedder and
    /// reranker (if configured). Locks held for the duration of `f`.
    fn with_project_search<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database, &mut Embedder, Option<&mut Reranker>) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_db_for(&state)?;
        let embedder_arc = self.get_or_load_embedder(&state.embedding_config)?;
        let reranker_arc = state
            .embedding_config
            .reranker
            .as_deref()
            .map(|m| self.get_or_load_reranker(m, &state.embedding_config.device))
            .transpose()?;
        let mut embedder_guard = embedder_arc.lock();
        let mut reranker_guard = reranker_arc.as_ref().map(|a| a.lock());
        let reranker_opt = reranker_guard.as_deref_mut();
        f(&db, &mut embedder_guard, reranker_opt)
    }
}

/// Token budget for a single MCP tool response. Above ~10k tokens Claude Code starts to
/// reject results and the agent falls back to multi-call patterns that blow the prompt cache.
/// 8000 leaves headroom and is the same number repowise's tool_context.py settled on.
const MCP_TOKEN_BUDGET: usize = 8000;
/// Conservative chars/token estimate. Replace with a real tokenizer if accuracy ever matters
/// (it doesn't here: under-estimating just means we cap a touch early).
const MCP_CHARS_PER_TOKEN: usize = 4;
const MCP_BUDGET_CHARS: usize = MCP_TOKEN_BUDGET * MCP_CHARS_PER_TOKEN;

/// Render a handler's `Result<T>` as a structured MCP `CallToolResult`. Successful
/// responses ship both the pretty-printed JSON (for the transcript) and the raw
/// `Value` as `structured_content` so clients can parse without re-deserializing.
/// Failures set `isError: true` per MCP spec; the full anyhow cause chain is
/// included via `{:#}`.
fn render_with_kind<T: serde::Serialize>(r: Result<T>, kind: &str) -> CallToolResult {
    match r {
        Ok(v) => {
            let value = serde_json::to_value(&v).unwrap_or(serde_json::Value::Null);
            let capped = cap_to_budget(value, kind);
            let text = serde_json::to_string_pretty(&capped).unwrap_or_default();
            let mut result = CallToolResult::structured(capped);
            // `CallToolResult::structured` defaults content to a compact
            // `value.to_string()`; replace with pretty JSON for transcript use.
            result.content = vec![Content::text(text)];
            result
        }
        Err(e) => CallToolResult::error(vec![Content::text(format!("Error: {e:#}"))]),
    }
}

/// If the serialized value fits within MCP_BUDGET_CHARS, return as-is. Otherwise truncate
/// the largest array field (or the whole value if it's already an array) and attach a
/// top-level `_meta` describing the truncation. Agents pick up the meta and either refine
/// or paginate via `offset`.
fn cap_to_budget(value: serde_json::Value, kind: &str) -> serde_json::Value {
    let initial_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if initial_len <= MCP_BUDGET_CHARS {
        return value;
    }

    match value {
        serde_json::Value::Array(items) => {
            let total = items.len();
            let kept = truncate_array(items, MCP_BUDGET_CHARS);
            let returned = kept.len();
            serde_json::json!({
                "results": kept,
                "_meta": {
                    "truncated": true,
                    "kind": kind,
                    "total_results": total,
                    "returned": returned,
                    "approx_tokens_budget": MCP_TOKEN_BUDGET,
                    "hint": "output exceeded budget; refine query, narrow scope (paths/language), or call with offset to paginate",
                }
            })
        }
        serde_json::Value::Object(mut map) => {
            // Pick the largest top-level array field and trim it.
            let mut largest_key: Option<String> = None;
            let mut largest_len = 0;
            for (k, v) in &map {
                if let serde_json::Value::Array(arr) = v {
                    let s = serde_json::to_string(arr).map(|s| s.len()).unwrap_or(0);
                    if s > largest_len {
                        largest_len = s;
                        largest_key = Some(k.clone());
                    }
                }
            }
            if let Some(key) = largest_key
                && let Some(serde_json::Value::Array(items)) = map.remove(&key)
            {
                let total = items.len();
                let other_chars = initial_len.saturating_sub(largest_len);
                let remaining = MCP_BUDGET_CHARS.saturating_sub(other_chars);
                let kept = truncate_array(items, remaining);
                let returned = kept.len();
                map.insert(key.clone(), serde_json::Value::Array(kept));
                map.insert(
                    "_meta".to_string(),
                    serde_json::json!({
                        "truncated": true,
                        "kind": kind,
                        "field": key,
                        "total_results": total,
                        "returned": returned,
                        "approx_tokens_budget": MCP_TOKEN_BUDGET,
                        "hint": "output exceeded budget; refine query or narrow scope",
                    }),
                );
            }
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

fn truncate_array(items: Vec<serde_json::Value>, budget_chars: usize) -> Vec<serde_json::Value> {
    let mut kept = Vec::new();
    let mut used = 0;
    for item in items {
        let s = serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
        if used + s > budget_chars && !kept.is_empty() {
            break;
        }
        used += s;
        kept.push(item);
    }
    kept
}

fn load_embedding_config(path: &Path) -> Result<EmbeddingConfig> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EmbeddingConfig::default());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("reading {}", path.display()));
        }
    };
    #[derive(serde::Deserialize)]
    struct Config {
        embedding: Option<EmbeddingConfig>,
    }
    let parsed: Config =
        toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed.embedding.unwrap_or_default())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeSageServer {
    fn get_info(&self) -> ServerInfo {
        use rmcp::model::ServerCapabilities;
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Structural and semantic code intelligence across multiple projects. \
                 Every tool requires an absolute `project` path pointing at an onboarded \
                 CodeSage project (one containing .codesage/index.db). \
                 Use find_symbol to locate definitions, find_references to trace callers \
                 and imports, list_dependencies for file-level dependency mapping, search \
                 for natural-language semantic code search, impact_analysis to estimate \
                 blast radius of a change, and export_context to bundle code for an LLM.",
        )
    }
}

#[tool_router]
impl CodeSageServer {
    #[tool(
        name = "find_symbol",
        description = "Find symbol definitions (functions, classes, methods, structs) by name. Returns file path, line number, and kind. Use partial names for broad search or qualified names (e.g. 'MyClass\\\\method' for PHP, 'MyClass.method' for Python) for exact match."
    )]
    fn find_symbol_tool(&self, Parameters(params): Parameters<FindSymbolParams>) -> CallToolResult {
        let kind = params.kind.as_deref().and_then(SymbolKind::parse);
        let req = FindSymbolRequest {
            name: params.name,
            kind,
        };
        render_with_kind(
            self.with_project_db(&params.project, |db| find_symbol(db, &req)),
            "find_symbol",
        )
    }

    #[tool(
        name = "find_references",
        description = "Find all references to a symbol across the codebase. Shows where a function, class, or module is called, imported, instantiated, or inherited."
    )]
    fn find_references_tool(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        let kind = params.kind.as_deref().and_then(ReferenceKind::parse);
        let req = FindReferencesRequest {
            symbol_name: params.name,
            kind,
        };
        render_with_kind(
            self.with_project_db(&params.project, |db| find_references(db, &req)),
            "find_references",
        )
    }

    #[tool(
        name = "list_dependencies",
        description = "List import/include dependencies for a file. Shows what the file imports and which other files import it."
    )]
    fn list_dependencies_tool(
        &self,
        Parameters(params): Parameters<ListDependenciesParams>,
    ) -> CallToolResult {
        render_with_kind(
            self.with_project_db(&params.project, |db| {
                list_dependencies(db, &params.file_path)
            }),
            "list_dependencies",
        )
    }

    #[tool(
        name = "search",
        description = "Semantic code search. Finds code chunks most similar to a natural language query using embedding-based similarity. Use for conceptual searches like 'error handling in authentication' or 'database connection pooling'."
    )]
    fn search_tool(&self, Parameters(params): Parameters<SearchParams>) -> CallToolResult {
        let languages = params
            .language
            .as_deref()
            .and_then(Language::parse)
            .map(|l| vec![l]);
        let req = SearchRequest {
            query: params.query,
            limit: params.limit,
            offset: params.offset,
            languages,
            paths: params.paths,
        };
        render_with_kind(
            self.with_project_search(&params.project, |db, emb, rr| search(db, emb, rr, &req)),
            "search",
        )
    }

    #[tool(
        name = "impact_analysis",
        description = "Estimate which files are affected by changing a symbol or file. Walks the reference graph up to `depth` hops, reports affected files ranked by distance and reference count. Use before making changes to understand blast radius."
    )]
    fn impact_analysis_tool(&self, Parameters(params): Parameters<ImpactParams>) -> CallToolResult {
        let req = ImpactRequest {
            target: ImpactTarget::from_hint(params.target, params.is_file),
            depth: params.depth.unwrap_or(2),
            source_only: params.source_only.unwrap_or(false),
        };
        render_with_kind(
            self.with_project_db(&params.project, |db| impact_analysis(db, &req)),
            "impact_analysis",
        )
    }

    #[tool(
        name = "export_context",
        description = "Build a curated context bundle for a query or symbol. Combines semantic search results, overlapping symbol definitions, and optionally caller/callee code. Output is a structured bundle ready for LLM consumption."
    )]
    fn export_context_tool(
        &self,
        Parameters(params): Parameters<ExportContextParams>,
    ) -> CallToolResult {
        let req = ExportRequest::from_target(
            params.target,
            params.is_symbol.unwrap_or(false),
            params.limit.unwrap_or(5),
            params.include_callers.unwrap_or(false),
            params.include_callees.unwrap_or(false),
        );
        render_with_kind(
            self.with_project_search(&params.project, |db, emb, rr| {
                export_context(db, emb, rr, &req)
            }),
            "export_context",
        )
    }

    #[tool(
        name = "find_coupling",
        description = "Files that historically change together with the given file, ranked by exponentially-decayed weight (τ=180d). Backed by git history. Use when planning a change to know which OTHER files (especially tests) tend to need updates too. Empty result means no co-change history yet — run `codesage git-index` if you haven't, or the file is too new to have signal."
    )]
    fn find_coupling_tool(&self, Parameters(params): Parameters<CouplingParams>) -> CallToolResult {
        let limit = params.limit.unwrap_or(10);
        let file_path = params.file_path.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| find_coupling(db, &file_path, limit)),
            "find_coupling",
        )
    }

    #[tool(
        name = "assess_risk",
        description = "Risk score for changing a file: combines churn percentile, fix ratio, blast radius (depth-2 reverse deps), historical coupling, and a test-gap signal. Output includes the decomposition and human-readable notes you can quote in PR descriptions or risk callouts. Use BEFORE writing a patch to calibrate caution and BEFORE submitting to flag concerns."
    )]
    fn assess_risk_tool(&self, Parameters(params): Parameters<RiskParams>) -> CallToolResult {
        let file_path = params.file_path.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| assess_risk(db, &file_path)),
            "assess_risk",
        )
    }

    #[tool(
        name = "assess_risk_diff",
        description = "Aggregate risk for a SET of files (the file list of a patch or PR). Returns per-file decomposition plus rollups: max_score, mean_score, max_risk_file, and lists of files in each risk category (test_gap, hotspot, fix-heavy, wide blast radius). Use BEFORE submitting a patch: if max_score is high or any test_gap_files exist, add tests, split the patch, or flag concerns. summary_notes are paste-ready for a PR description."
    )]
    fn assess_risk_diff_tool(
        &self,
        Parameters(params): Parameters<RiskDiffParams>,
    ) -> CallToolResult {
        let file_paths = params.file_paths.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| assess_risk_diff(db, &file_paths)),
            "assess_risk_diff",
        )
    }

    #[tool(
        name = "recommend_tests",
        description = "Tests an agent should run after editing the given files. Returns `primary` (sibling tests resolved by language convention — FooTest.php, foo.test.ts, test_foo.py, foo_test.go — high confidence, always run these) and `coupled` (tests that historically change with the input files via git co-change history — medium confidence, catches integration tests that don't follow naming conventions). Empty result means no test files in the index for these paths. Use AFTER making a change to know which subset of tests to actually run."
    )]
    fn recommend_tests_tool(
        &self,
        Parameters(params): Parameters<TestsForParams>,
    ) -> CallToolResult {
        let file_paths = params.file_paths.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| recommend_tests(db, &file_paths)),
            "recommend_tests",
        )
    }
}

pub async fn run_mcp_server() -> Result<()> {
    let server = CodeSageServer::new();
    let transport = rmcp::transport::io::stdio();
    let service = server
        .serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server stopped: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fat_string(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn cap_passes_through_when_under_budget() {
        let v = json!([{"name": "a"}, {"name": "b"}]);
        let out = cap_to_budget(v.clone(), "test");
        assert_eq!(out, v);
    }

    #[test]
    fn cap_truncates_top_level_array_when_over_budget() {
        // Each item is ~1100 chars; 50 items = ~55k chars, well over 32k budget.
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let out = cap_to_budget(Value::Array(items), "search");
        let obj = out.as_object().expect("wrapped as object");
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["kind"], json!("search"));
        assert_eq!(meta["total_results"], json!(50));
        let returned = meta["returned"].as_u64().unwrap() as usize;
        assert!(returned > 0 && returned < 50, "got {returned}");
        assert_eq!(obj["results"].as_array().unwrap().len(), returned);
    }

    #[test]
    fn cap_trims_largest_array_field_in_object() {
        // ContextBundle-like: small `primary` + huge `related`.
        let related: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let v = json!({
            "target_description": "test",
            "primary": [{"file_path": "a.rs", "content": "small"}],
            "related": related,
        });
        let out = cap_to_budget(v, "export_context");
        let obj = out.as_object().expect("still an object");
        assert_eq!(
            obj["primary"].as_array().unwrap().len(),
            1,
            "primary preserved"
        );
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["field"], json!("related"), "trimmed largest field");
        assert_eq!(meta["total_results"], json!(50));
        let returned = meta["returned"].as_u64().unwrap() as usize;
        assert!(returned > 0 && returned < 50);
        assert_eq!(obj["related"].as_array().unwrap().len(), returned);
    }

    #[test]
    fn cap_object_without_arrays_passes_through() {
        let v = json!({"a": "small", "b": 42});
        let out = cap_to_budget(v.clone(), "test");
        assert_eq!(out, v);
    }

    #[test]
    fn truncate_array_keeps_at_least_one_when_first_overflows() {
        let huge = json!({"blob": fat_string(100_000)});
        let small = json!({"blob": "x"});
        let kept = truncate_array(vec![huge.clone(), small.clone()], 10);
        assert_eq!(kept.len(), 1, "keep at least one rather than empty");
        assert_eq!(kept[0], huge);
    }

    #[test]
    fn truncate_array_keeps_prefix_that_fits() {
        let items: Vec<Value> = (0..10)
            .map(|i| json!({"i": i, "blob": fat_string(100)}))
            .collect();
        // Each item ~115 chars. Budget for 5 items = ~575 chars; allow some overhead.
        let kept = truncate_array(items, 600);
        assert!(
            (4..=6).contains(&kept.len()),
            "expected 4-6, got {}",
            kept.len()
        );
        // Prefix order preserved
        for (n, item) in kept.iter().enumerate() {
            assert_eq!(item["i"], json!(n));
        }
    }

    #[test]
    fn truncate_array_handles_empty() {
        let kept = truncate_array(vec![], 100);
        assert!(kept.is_empty());
    }
}
