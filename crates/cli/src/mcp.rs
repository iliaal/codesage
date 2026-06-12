use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    ImpactAnalysisResults, ImpactRequest, ImpactTarget, Language, ProjectOverview, ReferenceKind,
    ReviewRehearsal, RiskAssessment, RiskBatchAssessment, RiskDiffAssessment, SearchRequest,
    SearchResults, SessionDiff, SessionSnapshot, SymbolKind, TestRecommendations,
};
use codesage_storage::Database;
use parking_lot::Mutex;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::schema_for_type, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerInfo},
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
    #[schemars(
        description = "Keep only features whose entry/owned/context files changed since this git ref (e.g. \"main\", \"HEAD~5\"). Uses `git diff <ref>...HEAD`; errors if the ref is unknown."
    )]
    pub since: Option<String>,
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProjectOverviewParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReviewRehearsalParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths in the patch / working-tree change set (typically `git diff --name-only`)"
    )]
    pub file_paths: Vec<String>,
}

#[derive(Clone)]
struct ProjectState {
    db_path: PathBuf,
    embedding_config: EmbeddingConfig,
    embedding_config_error: Option<String>,
}

#[derive(Debug)]
struct LoadedEmbeddingConfig {
    config: EmbeddingConfig,
    semantic_error: Option<String>,
}

/// One model "slot" per key — the outer mutex serializes the cold load
/// for that key, the inner option holds the loaded model once init
/// succeeds. Concurrent callers for the same key wait on the per-key
/// mutex; callers for different keys run in parallel because they
/// hold different slots. This is the fix for CR-001: previously
/// `get_or_load_*` checked the map, dropped the lock, called `new()`,
/// then raced to insert — two concurrent cold misses for the same
/// model loaded two ORT sessions and the loser was thrown away.
type ModelSlot<T> = Arc<Mutex<Option<Arc<Mutex<T>>>>>;

