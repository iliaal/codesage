use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_embed::reranker::Reranker;
use codesage_graph::{
    assess_risk, assess_risk_batch, assess_risk_diff, export_context, export_context_for_symbol,
    feature_bundle, find_coupling, find_references, find_symbol, impact_analysis,
    list_dependencies, recommend_tests, search, session_end, session_start,
};
use codesage_protocol::{
    ContextBundle, CouplingReport, DependencyEntry, ExportRequest, FeatureKind, FeatureListResults,
    FindReferencesRequest, FindReferencesResults, FindSymbolRequest, FindSymbolResults,
    ImpactAnalysisResults, ImpactRequest, ImpactTarget, Language, ReferenceKind, RiskAssessment,
    RiskBatchAssessment, RiskDiffAssessment, SearchRequest, SearchResults, SessionDiff,
    SessionSnapshot, SymbolKind, TestRecommendations,
};
use codesage_storage::Database;
use parking_lot::Mutex;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::schema_for_type, wrapper::Parameters},
    model::{CallToolResult, Content, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

const PROJECT_ARG_DESC: &str = "Absolute path to the project root. Must be an onboarded CodeSage project (contains .codesage/index.db).";

/// Accept integer numeric params from agents that occasionally JSON-encode
/// numbers as strings (`{"limit": "5"}` instead of `{"limit": 5}`). The
/// default `Option<usize>` serde derive rejects the string form with
/// `invalid type: string "5", expected usize` — a hard error at the MCP
/// protocol layer that leaves the caller guessing. Retrospective session
/// analysis (`bench/analyze-codesage-quality.py`) found this was 100% of
/// the `find_coupling` error results, so the fix applies across every
/// integer param: `limit`, `offset`, `depth`.
fn deser_optional_usize<'de, D>(d: D) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum UsizeOrString {
        U(usize),
        S(String),
    }

    match Option::<UsizeOrString>::deserialize(d)? {
        None => Ok(None),
        Some(UsizeOrString::U(n)) => Ok(Some(n)),
        Some(UsizeOrString::S(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed.parse::<usize>().map(Some).map_err(|e| {
                serde::de::Error::custom(format!(
                    "expected integer or integer-as-string, got {s:?}: {e}"
                ))
            })
        }
    }
}

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
    #[serde(default, deserialize_with = "deser_optional_usize")]
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
pub struct RiskBatchParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths to score individually. Returns one RiskAssessment per path, in input order. Use when you have a list of files (e.g. from impact analysis or coupling) and want each one's individual risk decomposition — saves the per-file MCP round-trip overhead vs N separate `assess_risk` calls. For patch-level aggregation (max/mean, summary_notes, cycles), use `assess_risk_diff` instead."
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
pub struct SessionParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Session identifier (alphanumerics, '-', '_', '.', max 128 chars). Use the same id for the matching session_start and session_end. Defaults to \"default\" when omitted."
    )]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImpactParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name or file path to analyze")]
    pub target: String,
    #[schemars(
        description = "Treat target as file path (auto-detected if path-like); pass false to force symbol interpretation"
    )]
    pub is_file: Option<bool>,
    #[schemars(description = "Recursion depth for transitive impact (default 2)")]
    #[serde(default, deserialize_with = "deser_optional_usize")]
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
    #[serde(default, deserialize_with = "deser_optional_usize")]
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
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    #[schemars(description = "Results offset for pagination")]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub offset: Option<usize>,
    #[schemars(description = "Filter by language: php, python, c, rust, javascript, typescript")]
    pub language: Option<String>,
    #[schemars(description = "Filter by file path glob patterns")]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListFeaturesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Filter by feature kind: cli-command, route, service, library, test-suite, config, job, unknown"
    )]
    pub kind: Option<String>,
    #[schemars(
        description = "Filter by language: php, python, c, cpp, rust, javascript, typescript, go"
    )]
    pub language: Option<String>,
    #[schemars(description = "Filter by tag substring (e.g. \"framework:laravel\", \"library\")")]
    pub tag: Option<String>,
    #[schemars(description = "Max results (default 100, 0 = no limit)")]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindFeatureParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to look up")]
    pub file_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FeatureBundleParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Feature id (e.g. feat_abc123) from `list_features` / `find_feature`"
    )]
    pub feature_id: String,
    #[schemars(
        description = "Include caller chunks for the feature's entry symbol (default false)"
    )]
    pub include_callers: Option<bool>,
    #[schemars(
        description = "Include callee chunks reached from the feature's entry symbol (default false)"
    )]
    pub include_callees: Option<bool>,
    #[schemars(description = "Max chunks per section (primary, related). Default 5.")]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
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
        let embedding_config = load_embedding_config(&codesage_dir.join("config.toml"));
        let state = ProjectState {
            db_path: db_path.clone(),
            embedding_config,
        };
        let newly_registered = {
            let mut guard = self.projects.lock();
            if guard.contains_key(&canonical) {
                false
            } else {
                guard.insert(canonical.clone(), state.clone());
                true
            }
        };
        // Drift telemetry: on first resolution of a project in this MCP
        // session, append one JSON line to `.codesage/drift.log`. Non-fatal —
        // telemetry errors stay in tracing so a drift write never blocks a
        // tool call.
        if newly_registered && let Err(e) = write_drift_log_for_project(&canonical, &db_path) {
            tracing::debug!(error = %e, "drift log append failed");
        }
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

    fn open_structural_db_for(&self, state: &ProjectState) -> Result<Database> {
        Database::open(&state.db_path)
    }

    fn open_context_db_for(&self, state: &ProjectState) -> Result<Database> {
        Database::open_for_existing_model(&state.db_path, &state.embedding_config.model)
    }

    /// Resolve project, open its DB, run `f` with the DB. Error handling funnel:
    /// each handler's body lives under this so the tool dispatch stays one-liner.
    fn with_project_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        f(&db)
    }

    /// Variant of `with_project_db` that also passes the canonical project
    /// root path. Used by tools like `session_start` that need to write
    /// alongside `.codesage/index.db` (e.g. `.codesage/sessions/<id>.json`).
    fn with_project_root_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Path, &Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        // db_path = <project_root>/.codesage/index.db; pop twice to recover root.
        let root = state
            .db_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("could not derive project root from db path"))?;
        f(root, &db)
    }

    fn with_project_context_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_context_db_for(&state)?;
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
            // MCP requires structuredContent to be a JSON object. Tools that
            // return bare arrays (find_symbol, find_references, search) get
            // wrapped in {"results": [...]} so Claude's validator accepts the
            // response. cap_to_budget already wraps over-budget arrays into
            // {"results": ..., "_meta": {...}}; this covers the under-budget
            // path so the shape is consistent regardless of size.
            let structured = match capped {
                serde_json::Value::Array(items) => serde_json::json!({ "results": items }),
                other => other,
            };
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            let mut result = CallToolResult::structured(structured);
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
    for mut item in items {
        let s = serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
        if used + s > budget_chars {
            if !kept.is_empty() {
                break;
            }
            // First item alone overflows: try to shrink its `content` field
            // before giving up. Without this, a single 50KB chunk blows past
            // the 32KB token budget. If the item has no `content` string, we
            // surrender and keep the oversized item — refusing to return
            // anything is worse than a slightly over-budget response.
            shrink_content_field(&mut item, budget_chars.saturating_sub(used));
            kept.push(item);
            break;
        }
        used += s;
        kept.push(item);
    }
    kept
}