/// Find or create the slot for `key` and, if not yet populated, run
/// `load()` under the slot lock. Returns the shared `Arc<Mutex<T>>`
/// either way. The map lock is held only long enough to find-or-insert
/// the slot; the loader runs while only the per-key slot lock is held,
/// so concurrent calls for *different* keys never wait on each other.
///
/// The CR-001 race was `check map → drop → load → insert`: two threads
/// hitting the same cold key both ran `load()` and the loser's value
/// got dropped. This helper closes that window — for a single key, the
/// first thread to reach the slot lock runs `load()` exactly once; the
/// rest read the populated `Some(arc)` and return immediately.
fn get_or_load_slot<T, F>(
    map: &Mutex<HashMap<String, ModelSlot<T>>>,
    key: String,
    load: F,
) -> Result<Arc<Mutex<T>>>
where
    F: FnOnce() -> Result<T>,
{
    let slot = {
        let mut guard = map.lock();
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut slot_guard = slot.lock();
    if let Some(arc) = slot_guard.as_ref() {
        return Ok(arc.clone());
    }
    let value = load()?;
    let arc = Arc::new(Mutex::new(value));
    *slot_guard = Some(arc.clone());
    Ok(arc)
}

pub(crate) struct CodeSageServerState {
    projects: Mutex<HashMap<PathBuf, ProjectState>>,
    /// Fast-path cache keyed by the raw `project` arg string. Agents pass the
    /// same literal absolute path on every call, so this lets `resolve_project`
    /// skip `canonicalize()`'s per-component lstat on the hot path. `projects`
    /// (keyed by canonical path) stays the source of truth and dedupes distinct
    /// spellings of the same root.
    resolved: Mutex<HashMap<String, ProjectState>>,
    embedders: Mutex<HashMap<String, ModelSlot<Embedder>>>,
    rerankers: Mutex<HashMap<String, ModelSlot<Reranker>>>,
    /// Live filesystem watchers, one per project, keyed by canonical root.
    /// Spawned lazily on first tool call for a project (see
    /// [`CodeSageServer::ensure_watcher`]) and reaped on daemon shutdown.
    watchers: Mutex<HashMap<PathBuf, WatcherEntry>>,
}

/// Handle to a per-project watcher thread. `alive` flips to false when the
/// thread exits (idle timeout, disabled marker, error), so `ensure_watcher`
/// can tell a dead entry from a running one and respawn.
struct WatcherEntry {
    shutdown: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

impl CodeSageServerState {
    pub(crate) fn new() -> Self {
        Self {
            projects: Mutex::new(HashMap::new()),
            resolved: Mutex::new(HashMap::new()),
            embedders: Mutex::new(HashMap::new()),
            rerankers: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
        }
    }

    /// Signal every live watcher to drain and exit. Called from the daemon's
    /// shutdown path. Threads notice within one poll interval; we don't join
    /// (the process exit reaps any straggler), so this never blocks shutdown.
    pub(crate) fn shutdown_all_watchers(&self) {
        let mut guard = self.watchers.lock();
        for (root, entry) in guard.iter() {
            entry.shutdown.store(true, Ordering::SeqCst);
            tracing::info!(root = %root.display(), "signalling watcher shutdown");
        }
        guard.clear();
    }
}

#[derive(Clone)]
pub struct CodeSageServer {
    state: Arc<CodeSageServerState>,
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
        Self::with_state(Arc::new(CodeSageServerState::new()))
    }

    pub(crate) fn with_state(state: Arc<CodeSageServerState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a blocking tool-handler body off the tokio runtime threads.
    ///
    /// Every handler does blocking work — SQLite, ONNX inference, and on a cold
    /// miss a model load that includes a (network) HuggingFace download. Run
    /// directly on a runtime worker, enough concurrent calls block every worker
    /// and the daemon stops answering even `initialize`/`ping` (CR-001).
    /// Offloading to the blocking pool keeps the async workers free.
    ///
    /// `spawn_blocking` also gives a panic boundary: rmcp dispatches tool calls
    /// with no `catch_unwind`, so a panic in a handler is otherwise silently
    /// swallowed and the client hangs forever waiting for a reply that never
    /// comes (CR-003). Here a panic surfaces as a `JoinError` we turn into an
    /// error result the client actually receives.
    async fn blocking<F>(&self, f: F) -> CallToolResult
    where
        F: FnOnce(&Self) -> CallToolResult + Send + 'static,
    {
        // Cheap: state is an Arc, the tool_router is an Arc-backed map clone.
        let this = self.clone();
        match tokio::task::spawn_blocking(move || f(&this)).await {
            Ok(result) => result,
            Err(join_err) => {
                tracing::error!(error = %join_err, "MCP tool handler panicked");
                CallToolResult::error(vec![Content::text(format!(
                    "internal error: the tool handler panicked ({join_err}); see the daemon log"
                ))])
            }
        }
    }

    fn resolve_project(&self, project: &str) -> Result<ProjectState> {
        let state = self.resolve_project_inner(project)?;
        // Ensure a live watcher on every resolution (cheap when one is already
        // running) so an idle-exited watcher respawns the next time an agent
        // touches the project. Root is `<...>/.codesage/index.db` → two parents.
        if let Some(root) = state.db_path.parent().and_then(|p| p.parent()) {
            self.ensure_watcher(root, &state);
        }
        Ok(state)
    }

    fn resolve_project_inner(&self, project: &str) -> Result<ProjectState> {
        // Fast path: same raw arg string seen before — skip canonicalize().
        {
            let guard = self.state.resolved.lock();
            if let Some(state) = guard.get(project) {
                return Ok(state.clone());
            }
        }
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
            let guard = self.state.projects.lock();
            if let Some(state) = guard.get(&canonical) {
                let state = state.clone();
                drop(guard);
                self.state
                    .resolved
                    .lock()
                    .insert(project.to_string(), state.clone());
                return Ok(state);
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
            embedding_config: embedding_config.config,
            embedding_config_error: embedding_config.semantic_error,
        };
        let newly_registered = {
            let mut guard = self.state.projects.lock();
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
        self.state
            .resolved
            .lock()
            .insert(project.to_string(), state.clone());
        Ok(state)
    }

    /// Ensure a live filesystem watcher is running for `root`, spawning one if
    /// absent and the project hasn't opted out. The watcher reuses the daemon's
    /// pooled embedder (no extra model load) and self-exits after idle; this
    /// just guarantees one exists. Errors are logged, never propagated.
    fn ensure_watcher(&self, root: &Path, state: &ProjectState) {
        // Hot path: a live watcher already exists. Just a lock + atomic load,
        // no config I/O — this runs on every tool call.
        {
            let watchers = self.state.watchers.lock();
            if let Some(entry) = watchers.get(root)
                && entry.alive.load(Ordering::SeqCst)
            {
                return;
            }
        }

        // (Re)spawn path: load config and honor opt-out / disabled marker.
        let project_config = crate::load_project_config(root).ok().unwrap_or_default();
        let config_watch = project_config.index.as_ref().and_then(|i| i.watch);
        if !crate::statewatcher::watch_enabled(root, config_watch) {
            return;
        }

        let exclude_patterns = crate::get_exclude_patterns(&project_config);

        // Hand the watcher the daemon's pooled embedder, resolved lazily so an
        // idle watcher never forces a model load. `None` = structural-only.
        let embedder: Option<crate::statewatcher::EmbedderProvider> =
            if state.embedding_config.model.is_empty() || state.embedding_config_error.is_some() {
                None
            } else {
                let server = self.clone();
                let cfg = state.embedding_config.clone();
                Some(Arc::new(move || server.get_or_load_embedder(&cfg)))
            };

        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let watcher_config = crate::statewatcher::StateWatcherConfig {
            project_root: root.to_path_buf(),
            db_path: state.db_path.clone(),
            embed_config: state.embedding_config.clone(),
            exclude_patterns,
            debounce_ms: crate::statewatcher::resolve_debounce_ms(),
            idle_timeout: crate::statewatcher::resolve_idle_timeout(),
            mode: crate::statewatcher::WatcherMode::Daemon,
            embedder,
            shutdown: shutdown.clone(),
        };

        let alive_clone = alive.clone();
        let root_disp = root.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("cs-watch".to_string())
            .spawn(move || {
                if let Err(e) = crate::statewatcher::run_statewatcher(watcher_config) {
                    tracing::error!(error = %e, root = %root_disp.display(), "watcher exited with error");
                }
                alive_clone.store(false, Ordering::SeqCst);
            });

        match spawned {
            Ok(_join) => {
                self.state
                    .watchers
                    .lock()
                    .insert(root.to_path_buf(), WatcherEntry { shutdown, alive });
                tracing::info!(root = %root.display(), "live watcher started");
            }
            Err(e) => {
                tracing::warn!(error = %e, root = %root.display(), "failed to spawn watcher thread");
            }
        }
    }

    fn get_or_load_embedder(&self, config: &EmbeddingConfig) -> Result<Arc<Mutex<Embedder>>> {
        let batch_size = config.effective_batch_size().map_err(anyhow::Error::msg)?;
        let key = format!("{}|{}|{}", config.model, config.device, batch_size.get());
        get_or_load_slot(&self.state.embedders, key, || {
            Embedder::new(config).with_context(|| {
                format!(
                    "loading embedding model '{}' on device '{}'",
                    config.model, config.device
                )
            })
        })
    }

    fn get_or_load_reranker(
        &self,
        reranker_model: &str,
        device: &str,
    ) -> Result<Arc<Mutex<Reranker>>> {
        let key = format!("{}|{}", reranker_model, device);
        get_or_load_slot(&self.state.rerankers, key, || {
            Reranker::new(reranker_model, device).with_context(|| {
                format!("loading reranker model '{reranker_model}' on device '{device}'")
            })
        })
    }

    fn semantic_embedding_config<'a>(
        &self,
        state: &'a ProjectState,
    ) -> Result<&'a EmbeddingConfig> {
        if let Some(error) = &state.embedding_config_error {
            bail!("{error}");
        }
        Ok(&state.embedding_config)
    }

    fn open_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let embedder = embedder_arc.lock();
        Database::open_for_model(&state.db_path, &config.model, embedder.dim())
    }

    fn open_structural_db_for(&self, state: &ProjectState) -> Result<Database> {
        Database::open(&state.db_path)
    }

    fn open_context_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        Database::open_for_existing_model(&state.db_path, &config.model)
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

    /// Char budget for a context-bundle response, sized to the project's
    /// indexed file count (see [`mcp_bundle_token_budget`]). Falls back to a
    /// mid-tier default — still honoring the `CODESAGE_BUNDLE_TOKEN_BUDGET`
    /// override — when the file count can't be read.
    fn bundle_budget_chars(&self, project: &str) -> usize {
        let tokens = match self.with_project_db(project, |db| db.file_count()) {
            Ok(count) => mcp_bundle_token_budget(count),
            Err(_) => mcp_bundle_token_budget(1000),
        };
        tokens * MCP_CHARS_PER_TOKEN
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

    /// Render a tool result, then annotate it with a staleness banner if any of
    /// the files it references have changed on disk since indexing. Handlers
    /// route through this instead of the free `render_with_kind` so the project
    /// context needed to stat files is available.
    fn render<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
    ) -> CallToolResult {
        self.annotate_staleness(project, render_with_kind(r, kind))
    }

    /// [`Self::render`] with an explicit char budget (context-bundle tools).
    fn render_budget<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
        budget_chars: usize,
    ) -> CallToolResult {
        self.annotate_staleness(project, render_with_budget(r, kind, budget_chars))
    }

    /// If staleness checking is enabled and the result references indexed files
    /// that have since changed on disk, prepend a `⚠️` banner to the content and
    /// record the stale paths under `_meta.stale_files`. Best-effort: any error
    /// (project not resolvable, DB unreadable) leaves the result untouched —
    /// staleness is a hint, never a reason to fail a tool call.
    fn annotate_staleness(&self, project: &str, mut result: CallToolResult) -> CallToolResult {
        if result.is_error == Some(true) || !staleness_enabled() {
            return result;
        }
        let Some(structured) = result.structured_content.as_ref() else {
            return result;
        };
        let mut paths = Vec::new();
        collect_referenced_paths(structured, &mut paths);
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return result;
        }
        paths.truncate(STALENESS_MAX_FILES);

        let stale = match self.compute_stale_files(project, &paths) {
            Ok(stale) if !stale.is_empty() => stale,
            Ok(_) => return result,
            Err(e) => {
                tracing::debug!(error = %e, "staleness check skipped");
                return result;
            }
        };

        if let Some(mut structured) = result.structured_content.take() {
            merge_stale_meta(&mut structured, &stale);
            result.structured_content = Some(structured);
        }
        let banner = format!(
            "⚠️ {} file(s) changed on disk since indexing and may be stale in these results: {}. \
             Read them directly for current contents; run `codesage index` to refresh.",
            stale.len(),
            stale.join(", ")
        );
        let existing = std::mem::take(&mut result.content);
        let mut content = Vec::with_capacity(existing.len() + 1);
        content.push(Content::text(banner));
        content.extend(existing);
        result.content = content;
        result
    }

    /// Of `rel_paths` (project-relative), return those whose current on-disk
    /// content hash differs from the indexed hash (or that no longer exist).
    /// Paths not present in the index are skipped — they may be synthetic or
    /// out-of-index references, not drift.
    fn compute_stale_files(&self, project: &str, rel_paths: &[String]) -> Result<Vec<String>> {
        let state = self.resolve_project(project)?;
        let root = state
            .db_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("could not derive project root from db path"))?
            .to_path_buf();
        let db = self.open_structural_db_for(&state)?;
        let mut stale = Vec::new();
        for rel in rel_paths {
            let Some(expected) = db.get_file_hash(rel)? else {
                continue;
            };
            match std::fs::read(root.join(rel)) {
                Ok(bytes) => {
                    if codesage_parser::discover::content_hash(&bytes) != expected {
                        stale.push(rel.clone());
                    }
                }
                // Indexed file gone or unreadable: treat as stale so the agent
                // is told to look rather than trusting an indexed copy of a file
                // that no longer matches the tree.
                Err(_) => stale.push(rel.clone()),
            }
        }
        Ok(stale)
    }

    /// Resolve a project, embed `query`, and call `f` with the resulting
    /// query embedding + a reranker callback that lazily locks the shared
    /// reranker only when the search pipeline actually invokes it.
    ///
    /// The lock scopes are deliberately tight: the embedder mutex is held
    /// only for the `embed_one` call, the reranker mutex is held only for
    /// the (single) `score_pairs` call inside `search`. SQLite retrieval
    /// and result post-processing run lock-free, so concurrent agents on
    /// the same project can interleave their SQL work while one is in the
    /// (slow) ORT call. Pre-daemon each shim had a per-process embedder
    /// pool so calls were already parallel; this preserves that property
    /// under the shared-daemon model.
    fn with_project_query<F, R>(&self, project: &str, query: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database, &[f32], Option<codesage_graph::RerankFn<'_>>) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let config = self.semantic_embedding_config(&state)?;
        let db = self.open_db_for(&state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let reranker_arc = config
            .reranker
            .as_deref()
            .map(|m| self.get_or_load_reranker(m, &config.device))
            .transpose()?;

        let query_embedding = {
            let mut guard = embedder_arc.lock();
            guard.embed_one(query)?
        };

        let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker_arc.map(|rr| {
            // Closure captures the Arc; per-call .lock() means the reranker
            // mutex is held only across the score_pairs call inside search,
            // not for the surrounding SQL retrieval and post-processing.
            Box::new(move |q: &str, docs: &[&str]| rr.lock().score_pairs(q, docs))
                as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
        });

        f(&db, &query_embedding, rerank_fn)
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

/// Per-response token budget for context bundles, scaled by indexed repo
/// size. Small repos rarely need a fat bundle and a tighter cap keeps the
/// agent's context lean; large repos get more room because one bundle has to
/// cover more ground before the agent falls back to multi-call discovery.
/// Monotonic non-decreasing. `CODESAGE_BUNDLE_TOKEN_BUDGET=<tokens>` forces a
/// fixed value (escape hatch + test determinism).
fn mcp_bundle_token_budget(file_count: usize) -> usize {
    if let Ok(v) = std::env::var("CODESAGE_BUNDLE_TOKEN_BUDGET")
        && let Ok(n) = v.parse::<usize>()
        && n > 0
    {
        return n;
    }
    match file_count {
        0..=149 => 4000,
        150..=4999 => 8000,
        5000..=14999 => 10000,
        _ => 12000,
    }
}

/// Render a handler's `Result<T>` as a structured MCP `CallToolResult`. Successful
/// responses ship both the pretty-printed JSON (for the transcript) and the raw
/// `Value` as `structured_content` so clients can parse without re-deserializing.
/// Failures set `isError: true` per MCP spec; the full anyhow cause chain is
/// included via `{:#}`.
fn render_with_kind<T: serde::Serialize>(r: Result<T>, kind: &str) -> CallToolResult {
    render_with_budget(r, kind, MCP_BUDGET_CHARS)
}

/// Like [`render_with_kind`] but with an explicit char budget. Used by the
/// context-bundle tools, which size their budget by indexed repo file count.
fn render_with_budget<T: serde::Serialize>(
    r: Result<T>,
    kind: &str,
    budget_chars: usize,
) -> CallToolResult {
    match r {
        Ok(v) => {
            let value = serde_json::to_value(&v).unwrap_or(serde_json::Value::Null);
            let capped = cap_to_budget_with(value, kind, budget_chars);
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

/// Cap on how many distinct files a single response triggers an on-disk hash
/// for. Responses are already budget-capped, so the unique-file count is
/// bounded in practice; this is a hard backstop against a pathological result.
const STALENESS_MAX_FILES: usize = 50;

/// JSON keys whose string value (or string array elements) is a
/// project-relative file path in a tool result. Deliberately excludes
/// `imports` / `imported_by` (bare module names) and `clustered_directories`
/// (directories, not files). Over-inclusion is harmless — `compute_stale_files`
/// filters against the indexed file set — but under-inclusion silently misses
/// drift, so err toward listing a key.
const PATH_KEYS: &[&str] = &[
    "file_path",
    "path",
    "file",
    "from_file",
    "cycle_files",
    "members",
    "new_files",
    "removed_files",
    "test_gap_files",
    "wide_blast_files",
    "fix_heavy_files",
    "hotspot_files",
];

/// Staleness checking is on by default; `CODESAGE_STALENESS_CHECK` set to a
/// falsey value disables it (per-response stat+hash of referenced files).
fn staleness_enabled() -> bool {
    !matches!(
        std::env::var("CODESAGE_STALENESS_CHECK").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Walk a serialized tool result, collecting project-relative file paths from
/// the [`PATH_KEYS`] fields wherever they appear (recursing through nested
/// objects and arrays).
fn collect_referenced_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if PATH_KEYS.contains(&k.as_str()) {
                    match v {
                        serde_json::Value::String(s) => out.push(s.clone()),
                        serde_json::Value::Array(items) => {
                            for item in items {
                                if let serde_json::Value::String(s) = item {
                                    out.push(s.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                collect_referenced_paths(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_referenced_paths(item, out);
            }
        }
        _ => {}
    }
}

/// Record the stale paths under `_meta.stale_files` (+ a human `stale_warning`),
/// merging into any existing `_meta` (e.g. a truncation marker) rather than
/// overwriting it. No-op if the structured value isn't a JSON object.
fn merge_stale_meta(structured: &mut serde_json::Value, stale: &[String]) {
    let serde_json::Value::Object(map) = structured else {
        return;
    };
    let meta = map
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let serde_json::Value::Object(meta) = meta else {
        return;
    };
    meta.insert(
        "stale_files".to_string(),
        serde_json::Value::Array(
            stale
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    meta.insert(
        "stale_warning".to_string(),
        serde_json::Value::String(
            "listed files changed on disk since indexing; read them directly and run \
             `codesage index` to refresh"
                .to_string(),
        ),
    );
}

/// If the serialized value fits within MCP_BUDGET_CHARS, return as-is. Otherwise truncate
/// the largest array field (or the whole value if it's already an array) and attach a
/// top-level `_meta` describing the truncation. Agents pick up the meta and either refine
/// or paginate via `offset`.
fn cap_to_budget_with(
    value: serde_json::Value,
    kind: &str,
    budget_chars: usize,
) -> serde_json::Value {
    let approx_tokens_budget = budget_chars / MCP_CHARS_PER_TOKEN;
    let initial_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if initial_len <= budget_chars {
        return value;
    }

    match value {
        serde_json::Value::Array(items) => {
            let total = items.len();
            let kept = truncate_array(items, budget_chars);
            let returned = kept.len();
            serde_json::json!({
                "results": kept,
                "_meta": {
                    "truncated": true,
                    "kind": kind,
                    "total_results": total,
                    "returned": returned,
                    "approx_tokens_budget": approx_tokens_budget,
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
                let remaining = budget_chars.saturating_sub(other_chars);
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
                        "approx_tokens_budget": approx_tokens_budget,
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
/// `.codesage/config.toml` in one project must not poison structural tools
/// (`assess_risk`, `find_coupling`, `find_symbol`, ...). Read or parse failures
/// keep defaults available for those structural paths, but semantic tools fail
/// before loading a model or creating a default vec table because the indexed
/// model is no longer trustworthy.
///
/// The CLI path (`load_project_config` in `main.rs`) deliberately keeps the
/// loud-fail behavior: a user running `codesage index` interactively wants to
/// know their config is broken.
fn load_embedding_config(path: &Path) -> LoadedEmbeddingConfig {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: None,
            };
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not read project config; falling back to embedding defaults",
            );
            return LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: Some(format!(
                    "could not read project config `{}`: {e}",
                    path.display()
                )),
            };
        }
    };
    #[derive(serde::Deserialize)]
    struct Config {
        embedding: Option<EmbeddingConfig>,
    }
    match toml::from_str::<Config>(&content) {
        Ok(parsed) => LoadedEmbeddingConfig {
            config: parsed.embedding.unwrap_or_default(),
            semantic_error: None,
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not parse project config; falling back to embedding defaults",
            );
            LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: Some(format!(
                    "could not parse project config `{}`: {e}",
                    path.display()
                )),
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeSageServer {
    fn get_info(&self) -> ServerInfo {
        use rmcp::model::ServerCapabilities;
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codesage", env!("CARGO_PKG_VERSION")))
            .with_instructions(
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
        name = "project_overview",
        description = "First-call orientation for a project: one bounded response with languages and file/symbol counts, index freshness (structural drift vs git HEAD + semantic coverage), mapped feature summary by kind, a sample of entrypoints (routes/CLI/services/libraries), the top-risk files, trust-boundary clusters, the test-file naming conventions per language, and suggested next CodeSage calls for common intents. Pure aggregation of already-indexed facts — no semantic search, no analysis. Call this once at the start of a session to orient before reaching for `search`/`find_symbol`/`assess_risk`. `top_risk_files` is empty until git history is indexed; `freshness.structural_kind` of `behind_head`/`unrelated_ancestor` means structural results may be stale (re-run `codesage index`).",
        output_schema = schema_for_type::<ProjectOverview>()
    )]
    async fn project_overview_tool(
        &self,
        Parameters(params): Parameters<ProjectOverviewParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    crate::overview::build_project_overview(root, db)
                }),
                "project_overview",
            )
        })
        .await
    }

    #[tool(
        name = "review_rehearsal",
        description = "Predict the objections a reviewer will likely raise against a patch, BEFORE committing. Input is the patch's file list (e.g. `git diff --name-only`). Returns severity-ranked objections — missing tests, high-risk files, wide blast radius, fix-prone files, churn hotspots, import cycles touched, trust-boundary expansion (≥3 boundaries), and feature-test gaps (changed a feature's core files but none of its mapped tests) — each with concrete evidence and the files it concerns, plus paste-ready `summary_notes` (objection counts + risk summary + the exact tests to run). Pure composition of `assess_risk_diff`, `recommend_tests`, index-drift, and feature mapping — read-only, no AI prose. Use as the last step before a commit: fix or consciously accept each objection.",
        output_schema = schema_for_type::<ReviewRehearsal>()
    )]
    async fn review_rehearsal_tool(
        &self,
        Parameters(params): Parameters<ReviewRehearsalParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    crate::rehearsal::build_review_rehearsal(root, db, &file_paths)
                }),
                "review_rehearsal",
            )
        })
        .await
    }

    #[tool(
        name = "find_symbol",
        description = "Find symbol definitions (functions, classes, methods, structs, traits, enums) by name. Returns exact file path, line number, and kind. **Prefer this over Grep/ripgrep for any code-identifier lookup** — one call returns the definition, while grepping for a function name often produces many false hits (call sites, comments, other namespaces) that cost extra Read calls to disambiguate. Use partial names for broad search or qualified names ('MyClass\\\\method' for PHP, 'MyClass.method' for Python) for exact match. For the inverse question (who calls / imports / instantiates this symbol?) use `find_references`. When present, `rationale[]` carries `WHY:` / `NOTE:` / `IMPORTANT:` / `FIXME:` / `HACK:` / `XXX:` / `TODO:` comments attached to the definition — read these before refactoring or renaming so the agent doesn't drop a constraint the author wrote down. Currently extracted for Rust and Python.",
        output_schema = schema_for_type::<FindSymbolResults>()
    )]
    async fn find_symbol_tool(
        &self,
        Parameters(params): Parameters<FindSymbolParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let kind = params.kind.as_deref().and_then(SymbolKind::parse);
            let req = FindSymbolRequest {
                name: params.name,
                kind,
            };
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| find_symbol(db, &req)),
                "find_symbol",
            )
        })
        .await
    }

    #[tool(
        name = "find_references",
        description = "Find all references to a symbol across the codebase. **Prefer this over Grep for 'where is X called / imported / instantiated?'** — returns structured {file, line, kind} rows with the reference type (call/import/inheritance/instantiation/type_hint) already classified, instead of raw grep hits that mix definitions, comments, and string literals together. For the definition itself use `find_symbol`; for transitive blast radius (callers of callers) use `impact_analysis`.",
        output_schema = schema_for_type::<FindReferencesResults>()
    )]
    async fn find_references_tool(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let kind = params.kind.as_deref().and_then(ReferenceKind::parse);
            let req = FindReferencesRequest {
                symbol_name: params.name,
                kind,
            };
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| find_references(db, &req)),
                "find_references",
            )
        })
        .await
    }

    #[tool(
        name = "list_dependencies",
        description = "List immediate (single-hop) import/include dependencies for a file: what THIS file imports and which other files import THIS file. Use when the question is 'what does this file depend on?' or 'who imports this file?'. For 'what breaks if I change this?' use `impact_analysis` (walks multiple hops, ranks by distance). For per-symbol callers/callees use `find_references` (per-symbol grain, not per-file).",
        output_schema = schema_for_type::<DependencyEntry>()
    )]
    async fn list_dependencies_tool(
        &self,
        Parameters(params): Parameters<ListDependenciesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| {
                    list_dependencies(db, &params.file_path)
                }),
                "list_dependencies",
            )
        })
        .await
    }

    #[tool(
        name = "search",
        description = "Semantic code search (embedding-based + cross-encoder reranking). **Prefer this over Grep when you don't know the exact symbol name** — useful for queries like 'where is auth handled', 'error handling in the session pipeline', 'database connection pooling', 'where do we validate inputs'. Grep needs the literal token already; `search` lets the agent ask by intent. For exact identifier lookups with a known name, use `find_symbol` or `find_references` instead.",
        output_schema = schema_for_type::<SearchResults>()
    )]
    async fn search_tool(&self, Parameters(params): Parameters<SearchParams>) -> CallToolResult {
        self.blocking(move |s| {
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
            let query_for_embed = req.query.clone();
            s.render(
                &params.project,
                s.with_project_query(&params.project, &query_for_embed, |db, emb, rr| {
                    search(db, emb, rr, &req)
                }),
                "search",
            )
        })
        .await
    }

    #[tool(
        name = "impact_analysis",
        description = "Estimate which files are affected by changing a symbol or file. Walks the **reverse** reference graph up to `depth` hops (default 2) — i.e., callers/importers of the target and transitively their callers/importers — reports affected files ranked by distance and reference count. **Multi-hop blast radius from the target outward to its dependents.** Returns `[]` for leaf files nothing imports/calls. Does NOT include same-file symbols, does NOT include what the target itself depends on (use `list_dependencies` for the target's own forward dependencies). Use BEFORE making changes to know what else needs review/testing. For single-hop importer/importee of one file use `list_dependencies`; for raw call sites of a specific symbol use `find_references`.",
        output_schema = schema_for_type::<ImpactAnalysisResults>()
    )]
    async fn impact_analysis_tool(
        &self,
        Parameters(params): Parameters<ImpactParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let req = ImpactRequest {
                target: ImpactTarget::from_hint(params.target, params.is_file),
                depth: params.depth.unwrap_or(2),
                source_only: params.source_only.unwrap_or(false),
            };
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| impact_analysis(db, &req)),
                "impact_analysis",
            )
        })
        .await
    }

    #[tool(
        name = "export_context",
        description = "Build a curated context bundle for a free-form **query** or a single **symbol**: semantic search results, overlapping symbol definitions, and optionally caller/callee code, all wrapped as a structured bundle ready for LLM consumption. Use when the anchor is a phrase ('error handling in the parser') or one named symbol. For an already-mapped feature slice (entrypoint + owned files + tests + context already resolved), use `feature_bundle` instead — that anchors on `feature_id` and avoids re-running semantic search. Symbol entries inside the bundle carry `rationale[]` when the author left `WHY:` / `NOTE:` / `IMPORTANT:` / `FIXME:` / `HACK:` / `XXX:` / `TODO:` comments — preserve these in any synthesis the agent performs from the bundle. Currently extracted for Rust and Python.",
        output_schema = schema_for_type::<ContextBundle>()
    )]
    async fn export_context_tool(
        &self,
        Parameters(params): Parameters<ExportContextParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let req = ExportRequest::from_target(
                params.target,
                params.is_symbol.unwrap_or(false),
                params.limit.unwrap_or(5),
                params.include_callers.unwrap_or(false),
                params.include_callees.unwrap_or(false),
            );
            let budget = s.bundle_budget_chars(&params.project);
            if let Some(sym_name) = req.symbol.clone() {
                return s.render_budget(
                    &params.project,
                    s.with_project_context_db(&params.project, |db| {
                        export_context_for_symbol(db, &sym_name, &req)
                    }),
                    "export_context",
                    budget,
                );
            }
            let query_for_embed = req.query.clone().unwrap_or_default();
            s.render_budget(
                &params.project,
                s.with_project_query(&params.project, &query_for_embed, |db, emb, rr| {
                    export_context(db, emb, rr, &req)
                }),
                "export_context",
                budget,
            )
        })
        .await
    }

    #[tool(
        name = "find_coupling",
        description = "Files that historically change together with the given file, ranked by exponentially-decayed weight (τ=180d). Backed by git history. Use when planning a change to know which OTHER files (especially tests) tend to need updates too. Response is `{coupled: [...], file_indexed: bool, file_commits: u32, note?: string}` — read `coupled` for the ranked list. When `coupled` is empty, `note` disambiguates: file never indexed vs. file has history but no pair above the min-count=3 threshold vs. path shape mismatch. Index into `.coupled`, not the response directly. For the patch-level question 'which tests should I run after editing these files?' use `recommend_tests` instead (resolves test conventions + co-change in one call). For the single-file risk score that already folds in coupling pressure use `assess_risk`.",
        output_schema = schema_for_type::<CouplingReport>()
    )]
    async fn find_coupling_tool(
        &self,
        Parameters(params): Parameters<CouplingParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let limit = params.limit.unwrap_or(10);
            let file_path = params.file_path.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| find_coupling(db, &file_path, limit)),
                "find_coupling",
            )
        })
        .await
    }

    #[tool(
        name = "assess_risk",
        description = "Risk score for changing one file: blends seven signals — churn percentile, fix ratio, blast radius (depth-2 reverse deps), historical coupling, test-gap, import-cycle membership, and trust-boundary count — into a 0..1 score. Response also carries `in_cycle` / `cycle_size` / `cycle_files`, the `trust_boundaries[]` list, and `top_symbols[]` (up to 5 symbols inside the file ranked by line count + reference count + cycle membership). Notes are paste-ready for PR descriptions; the `crosses N trust boundaries` line fires when ≥3 boundaries cross. Use BEFORE writing a patch to calibrate caution and BEFORE submitting to flag concerns. For per-file scoring across N files in one call use `assess_risk_batch`; for patch-level aggregation (max/mean, summary_notes, cycles touching the patch) use `assess_risk_diff`.",
        output_schema = schema_for_type::<RiskAssessment>()
    )]
    async fn assess_risk_tool(&self, Parameters(params): Parameters<RiskParams>) -> CallToolResult {
        self.blocking(move |s| {
            let file_path = params.file_path.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| assess_risk(db, &file_path)),
                "assess_risk",
            )
        })
        .await
    }

    #[tool(
        name = "assess_risk_diff",
        description = "Aggregate risk for a SET of files (the file list of a patch or PR). Returns per-file decomposition plus rollups: max_score, mean_score, max_risk_file, and lists of files in each risk category (test_gap, hotspot, fix-heavy, wide blast radius). Use BEFORE submitting a patch: if max_score is high or any test_gap_files exist, add tests, split the patch, or flag concerns. summary_notes are paste-ready for a PR description. On large patches that touch ≥5 files from one directory, per-file entries for that directory move from `files` into a `clustered_directories[]` entry (top-3 by score preserved in detail, rest by name); rollup arrays still list every clustered file by name, so cross-referencing still works. `cycles_touching_patch[]` lists import cycles (files that mutually depend via import/include/inheritance/trait_use) that include at least one patch file, each with `members`, `size`, and `max_churn_file` (best refactor target). Honest caveat: we can't distinguish cycles the patch introduced from cycles that already existed; phrase PR feedback as 'this patch touches an existing cycle' unless you've verified the base branch.",
        output_schema = schema_for_type::<RiskDiffAssessment>()
    )]
    async fn assess_risk_diff_tool(
        &self,
        Parameters(params): Parameters<RiskDiffParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| assess_risk_diff(db, &file_paths)),
                "assess_risk_diff",
            )
        })
        .await
    }

    #[tool(
        name = "assess_risk_batch",
        description = "Risk score for EACH of N files, returned per-file with no patch-level aggregation. Use when you have a list of files (impact analysis output, coupling neighbours, the files of a feature you're touching one-by-one) and want each individual score — cuts the per-file MCP round-trip overhead vs calling `assess_risk` N times. Each entry is a full RiskAssessment with the same shape as `assess_risk`. The response also includes a top-level `_legend` short-code map: when ≥3 files in the batch share a categorical note (test-gap, no-git-history), per-file `notes[]` entries are aliased to short codes (e.g. `\"T\"`, `\"NG\"`) and the legend resolves them. For patch-level aggregation (max/mean, hotspot/test-gap rollups, cycles), use `assess_risk_diff` instead — they answer different questions.",
        output_schema = schema_for_type::<RiskBatchAssessment>()
    )]
    async fn assess_risk_batch_tool(
        &self,
        Parameters(params): Parameters<RiskBatchParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| assess_risk_batch(db, &file_paths)),
                "assess_risk_batch",
            )
        })
        .await
    }

    #[tool(
        name = "recommend_tests",
        description = "Tests an agent should run after editing the given files. Returns `primary` (sibling tests resolved by language convention — FooTest.php, foo.test.ts, test_foo.py, foo_test.go — high confidence, always run these) and `coupled` (tests that historically change with the input files via git co-change history — medium confidence, catches integration tests that don't follow naming conventions). Empty result means no test files in the index for these paths. Use AFTER making a change to know which subset of tests to actually run. Pair with `assess_risk_diff` on the same file list for the patch-level risk rollup (test-gap files, hotspot list, paste-ready summary notes).",
        output_schema = schema_for_type::<TestRecommendations>()
    )]
    async fn recommend_tests_tool(
        &self,
        Parameters(params): Parameters<TestsForParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| recommend_tests(db, &file_paths)),
                "recommend_tests",
            )
        })
        .await
    }

    #[tool(
        name = "session_start",
        description = "Snapshot the project's structural state at the START of an editing session. Persists file count, symbol count, the full file list, all import cycles, and the top-50 highest-risk files (with their scores) to `.codesage/sessions/<session_id>.json`. Pair with `session_end` using the same `session_id` to detect new cycles, removed/added files, or risk regressions on hot files introduced during the session. `session_id` defaults to \"default\" — use a distinct id when running multiple parallel sessions. Re-running `session_start` overwrites the snapshot (useful for resetting a baseline mid-session).",
        output_schema = schema_for_type::<SessionSnapshot>()
    )]
    async fn session_start_tool(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let session_id = params
                .session_id
                .clone()
                .unwrap_or_else(|| "default".to_string());
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    session_start(root, db, &session_id)
                }),
                "session_start",
            )
        })
        .await
    }

    #[tool(
        name = "list_features",
        description = "List feature slices in the project, optionally filtered by kind, language, or tag. A feature is a behavior-keyed bundle (entrypoint + owned files + context + tests + trust boundaries) — e.g. \"Laravel route POST /api/login\", \"Rust binary `codesage`\", \"php-src extension `iconv`\", \"CMake binary `myapp`\". Use this to discover the agent-facing surface area of the project before deep-diving into a specific slice. Pair with `find_feature` (file → features) and `assess_risk` (per-file scoring inside a feature).",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    async fn list_features_tool(
        &self,
        Parameters(params): Parameters<ListFeaturesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let kind = params.kind.as_deref().and_then(FeatureKind::parse);
            let language = params.language.as_deref().and_then(Language::parse);
            let tag = params.tag.clone();
            let since = params.since.clone();
            let limit = params.limit.unwrap_or(100);
            // With `since`, fetch unbounded then cap after the changed-file
            // intersection, mirroring the CLI: the SQL LIMIT runs before the
            // diff filter, so a pre-filter limit would truncate candidates.
            let query_limit = if since.is_some() { 0 } else { limit };
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    let mut features =
                        db.list_features(kind, language, tag.as_deref(), query_limit)?;
                    if let Some(git_ref) = since.as_deref() {
                        let changed = codesage_graph::changed_files_since(root, git_ref)?;
                        features
                            .retain(|f| codesage_graph::feature_touched_since(&f.files, &changed));
                        if limit > 0 && features.len() > limit {
                            features.truncate(limit);
                        }
                    }
                    Ok(features)
                }),
                "list_features",
            )
        })
        .await
    }

    #[tool(
        name = "find_feature",
        description = "Features that include the given file in any role (entry, owned, context, or test). Use to answer \"what feature owns src/auth/login.php?\" — returns the matching feature records with their full file lists, tags, and trust boundaries. Empty result means no mapped feature claims this file (common: not every file belongs to a feature slice). For the curated code bundle of a matched feature (entry + owned + tests + context wrapped for LLM consumption) call `feature_bundle` with the `feature_id`.",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    async fn find_feature_tool(
        &self,
        Parameters(params): Parameters<FindFeatureParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file = params.file_path.clone();
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| db.features_for_file(&file)),
                "find_feature",
            )
        })
        .await
    }

    #[tool(
        name = "feature_bundle",
        description = "Curated code bundle for one feature_id. Same shape as `export_context` but anchored on the feature's already-resolved file list (entry + owned + tests + context) instead of semantic search results. `primary[]` carries chunks from owned/entry files, `related[]` carries tests and context. Set `include_callers` / `include_callees` to also expand the entry symbol's callers/callees into `related[]` (reuses the symbol graph used by `export_context`). Use after `list_features` / `find_feature` to get all the code an agent needs to review or modify the slice in one MCP call — avoids fan-out Read calls per file. Empty bundle with `target_description` ending `(not found)` means the feature_id doesn't exist; empty bundle with non-empty title means the feature exists but no files have been semantically indexed yet (run `codesage index`).",
        output_schema = schema_for_type::<ContextBundle>()
    )]
    async fn feature_bundle_tool(
        &self,
        Parameters(params): Parameters<FeatureBundleParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let feature_id = params.feature_id.clone();
            let include_callers = params.include_callers.unwrap_or(false);
            let include_callees = params.include_callees.unwrap_or(false);
            let limit = params.limit.unwrap_or(5);
            // Use the context DB (binds to the configured embedding model's
            // chunk table) so `primary`/`related` resolve real chunks. The
            // structural-only db variant points at the default chunk table
            // and returns empty content on projects using a non-default
            // model (php-src uses jina v2 768-dim, MiniLM is the default).
            let budget = s.bundle_budget_chars(&params.project);
            s.render_budget(
                &params.project,
                s.with_project_context_db(&params.project, |db| {
                    feature_bundle(db, &feature_id, include_callers, include_callees, limit)
                }),
                "feature_bundle",
                budget,
            )
        })
        .await
    }

    #[tool(
        name = "session_end",
        description = "Diff the current structural state against the snapshot saved by `session_start` (matched by `session_id`, default \"default\"). Returns `pass: bool` (true when no new import cycles were introduced AND no top-risk file regressed by ≥ 0.10), plus `new_cycles`, `resolved_cycles`, `risk_regressions` (per-file before/after/delta), `new_files`, `removed_files`, and `summary_notes` ready to paste into a PR description. Errors when the snapshot file is missing — call `session_start` first. Snapshot file is left in place after the diff so the same id can be re-diffed.",
        output_schema = schema_for_type::<SessionDiff>()
    )]
    async fn session_end_tool(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let session_id = params
                .session_id
                .clone()
                .unwrap_or_else(|| "default".to_string());
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    session_end(root, db, &session_id)
                }),
                "session_end",
            )
        })
        .await
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

/// If `line` is a JSON-RPC `tools/call` whose `params.arguments` lacks a
/// non-empty `project`, inject `default`; otherwise return the line
/// unchanged. Lets non-Claude agents (registered via `codesage mcp
/// --project <root>`) call tools without threading the absolute project
/// path on every call. Non-JSON lines and other methods pass through
/// untouched.
pub(crate) fn inject_default_project_line(line: &str, default: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return line.to_string();
    }
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return line.to_string();
    };
    // Handle a JSON-RPC batch (top-level array) by injecting into each
    // contained message, as well as a single message.
    let changed = match &mut v {
        serde_json::Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, item| acc | inject_into_message(item, default)),
        other => inject_into_message(other, default),
    };
    if !changed {
        return line.to_string();
    }
    serde_json::to_string(&v).unwrap_or_else(|_| line.to_string())
}

/// Inject `default` into one JSON-RPC message if it is a `tools/call` whose
/// arguments omit a non-empty `project`. Returns whether it changed `v`.
/// Creates an empty `arguments` object when the call omits it entirely.
fn inject_into_message(v: &mut serde_json::Value, default: &str) -> bool {
    if v.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
        return false;
    }
    let Some(params) = v.get_mut("params").and_then(|p| p.as_object_mut()) else {
        return false;
    };
    let args = params
        .entry("arguments")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if args.is_null() {
        *args = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(args) = args.as_object_mut() else {
        return false; // arguments present but not an object: let the server validate
    };
    let needs = match args.get("project") {
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        Some(_) => false, // present but non-string: let the server validate
        None => true,
    };
    if !needs {
        return false;
    }
    args.insert(
        "project".to_string(),
        serde_json::Value::String(default.to_string()),
    );
    true
}