/// Best-effort: if `item` is an object with a `content: String` field,
/// truncate that string so the serialized item fits roughly within
/// `budget_chars`. Marks the truncation visibly so an agent reading the
/// payload knows it's incomplete.
fn shrink_content_field(item: &mut serde_json::Value, budget_chars: usize) {
    let serde_json::Value::Object(map) = item else {
        return;
    };
    let Some(serde_json::Value::String(content)) = map.get_mut("content") else {
        return;
    };
    if content.len() <= budget_chars {
        return;
    }
    // Reserve a few hundred bytes for the rest of the JSON envelope.
    let target = budget_chars.saturating_sub(256);
    let cut = content
        .char_indices()
        .nth(target)
        .map(|(i, _)| i)
        .unwrap_or(target.min(content.len()));
    let mut shrunk = content[..cut].to_string();
    shrunk.push_str("\n…[truncated by MCP budget]");
    *content = shrunk;
}

/// Load the per-project embedding config for the MCP server.
///
/// MCP serves multiple projects through one process; a malformed
/// `.codesage/config.toml` in one project must not poison every tool call
/// against that project. Read or parse failures fall back to defaults with
/// a one-line warning so structural tools (`assess_risk`, `find_coupling`,
/// `find_symbol`, …) keep working. `search` will then fail at vec-table
/// lookup if the embedder defaults don't match the indexed model — a
/// narrower, clearer failure than a TOML parse error on every tool call.
///
/// The CLI path (`load_project_config` in `main.rs`) deliberately keeps
/// the loud-fail behavior: a user running `codesage index` interactively
/// wants to know their config is broken.
fn load_embedding_config(path: &Path) -> EmbeddingConfig {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return EmbeddingConfig::default();
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not read project config; falling back to embedding defaults",
            );
            return EmbeddingConfig::default();
        }
    };
    #[derive(serde::Deserialize)]
    struct Config {
        embedding: Option<EmbeddingConfig>,
    }
    match toml::from_str::<Config>(&content) {
        Ok(parsed) => parsed.embedding.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not parse project config; falling back to embedding defaults",
            );
            EmbeddingConfig::default()
        }
    }
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
        description = "Find symbol definitions (functions, classes, methods, structs, traits, enums) by name. Returns exact file path, line number, and kind. **Prefer this over Grep/ripgrep for any code-identifier lookup** — one call returns the definition, while grepping for a function name often produces many false hits (call sites, comments, other namespaces) that cost extra Read calls to disambiguate. Use partial names for broad search or qualified names ('MyClass\\\\method' for PHP, 'MyClass.method' for Python) for exact match. When present, `rationale[]` carries `WHY:` / `NOTE:` / `IMPORTANT:` / `FIXME:` / `HACK:` / `XXX:` / `TODO:` comments attached to the definition — read these before refactoring or renaming so the agent doesn't drop a constraint the author wrote down. Currently extracted for Rust and Python.",
        output_schema = schema_for_type::<FindSymbolResults>()
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
        description = "Find all references to a symbol across the codebase. **Prefer this over Grep for 'where is X called / imported / instantiated?'** — returns structured {file, line, kind} rows with the reference type (call/import/inheritance/instantiation/type_hint) already classified, instead of raw grep hits that mix definitions, comments, and string literals together.",
        output_schema = schema_for_type::<FindReferencesResults>()
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
        description = "List immediate (single-hop) import/include dependencies for a file: what THIS file imports and which other files import THIS file. Use when the question is 'what does this file depend on?' or 'who imports this file?'. For 'what breaks if I change this?' use `impact_analysis` (walks multiple hops, ranks by distance). For per-symbol callers/callees use `find_references` (per-symbol grain, not per-file).",
        output_schema = schema_for_type::<DependencyEntry>()
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
        description = "Semantic code search (embedding-based + cross-encoder reranking). **Prefer this over Grep when you don't know the exact symbol name** — useful for queries like 'where is auth handled', 'error handling in the session pipeline', 'database connection pooling', 'where do we validate inputs'. Grep needs the literal token already; `search` lets the agent ask by intent. For exact identifier lookups with a known name, use `find_symbol` or `find_references` instead.",
        output_schema = schema_for_type::<SearchResults>()
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
        description = "Estimate which files are affected by changing a symbol or file. Walks the **reverse** reference graph up to `depth` hops (default 2) — i.e., callers/importers of the target and transitively their callers/importers — reports affected files ranked by distance and reference count. **Multi-hop blast radius from the target outward to its dependents.** Returns `[]` for leaf files nothing imports/calls. Does NOT include same-file symbols, does NOT include what the target itself depends on (use `list_dependencies` for the target's own forward dependencies). Use BEFORE making changes to know what else needs review/testing. For single-hop importer/importee of one file use `list_dependencies`; for raw call sites of a specific symbol use `find_references`.",
        output_schema = schema_for_type::<ImpactAnalysisResults>()
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
        description = "Build a curated context bundle for a free-form **query** or a single **symbol**: semantic search results, overlapping symbol definitions, and optionally caller/callee code, all wrapped as a structured bundle ready for LLM consumption. Use when the anchor is a phrase ('error handling in the parser') or one named symbol. For an already-mapped feature slice (entrypoint + owned files + tests + context already resolved), use `feature_bundle` instead — that anchors on `feature_id` and avoids re-running semantic search. Symbol entries inside the bundle carry `rationale[]` when the author left `WHY:` / `NOTE:` / `IMPORTANT:` / `FIXME:` / `HACK:` / `XXX:` / `TODO:` comments — preserve these in any synthesis the agent performs from the bundle. Currently extracted for Rust and Python.",
        output_schema = schema_for_type::<ContextBundle>()
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
        if let Some(sym_name) = req.symbol.clone() {
            return render_with_kind(
                self.with_project_context_db(&params.project, |db| {
                    export_context_for_symbol(db, &sym_name, &req)
                }),
                "export_context",
            );
        }
        render_with_kind(
            self.with_project_search(&params.project, |db, emb, rr| {
                export_context(db, emb, rr, &req)
            }),
            "export_context",
        )
    }

    #[tool(
        name = "find_coupling",
        description = "Files that historically change together with the given file, ranked by exponentially-decayed weight (τ=180d). Backed by git history. Use when planning a change to know which OTHER files (especially tests) tend to need updates too. Response is `{coupled: [...], file_indexed: bool, file_commits: u32, note?: string}` — read `coupled` for the ranked list. When `coupled` is empty, `note` disambiguates: file never indexed vs. file has history but no pair above the min-count=3 threshold vs. path shape mismatch. Index into `.coupled`, not the response directly.",
        output_schema = schema_for_type::<CouplingReport>()
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
        description = "Risk score for changing a file: combines churn percentile, fix ratio, blast radius (depth-2 reverse deps), historical coupling, a test-gap signal, and import-cycle membership. Response carries `in_cycle` / `cycle_size` / `cycle_files` so the agent can name the other members of the cycle. Output includes the decomposition and human-readable notes you can quote in PR descriptions or risk callouts. Use BEFORE writing a patch to calibrate caution and BEFORE submitting to flag concerns.",
        output_schema = schema_for_type::<RiskAssessment>()
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
        description = "Aggregate risk for a SET of files (the file list of a patch or PR). Returns per-file decomposition plus rollups: max_score, mean_score, max_risk_file, and lists of files in each risk category (test_gap, hotspot, fix-heavy, wide blast radius). Use BEFORE submitting a patch: if max_score is high or any test_gap_files exist, add tests, split the patch, or flag concerns. summary_notes are paste-ready for a PR description. On large patches that touch ≥5 files from one directory, per-file entries for that directory move from `files` into a `clustered_directories[]` entry (top-3 by score preserved in detail, rest by name); rollup arrays still list every clustered file by name, so cross-referencing still works. `cycles_touching_patch[]` lists import cycles (files that mutually depend via import/include/inheritance/trait_use) that include at least one patch file, each with `members`, `size`, and `max_churn_file` (best refactor target). Honest caveat: we can't distinguish cycles the patch introduced from cycles that already existed; phrase PR feedback as 'this patch touches an existing cycle' unless you've verified the base branch.",
        output_schema = schema_for_type::<RiskDiffAssessment>()
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
        name = "assess_risk_batch",
        description = "Risk score for EACH of N files, returned per-file with no patch-level aggregation. Use when you have a list of files (impact analysis output, coupling neighbours, the files of a feature you're touching one-by-one) and want each individual score — cuts the per-file MCP round-trip overhead vs calling `assess_risk` N times. Each entry is a full RiskAssessment with the same shape as `assess_risk`. The response also includes a top-level `_legend` short-code map: when ≥3 files in the batch share a categorical note (test-gap, no-git-history), per-file `notes[]` entries are aliased to short codes (e.g. `\"T\"`, `\"NG\"`) and the legend resolves them. For patch-level aggregation (max/mean, hotspot/test-gap rollups, cycles), use `assess_risk_diff` instead — they answer different questions.",
        output_schema = schema_for_type::<RiskBatchAssessment>()
    )]
    fn assess_risk_batch_tool(
        &self,
        Parameters(params): Parameters<RiskBatchParams>,
    ) -> CallToolResult {
        let file_paths = params.file_paths.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| assess_risk_batch(db, &file_paths)),
            "assess_risk_batch",
        )
    }

    #[tool(
        name = "recommend_tests",
        description = "Tests an agent should run after editing the given files. Returns `primary` (sibling tests resolved by language convention — FooTest.php, foo.test.ts, test_foo.py, foo_test.go — high confidence, always run these) and `coupled` (tests that historically change with the input files via git co-change history — medium confidence, catches integration tests that don't follow naming conventions). Empty result means no test files in the index for these paths. Use AFTER making a change to know which subset of tests to actually run.",
        output_schema = schema_for_type::<TestRecommendations>()
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

    #[tool(
        name = "session_start",
        description = "Snapshot the project's structural state at the START of an editing session. Persists file count, symbol count, the full file list, all import cycles, and the top-50 highest-risk files (with their scores) to `.codesage/sessions/<session_id>.json`. Pair with `session_end` using the same `session_id` to detect new cycles, removed/added files, or risk regressions on hot files introduced during the session. `session_id` defaults to \"default\" — use a distinct id when running multiple parallel sessions. Re-running `session_start` overwrites the snapshot (useful for resetting a baseline mid-session).",
        output_schema = schema_for_type::<SessionSnapshot>()
    )]
    fn session_start_tool(&self, Parameters(params): Parameters<SessionParams>) -> CallToolResult {
        let session_id = params
            .session_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        render_with_kind(
            self.with_project_root_db(&params.project, |root, db| {
                session_start(root, db, &session_id)
            }),
            "session_start",
        )
    }

    #[tool(
        name = "list_features",
        description = "List feature slices in the project, optionally filtered by kind, language, or tag. A feature is a behavior-keyed bundle (entrypoint + owned files + context + tests + trust boundaries) — e.g. \"Laravel route POST /api/login\", \"Rust binary `codesage`\", \"php-src extension `iconv`\", \"CMake binary `myapp`\". Use this to discover the agent-facing surface area of the project before deep-diving into a specific slice. Pair with `find_feature` (file → features) and `assess_risk` (per-file scoring inside a feature).",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    fn list_features_tool(
        &self,
        Parameters(params): Parameters<ListFeaturesParams>,
    ) -> CallToolResult {
        let kind = params.kind.as_deref().and_then(FeatureKind::parse);
        let language = params.language.as_deref().and_then(Language::parse);
        let tag = params.tag.clone();
        let limit = params.limit.unwrap_or(100);
        render_with_kind(
            self.with_project_db(&params.project, |db| {
                db.list_features(kind, language, tag.as_deref(), limit)
            }),
            "list_features",
        )
    }

    #[tool(
        name = "find_feature",
        description = "Features that include the given file in any role (entry, owned, context, or test). Use to answer \"what feature owns src/auth/login.php?\" — returns the matching feature records with their full file lists, tags, and trust boundaries. Empty result means no mapped feature claims this file (common: not every file belongs to a feature slice).",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    fn find_feature_tool(
        &self,
        Parameters(params): Parameters<FindFeatureParams>,
    ) -> CallToolResult {
        let file = params.file_path.clone();
        render_with_kind(
            self.with_project_db(&params.project, |db| db.features_for_file(&file)),
            "find_feature",
        )
    }

    #[tool(
        name = "feature_bundle",
        description = "Curated code bundle for one feature_id. Same shape as `export_context` but anchored on the feature's already-resolved file list (entry + owned + tests + context) instead of semantic search results. `primary[]` carries chunks from owned/entry files, `related[]` carries tests and context. Set `include_callers` / `include_callees` to also expand the entry symbol's callers/callees into `related[]` (reuses the symbol graph used by `export_context`). Use after `list_features` / `find_feature` to get all the code an agent needs to review or modify the slice in one MCP call — avoids fan-out Read calls per file. Empty bundle with `target_description` ending `(not found)` means the feature_id doesn't exist; empty bundle with non-empty title means the feature exists but no files have been semantically indexed yet (run `codesage index`).",
        output_schema = schema_for_type::<ContextBundle>()
    )]
    fn feature_bundle_tool(
        &self,
        Parameters(params): Parameters<FeatureBundleParams>,
    ) -> CallToolResult {
        let feature_id = params.feature_id.clone();
        let include_callers = params.include_callers.unwrap_or(false);
        let include_callees = params.include_callees.unwrap_or(false);
        let limit = params.limit.unwrap_or(5);
        // Use the context DB (binds to the configured embedding model's
        // chunk table) so `primary`/`related` resolve real chunks. The
        // structural-only db variant points at the default chunk table
        // and returns empty content on projects using a non-default
        // model (php-src uses jina v2 768-dim, MiniLM is the default).
        render_with_kind(
            self.with_project_context_db(&params.project, |db| {
                feature_bundle(db, &feature_id, include_callers, include_callees, limit)
            }),
            "feature_bundle",
        )
    }

    #[tool(
        name = "session_end",
        description = "Diff the current structural state against the snapshot saved by `session_start` (matched by `session_id`, default \"default\"). Returns `pass: bool` (true when no new import cycles were introduced AND no top-risk file regressed by ≥ 0.10), plus `new_cycles`, `resolved_cycles`, `risk_regressions` (per-file before/after/delta), `new_files`, `removed_files`, and `summary_notes` ready to paste into a PR description. Errors when the snapshot file is missing — call `session_start` first. Snapshot file is left in place after the diff so the same id can be re-diffed.",
        output_schema = schema_for_type::<SessionDiff>()
    )]
    fn session_end_tool(&self, Parameters(params): Parameters<SessionParams>) -> CallToolResult {
        let session_id = params
            .session_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        render_with_kind(
            self.with_project_root_db(&params.project, |root, db| {
                session_end(root, db, &session_id)
            }),
            "session_end",
        )
    }
}