/// Pump newline-delimited JSON-RPC from `reader` to `writer`, injecting
/// `default_project` into `tools/call` messages that omit it. Used by both
/// the daemon shim (stdin → socket) and the direct-mode server (stdin →
/// in-process transport).
pub(crate) async fn pump_lines_injecting<R, W>(
    reader: R,
    mut writer: W,
    default_project: String,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let out = inject_default_project_line(&line, &default_project);
        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

pub async fn run_mcp_server(default_project: Option<String>) -> Result<()> {
    match default_project {
        Some(dp) => {
            // Feed stdin through the project-injecting pump into an
            // in-process pipe that backs the MCP transport's read half.
            let (mut feed, server_read) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = pump_lines_injecting(tokio::io::stdin(), &mut feed, dp).await;
                let _ = feed.shutdown().await;
            });
            let server = CodeSageServer::new();
            let code = match server.serve((server_read, tokio::io::stdout())).await {
                Ok(service) => match service.waiting().await {
                    Ok(_) => 0,
                    Err(e) => {
                        tracing::error!(error = %e, "MCP server stopped");
                        1
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "MCP server error");
                    1
                }
            };
            // The spawned stdin pump owns a blocking read that can't be
            // cancelled; if the server exits first, the runtime drop in
            // cmd_mcp would block on it forever. Exit the process directly,
            // mirroring the daemon shim's proxy_stdio rationale.
            std::process::exit(code);
        }
        None => {
            let server = CodeSageServer::new();
            let service = server
                .serve(rmcp::transport::io::stdio())
                .await
                .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
            service
                .waiting()
                .await
                .map_err(|e| anyhow::anyhow!("MCP server stopped: {e}"))?;
        }
    }
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
        let out = cap_to_budget_with(v.clone(), "test", MCP_BUDGET_CHARS);
        assert_eq!(out, v);
    }

    #[test]
    fn inject_default_project_fills_missing() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let out = inject_default_project_line(line, "/abs/proj");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["params"]["arguments"]["project"], json!("/abs/proj"));
    }

    #[test]
    fn inject_default_project_fills_empty_string() {
        let line = r#"{"method":"tools/call","params":{"name":"search","arguments":{"project":"  ","query":"x"}}}"#;
        let out = inject_default_project_line(line, "/abs/proj");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["params"]["arguments"]["project"], json!("/abs/proj"));
        assert_eq!(v["params"]["arguments"]["query"], json!("x"));
    }

    #[test]
    fn inject_default_project_leaves_present_value() {
        let line = r#"{"method":"tools/call","params":{"name":"search","arguments":{"project":"/other"}}}"#;
        let out = inject_default_project_line(line, "/abs/proj");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["params"]["arguments"]["project"], json!("/other"));
    }

    #[test]
    fn inject_default_project_creates_missing_arguments() {
        let line = r#"{"method":"tools/call","params":{"name":"list_features"}}"#;
        let out = inject_default_project_line(line, "/abs/proj");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["params"]["arguments"]["project"], json!("/abs/proj"));
    }

    #[test]
    fn inject_default_project_handles_batch_array() {
        let line = r#"[{"method":"tools/call","params":{"name":"a","arguments":{}}},{"method":"initialize","params":{}},{"method":"tools/call","params":{"name":"b","arguments":{"project":"/keep"}}}]"#;
        let out = inject_default_project_line(line, "/abs/proj");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["params"]["arguments"]["project"], json!("/abs/proj"));
        assert!(
            v[1]["params"].get("arguments").is_none(),
            "initialize untouched"
        );
        assert_eq!(v[2]["params"]["arguments"]["project"], json!("/keep"));
    }

    #[test]
    fn inject_default_project_ignores_non_tool_calls() {
        let init = r#"{"method":"initialize","params":{}}"#;
        assert_eq!(inject_default_project_line(init, "/abs/proj"), init);
        let garbage = "not json at all";
        assert_eq!(inject_default_project_line(garbage, "/abs/proj"), garbage);
    }

    #[test]
    fn bundle_token_budget_is_monotonic_by_repo_size() {
        // Env override not exercised here: setting env vars is `unsafe` and
        // racy under edition 2024; tier coverage is what matters.
        let tiers = [
            mcp_bundle_token_budget(0),
            mcp_bundle_token_budget(149),
            mcp_bundle_token_budget(150),
            mcp_bundle_token_budget(4999),
            mcp_bundle_token_budget(5000),
            mcp_bundle_token_budget(14999),
            mcp_bundle_token_budget(15000),
            mcp_bundle_token_budget(500_000),
        ];
        assert_eq!(tiers[0], 4000);
        assert_eq!(tiers[1], 4000);
        assert_eq!(tiers[2], 8000);
        assert_eq!(tiers[4], 10000);
        assert_eq!(tiers[6], 12000);
        for w in tiers.windows(2) {
            assert!(w[1] >= w[0], "budget must be non-decreasing: {tiers:?}");
        }
    }

    #[test]
    fn cap_respects_explicit_smaller_budget() {
        // A small per-call budget truncates input that the default would pass.
        let items: Vec<Value> = (0..20)
            .map(|i| json!({"i": i, "blob": fat_string(500)}))
            .collect();
        let out = cap_to_budget_with(Value::Array(items), "feature_bundle", 4000);
        let obj = out.as_object().expect("wrapped as object");
        assert_eq!(obj["_meta"]["truncated"], json!(true));
        assert_eq!(obj["_meta"]["approx_tokens_budget"], json!(1000));
    }

    #[test]
    fn cap_truncates_top_level_array_when_over_budget() {
        // Each item is ~1100 chars; 50 items = ~55k chars, well over 32k budget.
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let out = cap_to_budget_with(Value::Array(items), "search", MCP_BUDGET_CHARS);
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
        let out = cap_to_budget_with(v, "export_context", MCP_BUDGET_CHARS);
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
        let out = cap_to_budget_with(v.clone(), "test", MCP_BUDGET_CHARS);
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
    fn malformed_config_keeps_structural_defaults_and_reports_semantic_error() {
        let path = write_tmp("malformed", "embedding = { this is not valid toml ===");
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(
            loaded
                .semantic_error
                .as_deref()
                .is_some_and(|e| e.contains("could not parse project config")),
            "semantic paths should fail loudly on malformed config: {loaded:?}"
        );
    }

    #[test]
    fn missing_config_returns_defaults() {
        let path = std::env::temp_dir().join(format!(
            "codesage-mcp-test-missing-{}.toml",
            std::process::id()
        ));
        // ensure path doesn't exist
        let _ = std::fs::remove_file(&path);
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn well_formed_config_parses() {
        let path = write_tmp(
            "valid",
            "[embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\ndevice = \"cpu\"\n",
        );
        let loaded = load_embedding_config(&path);
        assert_eq!(
            loaded.config.model,
            "sentence-transformers/all-MiniLM-L6-v2"
        );
        assert_eq!(loaded.config.device, "cpu");
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn config_without_embedding_section_returns_defaults() {
        // A valid TOML that just doesn't have an `[embedding]` table — the
        // file is fine, the embedding section is absent, defaults apply.
        let path = write_tmp("no-embedding", "[project]\nname = \"foo\"\n");
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn server_instances_can_share_cache_state() {
        let state = Arc::new(CodeSageServerState::new());
        let first = CodeSageServer::with_state(state.clone());
        let second = CodeSageServer::with_state(state.clone());

        assert!(Arc::ptr_eq(&first.state, &second.state));
    }

    #[test]
    fn slot_loader_runs_exactly_once_under_concurrent_first_callers() {
        // CR-001 regression. Pre-fix: check map → drop lock → call new()
        // → race to insert. Two cold misses for the same key both ran
        // the loader and the loser's value was thrown away. With the
        // per-key slot lock, only the first thread runs `load`; the rest
        // observe Some(arc) and return it.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let map: Arc<Mutex<HashMap<String, ModelSlot<u32>>>> = Arc::new(Mutex::new(HashMap::new()));
        let load_count = Arc::new(AtomicUsize::new(0));

        // Gate the loader on a shared start signal so all threads are
        // poised to race, then release them simultaneously.
        let start = Arc::new(std::sync::Barrier::new(16));

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let map = map.clone();
                let load_count = load_count.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    get_or_load_slot(&map, "shared-key".to_string(), || {
                        load_count.fetch_add(1, Ordering::SeqCst);
                        // Brief sleep widens the race window so a buggy
                        // implementation actually loses.
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok::<u32, anyhow::Error>(42 + i as u32)
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            load_count.load(Ordering::SeqCst),
            1,
            "loader must run exactly once across all concurrent callers"
        );

        // All callers must observe the SAME Arc (pointer equality), not
        // separate constructions.
        let first = results[0].as_ref().unwrap().clone();
        for r in &results {
            let arc = r.as_ref().unwrap();
            assert!(Arc::ptr_eq(&first, arc), "all callers should share one Arc");
        }
    }

    #[test]
    fn slot_loader_runs_per_key_in_parallel() {
        // Distinct keys must hold distinct slots — different cold loads
        // should not serialize on each other. Verify by measuring that
        // two loaders that each block for ~80ms complete in well under
        // 160ms (they run concurrently, not back-to-back).
        let map: Arc<Mutex<HashMap<String, ModelSlot<u32>>>> = Arc::new(Mutex::new(HashMap::new()));
        let start = Arc::new(std::sync::Barrier::new(2));

        let t0 = std::time::Instant::now();
        let h1 = {
            let map = map.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                get_or_load_slot(&map, "k1".to_string(), || {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    Ok::<u32, anyhow::Error>(1)
                })
            })
        };
        let h2 = {
            let map = map.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                get_or_load_slot(&map, "k2".to_string(), || {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    Ok::<u32, anyhow::Error>(2)
                })
            })
        };
        h1.join().unwrap().unwrap();
        h2.join().unwrap().unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(140),
            "distinct keys must load in parallel; elapsed {:?} suggests serialization",
            elapsed
        );
    }

    #[test]
    fn slot_loader_failure_leaves_slot_retryable() {
        // A failed load must not poison the slot: the next caller should
        // be able to retry. Pre-fix code had the same behavior (the
        // failed value never went into the map); the helper preserves
        // that property by writing to *slot_guard only on Ok.
        let map: Arc<Mutex<HashMap<String, ModelSlot<u32>>>> = Arc::new(Mutex::new(HashMap::new()));

        let first: Result<_, anyhow::Error> =
            get_or_load_slot(&map, "k".to_string(), || anyhow::bail!("boom"));
        assert!(first.is_err());

        let second = get_or_load_slot(&map, "k".to_string(), || Ok::<u32, anyhow::Error>(99));
        let arc = second.unwrap();
        assert_eq!(*arc.lock(), 99);
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
    fn structural_project_db_still_opens_with_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "embedding = { this is not valid toml ===",
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
    fn semantic_project_query_rejects_malformed_config_without_creating_default_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "embedding = { this is not valid toml ===",
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        Database::open(&db_path).unwrap();

        let server = CodeSageServer::new();
        let err = server
            .with_project_query(root.to_str().unwrap(), "query", |_, _, _| Ok(()))
            .unwrap_err();

        assert!(
            err.to_string().contains("could not parse project config"),
            "unexpected error: {err:#}"
        );
        let db = Database::open(&db_path).unwrap();
        assert!(
            db.list_vec_tables().unwrap().is_empty(),
            "semantic query should fail before creating a default vec table"
        );
    }

    #[tokio::test]
    async fn symbol_export_uses_existing_chunks_without_loading_embedding_model() {
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
        let result = server
            .export_context_tool(Parameters(ExportContextParams {
                project: root.to_str().unwrap().to_string(),
                target: "target".to_string(),
                is_symbol: Some(true),
                limit: Some(5),
                include_callers: Some(false),
                include_callees: Some(false),
            }))
            .await;

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

    #[test]
    fn collect_referenced_paths_pulls_path_fields_recursively() {
        let v = json!({
            "results": [
                { "file_path": "src/a.rs", "name": "foo", "imports": ["std::io"] },
                { "from_file": "src/b.rs" }
            ],
            "cycle_files": ["src/c.rs", "src/d.rs"],
            "clustered_directories": ["src/ignored"],
            "_meta": { "truncated": true }
        });
        let mut paths = Vec::new();
        collect_referenced_paths(&v, &mut paths);
        paths.sort();
        // file_path, from_file, and cycle_files[*] collected; `imports`
        // (bare module name) and `clustered_directories` (a dir) excluded.
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]);
    }

    #[test]
    fn merge_stale_meta_preserves_existing_meta() {
        let mut v = json!({ "results": [], "_meta": { "truncated": true } });
        merge_stale_meta(&mut v, &["src/a.rs".to_string()]);
        // existing key untouched, stale info added alongside.
        assert_eq!(v["_meta"]["truncated"], json!(true));
        assert_eq!(v["_meta"]["stale_files"], json!(["src/a.rs"]));
        assert!(v["_meta"]["stale_warning"].is_string());
    }

    #[test]
    fn merge_stale_meta_creates_meta_when_absent() {
        let mut v = json!({ "results": [] });
        merge_stale_meta(&mut v, &["x.rs".to_string()]);
        assert_eq!(v["_meta"]["stale_files"], json!(["x.rs"]));
    }

    #[test]
    fn staleness_detects_changed_and_missing_files() {
        // End-to-end against a real structural index: a file whose on-disk
        // content matches its indexed hash is not stale; one that changed is;
        // one deleted is; one never indexed is ignored (not in the index).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        let db = Database::open(&codesage_dir.join("index.db")).unwrap();

        let write = |rel: &str, body: &[u8]| {
            let abs = root.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, body).unwrap();
        };
        let index = |rel: &str, body: &[u8]| {
            db.upsert_file(&codesage_protocol::FileInfo {
                path: rel.to_string(),
                language: Language::Rust,
                content_hash: codesage_parser::discover::content_hash(body),
            })
            .unwrap();
        };

        // unchanged on disk vs index
        write("src/same.rs", b"fn a() {}");
        index("src/same.rs", b"fn a() {}");
        // changed on disk since indexing
        write("src/changed.rs", b"fn b() {} // edited");
        index("src/changed.rs", b"fn b() {}");
        // indexed but deleted from disk
        index("src/gone.rs", b"fn c() {}");
        drop(db);

        let server = CodeSageServer::with_state(Arc::new(CodeSageServerState::new()));
        let project = root.to_str().unwrap();

        let stale = server
            .compute_stale_files(
                project,
                &[
                    "src/same.rs".to_string(),
                    "src/changed.rs".to_string(),
                    "src/gone.rs".to_string(),
                    "src/never_indexed.rs".to_string(),
                ],
            )
            .unwrap();

        assert!(!stale.contains(&"src/same.rs".to_string()));
        assert!(stale.contains(&"src/changed.rs".to_string()));
        assert!(stale.contains(&"src/gone.rs".to_string()));
        assert!(!stale.contains(&"src/never_indexed.rs".to_string()));

        // annotate_staleness should prepend a banner and set _meta.stale_files
        // when the result references a changed file.
        let result = render_with_kind(
            Ok(json!([{ "file_path": "src/changed.rs", "line": 1 }])),
            "search",
        );
        let annotated = server.annotate_staleness(project, result);
        let banner = annotated.content.first().and_then(|c| c.as_text());
        assert!(
            banner
                .map(|t| t.text.contains("src/changed.rs"))
                .unwrap_or(false),
            "expected a staleness banner naming the changed file"
        );
        let stale_files = &annotated.structured_content.unwrap()["_meta"]["stale_files"];
        assert_eq!(stale_files, &json!(["src/changed.rs"]));
    }
}