/// Opens the project DB read-only-enough to compute a drift snapshot and
/// append one JSON line to `.codesage/drift.log`. Returns quickly — the DB
/// handle drops at the end of this call. Failures propagate so the caller
/// can log them; drift telemetry never kills a tool call.
fn write_drift_log_for_project(project_root: &Path, db_path: &Path) -> Result<()> {
    let db = Database::open(db_path)?;
    let report = crate::drift::check_drift(project_root, &db);
    crate::drift::append_drift_log(project_root, ".codesage", &report)?;
    Ok(())
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
    fn coupling_params_accept_int_limit() {
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": 5,
        }))
        .unwrap();
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn coupling_params_accept_stringy_limit() {
        // Session logs showed 100% of find_coupling MCP -32602 errors were
        // agents sending `"limit": "5"` as a JSON string. Must parse.
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": "5",
        }))
        .unwrap();
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn coupling_params_accept_missing_limit() {
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
        }))
        .unwrap();
        assert_eq!(p.limit, None);
    }

    #[test]
    fn coupling_params_reject_non_numeric_string() {
        let r: Result<CouplingParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": "not-a-number",
        }));
        assert!(r.is_err(), "non-numeric string must still error");
        // Error should name the offending value rather than be a generic
        // "expected usize" so the agent can fix its request.
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("not-a-number"),
            "error must quote offending value, got: {msg}"
        );
    }

    #[test]
    fn impact_params_coerce_depth_string() {
        let p: ImpactParams = serde_json::from_value(json!({
            "project": "/p",
            "target": "Foo",
            "depth": "3",
        }))
        .unwrap();
        assert_eq!(p.depth, Some(3));
    }

    #[test]
    fn search_params_coerce_limit_and_offset_strings() {
        let p: SearchParams = serde_json::from_value(json!({
            "project": "/p",
            "query": "auth",
            "limit": "10",
            "offset": "20",
        }))
        .unwrap();
        assert_eq!(p.limit, Some(10));
        assert_eq!(p.offset, Some(20));
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
    fn truncate_array_shrinks_oversized_first_content_field() {
        // Regression: when the first item has a `content: String` that
        // alone exceeds budget, shrink_content_field must trim it instead
        // of letting the whole 50KB blob through verbatim.
        let huge = json!({"file_path": "src/big.rs", "content": fat_string(50_000)});
        let kept = truncate_array(vec![huge], 4_000);
        assert_eq!(kept.len(), 1);
        let s = serde_json::to_string(&kept[0]).unwrap();
        assert!(
            s.len() < 5_000,
            "shrunk item still oversized: {} bytes",
            s.len()
        );
        let content = kept[0].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(
            content.contains("[truncated by MCP budget]"),
            "expected truncation marker, got tail: …{}",
            &content[content.len().saturating_sub(80)..]
        );
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

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codesage-mcp-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn malformed_config_falls_back_to_defaults() {
        // One bad project must not poison every tool call against it. The
        // MCP server keeps serving structural tools; only `search` will
        // fail at vec-table lookup if defaults don't match the indexed
        // model.
        let path = write_tmp("malformed", "embedding = { this is not valid toml ===");
        let config = load_embedding_config(&path);
        assert_eq!(config.model, EmbeddingConfig::default().model);
    }

    #[test]
    fn missing_config_returns_defaults() {
        let path = std::env::temp_dir().join(format!(
            "codesage-mcp-test-missing-{}.toml",
            std::process::id()
        ));
        // ensure path doesn't exist
        let _ = std::fs::remove_file(&path);
        let config = load_embedding_config(&path);
        assert_eq!(config.model, EmbeddingConfig::default().model);
    }

    #[test]
    fn well_formed_config_parses() {
        let path = write_tmp(
            "valid",
            "[embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\ndevice = \"cpu\"\n",
        );
        let config = load_embedding_config(&path);
        assert_eq!(config.model, "sentence-transformers/all-MiniLM-L6-v2");
        assert_eq!(config.device, "cpu");
    }

    #[test]
    fn config_without_embedding_section_returns_defaults() {
        // A valid TOML that just doesn't have an `[embedding]` table — the
        // file is fine, the embedding section is absent, defaults apply.
        let path = write_tmp("no-embedding", "[project]\nname = \"foo\"\n");
        let config = load_embedding_config(&path);
        assert_eq!(config.model, EmbeddingConfig::default().model);
    }

    #[test]
    fn structural_project_db_does_not_load_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "[embedding]\nmodel = \"codesage-test/does-not-exist\"\ndevice = \"cpu\"\n",
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        Database::open(&db_path).unwrap();

        let server = CodeSageServer::new();
        let count = server
            .with_project_db(root.to_str().unwrap(), |db| db.file_count())
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn symbol_export_uses_existing_chunks_without_loading_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        let model = "codesage-test/does-not-exist";
        std::fs::write(
            codesage_dir.join("config.toml"),
            format!("[embedding]\nmodel = \"{model}\"\ndevice = \"cpu\"\n"),
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        let db =
            Database::open_for_model(&db_path, model, codesage_storage::db::DEFAULT_EMBEDDING_DIM)
                .unwrap();
        let file_id = db
            .upsert_file(&codesage_protocol::FileInfo {
                path: "src/lib.rs".to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: "h1".to_string(),
            })
            .unwrap();
        db.insert_symbols(
            file_id,
            &[codesage_protocol::Symbol {
                name: "target".to_string(),
                qualified_name: "target".to_string(),
                kind: SymbolKind::Function,
                file_path: "src/lib.rs".to_string(),
                line_start: 1,
                line_end: 1,
                col_start: 0,
                col_end: 0,
                rationale: vec![],
            }],
        )
        .unwrap();
        let embedding = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "src/lib.rs",
            "rust",
            &[("fn target() {}", 1, 1, embedding.as_slice())],
        )
        .unwrap();

        let server = CodeSageServer::new();
        let result = server.export_context_tool(Parameters(ExportContextParams {
            project: root.to_str().unwrap().to_string(),
            target: "target".to_string(),
            is_symbol: Some(true),
            limit: Some(5),
            include_callers: Some(false),
            include_callees: Some(false),
        }));

        assert_ne!(result.is_error, Some(true));
        let value = result.structured_content.expect("structured content");
        assert_eq!(value["symbol_definitions"].as_array().unwrap().len(), 1);
        assert_eq!(value["primary"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn render_wraps_under_budget_array_as_results_object() {
        // Tools like find_symbol return Result<Vec<T>>. Without the wrap,
        // structuredContent ships as a bare JSON array and Claude's MCP
        // client rejects it with `expected record, received array`.
        let r: Result<Vec<Value>> = Ok(vec![json!({"name": "foo"}), json!({"name": "bar"})]);
        let result = render_with_kind(r, "find_symbol");
        assert_ne!(result.is_error, Some(true));
        let value = result.structured_content.expect("structured content");
        let obj = value
            .as_object()
            .expect("structuredContent must be an object");
        let items = obj["results"].as_array().expect("results is an array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["name"], json!("foo"));
        // No truncation under budget: _meta must be absent.
        assert!(!obj.contains_key("_meta"));
    }

    #[test]
    fn render_passes_object_through_unchanged() {
        // list_dependencies returns a struct (object); the wrap must not
        // mutate it into a nested {"results": {...}}.
        let r: Result<Value> = Ok(json!({"file_path": "a.rs", "imports": ["b.rs"]}));
        let result = render_with_kind(r, "list_dependencies");
        let value = result.structured_content.expect("structured content");
        let obj = value.as_object().expect("object preserved");
        assert_eq!(obj["file_path"], json!("a.rs"));
        assert!(!obj.contains_key("results"));
    }

    #[test]
    fn render_wraps_empty_array() {
        // Empty array is still an array; must wrap so the response stays a
        // valid record (empty find_symbol / find_references is the common
        // miss case and would otherwise ship `[]`).
        let r: Result<Vec<Value>> = Ok(vec![]);
        let result = render_with_kind(r, "find_symbol");
        let value = result.structured_content.expect("structured content");
        let obj = value
            .as_object()
            .expect("structuredContent must be an object");
        assert_eq!(obj["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_over_budget_array_keeps_results_and_meta_shape() {
        // cap_to_budget already wraps oversized arrays as {results, _meta}.
        // Verify render_with_kind passes that wrapped object through without
        // double-nesting it.
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let r: Result<Vec<Value>> = Ok(items);
        let result = render_with_kind(r, "search");
        let value = result.structured_content.expect("structured content");
        let obj = value
            .as_object()
            .expect("structuredContent must be an object");
        assert!(obj.contains_key("results"));
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["kind"], json!("search"));
        assert_eq!(meta["total_results"], json!(50));
        // No double-wrapping: results sits directly under the top object.
        assert!(obj["results"].is_array());
    }

    #[test]
    fn render_error_preserves_is_error() {
        let r: Result<Vec<Value>> = Err(anyhow::anyhow!("bad path"));
        let result = render_with_kind(r, "find_symbol");
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }

    /// Every registered MCP tool must carry a valid output schema. Catches
    /// the regression where a tool ships without `output_schema = ...` (then
    /// agents have to guess the response shape) and where the schema's root
    /// is not a JSON object (which the MCP spec requires; rmcp rejects it
    /// at registration time but the assertion here makes the contract
    /// explicit in test output).
    #[test]
    fn every_tool_advertises_an_output_schema() {
        let server = CodeSageServer::new();
        let tools = server.tool_router.list_all();
        assert!(!tools.is_empty(), "router should expose at least one tool");
        for tool in &tools {
            let schema = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing output_schema", tool.name));
            let root_type = schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                root_type,
                Some("object"),
                "tool `{}` output schema root must be `object`, got {:?}",
                tool.name,
                root_type
            );
            assert!(
                schema.contains_key("properties")
                    || schema.contains_key("$ref")
                    || schema.contains_key("$defs"),
                "tool `{}` output schema has no properties/$ref/$defs",
                tool.name
            );
        }
    }
}
