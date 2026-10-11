mod detail;
mod diagnostics;
mod dispatch;
mod edit_check;
pub(crate) mod envelope;
pub(crate) mod error;
pub(crate) mod framing;
mod help;
mod next;
mod overview_cache;
pub(crate) mod params;
mod render;
mod schema;
mod state;
mod work;

pub(crate) use state::{CodeSageServerState, WATCHER_STOP_WAIT};

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use codesage_graph::{
    assess_risk, assess_risk_batch, assess_risk_diff, export_context, export_context_for_target,
    feature_bundle, find_coupling, find_references, find_similar, find_symbol, from_trace,
    impact_analysis_report, list_dependencies, recommend_tests_with_reachability, search_page,
    session_end, session_start, trace_call_path,
};
use codesage_protocol::{
    CallPathReport, CallPathRequest, ContextBundle, CouplingReport, DependencyEntry,
    DescribeResult, ExportRequest, FeatureListResults, FindReferencesRequest,
    FindReferencesResults, FindSimilarResults, FindSymbolRequest, FindSymbolResults,
    FromTraceReport, FromTraceRequest, ImpactOptions, ImpactReport, ImpactRequest, ImpactTarget,
    ProjectOverview, ReviewRehearsal, RiskAssessment, RiskBatchAssessment, RiskDiffAssessment,
    SearchRequest, SearchResults, SessionDiff, SessionStartReport, TestRecommendations,
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::schema_for_type, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerConfig},
    tool, tool_handler, tool_router,
};

use params::*;
use schema::finalize_tools_for_listing;

const MAX_MCP_LIMIT: usize = 100;
const MAX_MCP_IMPACT_LIMIT: usize = 500;
const MAX_MCP_IMPACT_DEPTH: usize = 6;
const MAX_MCP_FILE_PATHS: usize = 500;
const MAX_MCP_CONTEXT_LIMIT: usize = 20;
const MAX_MCP_FEATURE_LIMIT: usize = 500;
const MAX_MCP_FEATURE_SCAN_LIMIT: usize = 5_000;
const MAX_MCP_OFFSET: usize = 1_000;
const MAX_MCP_TRACE_FRAMES: usize = 50;
/// Headroom for CLI commit batches of 50 files.
pub(crate) const MAX_MCP_EMBED_TEXTS: usize = 4_096;
/// Allow UTF-8 expansion, overlap, and symbol headers without accepting whole files.
pub(crate) const MAX_MCP_EMBED_TEXT_BYTES: usize = 32 * codesage_embed::chunk::DEFAULT_CHUNK_SIZE;
/// Bound aggregate allocation independently of the count and per-text caps.
pub(crate) const MAX_MCP_EMBED_TOTAL_BYTES: usize =
    MAX_MCP_EMBED_TEXTS * 4 * codesage_embed::chunk::DEFAULT_CHUNK_SIZE;
/// Stable refusal prefix that triggers the CLI's private-embedder fallback.
pub(crate) const EMBED_TEXTS_OVER_CAP: &str = "embed_texts: over cap:";
/// Stable identity refusal prefix: the CLI must abort rather than fall back privately.
pub(crate) const EMBED_TEXTS_FINGERPRINT_MISMATCH: &str = "embed_texts: fingerprint mismatch:";

fn check_embed_texts_caps(texts: &[String]) -> Result<(), error::McpError> {
    let over_cap = |message: String| Err(error::McpError::new(error::ErrorCode::OverCap, message));
    if texts.len() > MAX_MCP_EMBED_TEXTS {
        return over_cap(format!(
            "{EMBED_TEXTS_OVER_CAP} {} texts exceeds the per-call cap of {MAX_MCP_EMBED_TEXTS}",
            texts.len()
        ));
    }
    let mut total = 0usize;
    for (i, text) in texts.iter().enumerate() {
        if text.len() > MAX_MCP_EMBED_TEXT_BYTES {
            return over_cap(format!(
                "{EMBED_TEXTS_OVER_CAP} text {i} is {} bytes, over the per-text cap of {MAX_MCP_EMBED_TEXT_BYTES}",
                text.len()
            ));
        }
        total += text.len();
    }
    if total > MAX_MCP_EMBED_TOTAL_BYTES {
        return over_cap(format!(
            "{EMBED_TEXTS_OVER_CAP} {total} bytes in total, over the per-call cap of {MAX_MCP_EMBED_TOTAL_BYTES}"
        ));
    }
    Ok(())
}

/// CLI-only daemon operations reuse resident models without exposing raw vectors to agents.
const HIDDEN_TOOLS: &[&str] = &["embed_texts", "rerank_pairs"];

/// Report capped values through `_meta.clamps` as requested/applied pairs.
fn capped_limit_tracked(
    value: Option<usize>,
    default: usize,
    max: usize,
    param: &'static str,
) -> (usize, Option<render::ClampNote>) {
    let requested = value.unwrap_or(default);
    if requested > max {
        (
            max,
            Some(render::ClampNote {
                param,
                requested: serde_json::json!(requested),
                applied: serde_json::json!(max),
            }),
        )
    } else {
        (requested, None)
    }
}

/// Preserve `Some(0)` as unbounded; only omission takes the default.
fn capped_limit_or_unbounded(
    value: Option<usize>,
    default: usize,
    max: usize,
    param: &'static str,
) -> (usize, Option<render::ClampNote>) {
    match value {
        None => (default, None),
        Some(0) => (0, None),
        Some(n) => capped_limit_tracked(Some(n), default, max, param),
    }
}

fn capped_optional_limit_tracked(
    value: Option<usize>,
    max: usize,
    param: &'static str,
) -> (Option<usize>, Option<render::ClampNote>) {
    match value {
        Some(n) if n > max => (
            Some(max),
            Some(render::ClampNote {
                param,
                requested: serde_json::json!(n),
                applied: serde_json::json!(max),
            }),
        ),
        other => (other, None),
    }
}

/// Preserve graph-layer normalization and disclose adjustments through `_meta.clamps`.
fn clamp_min_jaccard_tracked(requested: Option<f32>) -> (f32, Option<render::ClampNote>) {
    const DEFAULT: f32 = 0.85;
    let Some(v) = requested else {
        return (DEFAULT, None);
    };
    let applied = if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        DEFAULT
    };
    let note = (applied != v || !v.is_finite()).then(|| render::ClampNote {
        param: "min_jaccard",
        requested: serde_json::json!(v),
        applied: serde_json::json!(applied),
    });
    (applied, note)
}

fn session_start_report(
    root: &Path,
    snapshot: codesage_protocol::SessionSnapshot,
) -> SessionStartReport {
    let snapshot_path = root
        .join(".codesage")
        .join("sessions")
        .join(format!("{}.json", snapshot.session_id));
    SessionStartReport {
        session_id: snapshot.session_id,
        created_at: snapshot.created_at,
        file_count: snapshot.file_count,
        symbol_count: snapshot.symbol_count,
        cycle_count: snapshot.cycles.len(),
        top_risk_file_count: snapshot.top_risk_files.len(),
        snapshot_path: snapshot_path.to_string_lossy().into_owned(),
        git_head: snapshot.git_head,
    }
}

/// `fields` names every list argument the call carried, so the remedy retries
/// with each of them cut to the cap: rewriting one of an accepted pair would
/// leave the retry contradicting itself.
fn validate_file_list_len(paths: &[String], tool: &str, fields: &[&str]) -> Result<()> {
    if paths.len() > MAX_MCP_FILE_PATHS {
        return Err(error::McpError::new(
            error::ErrorCode::OverCap,
            format!(
                "{tool} accepts at most {MAX_MCP_FILE_PATHS} file paths per call (got {})",
                paths.len()
            ),
        )
        .remedy(error::Remedy::retry_with_all(
            fields,
            serde_json::json!(paths[..MAX_MCP_FILE_PATHS]),
        ))
        .into());
    }
    Ok(())
}

fn validate_non_empty_file_list(paths: &[String], tool: &str, fields: &[&str]) -> Result<()> {
    validate_file_list_len(paths, tool, fields)?;
    if paths.is_empty() {
        return Err(error::McpError::new(
            error::ErrorCode::EmptyInput,
            format!(
                "{tool}: no file paths provided (pass at least one file path as args or pipe via stdin)"
            ),
        )
        .into());
    }
    Ok(())
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

    /// Keep SQLite/model work off async workers so initialize/ping remain responsive.
    /// Convert panics to tool errors; uncaught handler panics otherwise leave clients waiting.
    async fn blocking<F>(&self, f: F) -> CallToolResult
    where
        F: FnOnce(&Self) -> CallToolResult + Send + 'static,
    {
        if let Ok(request) = dispatch::CURRENT_REQUEST.try_with(Arc::clone) {
            return self.controlled_blocking(request, f).await;
        }
        let this = self.clone();
        match tokio::task::spawn_blocking(move || f(&this)).await {
            Ok(result) => result,
            Err(join_err) => {
                tracing::error!(error = %join_err, "MCP tool handler panicked");
                // Only reached outside `dispatch_tool`, which always scopes a request.
                next::annotate(
                    "",
                    "",
                    error::render_mcp_error(
                        "",
                        None,
                        error::McpError::new(
                            error::ErrorCode::Internal,
                            format!(
                                "internal error: the tool handler panicked ({join_err}); see the daemon log"
                            ),
                        ),
                    ),
                )
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeSageServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        self.dispatch_tool(request, context).await
    }

    fn get_info(&self) -> ServerConfig {
        use rmcp::model::ServerCapabilities;
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codesage", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Structural and semantic code intelligence across multiple projects. \
                 Pass an absolute `project` path. Indexed retrieval requires an onboarded \
                 CodeSage project (one containing .codesage/index.db); help works in any \
                 existing directory, and edit_check reads pinned Git HEAD without an index. \
                 Use find_symbol to locate definitions, find_references to trace callers \
                 and imports, list_dependencies for file-level dependency mapping, search \
                 for natural-language semantic code search, impact_analysis to estimate \
                 blast radius of a change, and export_context to bundle code for an LLM.",
            )
    }

    // Defining list_tools suppresses the macro version, allowing strict-client schema
    // cleanup and tool annotations before advertisement.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListToolsResult::with_all_items(
            self.advertised_tools(),
        ))
    }
}

impl CodeSageServer {
    fn advertised_tools(&self) -> Vec<rmcp::model::Tool> {
        let mut tools = self.tool_router.list_all();
        tools.retain(|t| !HIDDEN_TOOLS.contains(&t.name.as_ref()));
        finalize_tools_for_listing(&mut tools);
        tools
    }
}

#[tool_router]
impl CodeSageServer {
    #[tool(
        name = "help",
        description = "Explain tool or field semantics, recovery, investigation recipes, and measured daemon latency. Use when arguments or evidence need interpretation; use describe for facts about code. Requires an absolute existing project directory, without an index or model. Select tool=daemon for operator tools.",
        output_schema = schema_for_type::<help::HelpResult>()
    )]
    async fn help_tool(&self, Parameters(params): Parameters<HelpParams>) -> CallToolResult {
        self.blocking(move |server| {
            let result = help::project_root(Path::new(&params.project))
                .and_then(|_| help::answer(server, &params));
            match result {
                Ok(result) => next::annotate(
                    &params.project,
                    "help",
                    CallToolResult::structured(
                        serde_json::to_value(result).expect("help serializes"),
                    ),
                ),
                Err(error) => error::render_error("help", None, &error),
            }
        })
        .await
    }

    #[tool(
        name = "describe",
        description = "Inspect one indexed file, symbol, feature, or directory: facts, dependencies, tests, risk, and follow-up calls. Use project_overview for project orientation or export_context for source bundles. Ambiguous targets return candidates; unavailable sections disclose recovery.",
        output_schema = schema_for_type::<DescribeResult>()
    )]
    async fn describe_tool(
        &self,
        Parameters(params): Parameters<DescribeParams>,
    ) -> CallToolResult {
        self.blocking(move |server| {
            let result = (|| {
                let state = server.resolve_project_read_only(&params.project)?;
                let db = codesage_storage::Database::open_read_only(&state.db_path)?;
                let root = state
                    .db_path
                    .parent()
                    .and_then(|path| path.parent())
                    .ok_or_else(|| anyhow::anyhow!("could not derive project root from db path"))?;
                let db = &db;
                let ranking = if server.state.overview_cache_enabled {
                    server.state.overview_cache.ready_for_snapshot(root, db)?
                } else {
                    None
                };
                let _snapshot = db.read_snapshot()?;
                let ranking = match ranking {
                    Some((generation, ranking))
                        if server.state.overview_cache.snapshot_matches(
                            root,
                            db,
                            &generation,
                        )? =>
                    {
                        Some(ranking)
                    }
                    _ => None,
                };
                codesage_graph::describe_with_ranking(
                    root,
                    db,
                    &params.target,
                    &codesage_graph::DescribeOptions {
                        detail: params.detail,
                        sections: params.sections.clone(),
                        ..Default::default()
                    },
                    ranking.as_deref(),
                )
            })();
            server.render_read_only(&params.project, result, "describe")
        })
        .await
    }

    #[tool(
        name = "project_overview",
        description = "Orient once at session start: languages, index freshness, features, entrypoints, risk, boundaries, and test conventions. Uses indexed facts without model inference. Use describe for one entity; use search to locate behavior. Refresh stale structural evidence before relying on it.",
        output_schema = schema_for_type::<ProjectOverview>()
    )]
    async fn project_overview_tool(
        &self,
        Parameters(params): Parameters<ProjectOverviewParams>,
    ) -> CallToolResult {
        if let Ok(request) = dispatch::CURRENT_REQUEST.try_with(Arc::clone) {
            if params.include_tests {
                return self.overview_including_tests(request, params.project).await;
            }
            return self.cached_overview(request, params.project).await;
        }
        self.blocking(move |s| {
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    codesage_graph::build_project_overview_with_options(
                        root,
                        db,
                        params.include_tests,
                    )
                }),
                "project_overview",
            )
        })
        .await
    }

    #[tool(
        name = "review_rehearsal",
        description = "Review an explicit patch file set before committing: ranked objections about risk, tests, cycles, boundaries, and scope, with evidence and suggested checks. Use assess_risk_diff for risk alone. Inspect each objection against the actual patch; this read-only analysis does not run tests.",
        output_schema = schema_for_type::<ReviewRehearsal>()
    )]
    async fn review_rehearsal_tool(
        &self,
        Parameters(params): Parameters<ReviewRehearsalParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let fields = params.list_fields();
            // MCP has no stdin or working-tree fallback for an omitted file set.
            s.render(
                &params.project,
                params.target_args().and_then(|file_paths| {
                    validate_non_empty_file_list(&file_paths, "review_rehearsal", fields)?;
                    let root = crate::evidence_root(Path::new(&params.project))?;
                    if !crate::db_path(&root).try_exists()? {
                        return Ok(codesage_graph::build_branch_only_rehearsal(
                            &root,
                            &file_paths,
                        ));
                    }
                    s.with_project_root_db(&params.project, |root, db| {
                        let file_paths = dispatch::resolve_file_targets(root, db, &file_paths)?;
                        codesage_graph::build_review_rehearsal(root, db, &file_paths)
                    })
                }),
                "review_rehearsal",
            )
        })
        .await
    }

    #[tool(
        name = "edit_check",
        description = "Check a complete replacement declaration against pinned Git HEAD before writing. Reports signature changes, proven incompatible callers where supported, and explicit unknowns. Requires Git, without an index. Use find_references for broader caller discovery. Does not compile, write source, or establish safety.",
        output_schema = schema_for_type::<codesage_graph::edit_check::EditCheckReport>()
    )]
    async fn edit_check_tool(
        &self,
        Parameters(params): Parameters<edit_check::EditCheckParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let result = params.target_arg().and_then(|symbol_name| {
                codesage_graph::edit_check::edit_check(
                    Path::new(&params.project),
                    &params.file_path,
                    &symbol_name,
                    params.line,
                    &params.replacement,
                )
            });
            // Staleness resolves the project without opening a watcher, so a
            // project with no index still answers; one with an index discloses
            // which of the named files moved on disk.
            s.render(&params.project, result, "edit_check")
        })
        .await
    }

    #[tool(
        name = "find_symbol",
        description = "Locate indexed definitions by name or target, with source locations, kinds, and author rationale. Use find_references for uses of a symbol; use search when its name is unknown. Inspect ambiguous matches before choosing a definition.",
        output_schema = schema_for_type::<FindSymbolResults>()
    )]
    async fn find_symbol_tool(
        &self,
        Parameters(params): Parameters<FindSymbolParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                params.target_arg().and_then(|name| {
                    let req = FindSymbolRequest {
                        name,
                        kind: params.kind,
                    };
                    s.with_project_db(&params.project, |db| find_symbol(db, &req))
                }),
                "find_symbol",
            )
        })
        .await
    }

    #[tool(
        name = "find_references",
        description = "Find indexed calls, imports, and other references to a symbol. Results are name-based evidence and may combine same-named definitions; missing rows do not prove absence. Use find_symbol for definitions or impact_analysis for transitive dependents.",
        output_schema = schema_for_type::<FindReferencesResults>()
    )]
    async fn find_references_tool(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                params.target_arg().and_then(|symbol_name| {
                    let req = FindReferencesRequest {
                        symbol_name,
                        kind: params.kind,
                    };
                    s.with_project_db(&params.project, |db| find_references(db, &req))
                }),
                "find_references",
            )
        })
        .await
    }

    #[tool(
        name = "find_similar",
        description = "Find structurally similar functions or methods before applying a fix across copies. Matches AST shape, ignoring identifiers and literals; same-named definitions share the seed set. Use search for semantic similarity or find_symbol for exact definitions. Test files are excluded.",
        output_schema = schema_for_type::<FindSimilarResults>()
    )]
    async fn find_similar_tool(
        &self,
        Parameters(params): Parameters<FindSimilarParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let (min_jaccard, jaccard_clamp) = clamp_min_jaccard_tracked(params.min_jaccard);
            let (limit, limit_clamp) =
                capped_limit_tracked(params.limit, 20, MAX_MCP_LIMIT, "limit");
            let clamps: Vec<render::ClampNote> =
                [jaccard_clamp, limit_clamp].into_iter().flatten().collect();
            let result = s.render(
                &params.project,
                params.target_arg().and_then(|name| {
                    s.with_project_db(&params.project, |db| {
                        find_similar(db, &name, min_jaccard, limit)
                    })
                }),
                "find_similar",
            );
            render::annotate_clamps(result, &clamps)
        })
        .await
    }

    #[tool(
        name = "list_dependencies",
        description = "List a file's immediate imports and importers. Use for direct file dependencies; use find_references for symbol uses or impact_analysis for transitive dependents. Results describe indexed relationships, not an exhaustive runtime dependency graph.",
        output_schema = schema_for_type::<DependencyEntry>()
    )]
    async fn list_dependencies_tool(
        &self,
        Parameters(params): Parameters<ListDependenciesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                params.target_arg().and_then(|target| {
                    s.with_project_root_db(&params.project, |root, db| {
                        let file_path = dispatch::resolve_file_target(root, db, &target)?;
                        list_dependencies(db, &file_path)
                    })
                }),
                "list_dependencies",
            )
        })
        .await
    }

    #[tool(
        name = "search",
        description = "Locate code by behavior or intent using embeddings and optional reranking. Use find_symbol for known definitions or find_references for known uses. Path filtering has approximate recall. Ranking confidence describes score separation, not correctness or proof of absence.",
        output_schema = schema_for_type::<SearchResults>()
    )]
    async fn search_tool(&self, Parameters(params): Parameters<SearchParams>) -> CallToolResult {
        self.blocking(move |s| {
            let languages = params.language.map(|l| vec![l]);
            let (limit, limit_clamp) =
                capped_limit_tracked(params.limit, 10, MAX_MCP_LIMIT, "limit");
            let (offset, offset_clamp) =
                capped_limit_tracked(params.offset, 0, MAX_MCP_OFFSET, "offset");
            let clamps: Vec<render::ClampNote> =
                [limit_clamp, offset_clamp].into_iter().flatten().collect();
            let req = SearchRequest {
                query: params.query,
                limit: Some(limit),
                offset: Some(offset),
                languages,
                paths: params.paths,
                adaptive_limit: params.adaptive_limit.unwrap_or(false),
                explain: params.explain.unwrap_or(false),
            };
            let query_for_embed = req.query.clone();
            // Offset and zero-limit requests cannot establish absence from the corpus.
            let paged = req.offset.unwrap_or(0) > 0 || req.limit == Some(0);
            let result = s.render_coverage_gated(
                &params.project,
                req.paths
                    .as_ref()
                    .map(|paths| validate_file_list_len(paths, "search.paths", &["paths"]))
                    .unwrap_or(Ok(()))
                    .and_then(|()| {
                        s.with_project_query(&params.project, &query_for_embed, |db, emb, rr| {
                            search_page(db, emb, rr, &req)
                        })
                    }),
                "search",
                !paged,
            );
            let result = render::annotate_test_override(result, Self::test_override_active());
            render::annotate_clamps(result, &clamps)
        })
        .await
    }

    #[tool(
        name = "embed_texts",
        description = "Internal: embed chunk texts with this daemon's resident embedding session. Called by the codesage CLI so index and search reuse one loaded model instead of each process creating its own; not advertised to agents.",
        output_schema = schema_for_type::<EmbedTextsResult>()
    )]
    async fn embed_texts_tool(
        &self,
        Parameters(params): Parameters<EmbedTextsParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            if let Err(refusal) = check_embed_texts_caps(&params.texts) {
                return render::render_with_kind::<()>(
                    Err(anyhow::Error::new(refusal)),
                    "embed_texts",
                );
            }
            // Digest budgeting/truncation would corrupt raw vectors.
            match s.embed_texts_for(
                &params.project,
                &params.model,
                params.fingerprint.as_deref(),
                &params.texts,
            ) {
                Ok(result) => match serde_json::to_value(&result) {
                    Ok(value) => CallToolResult::structured(value),
                    Err(e) => render::render_with_kind::<()>(
                        Err(anyhow::Error::new(e).context("embed_texts: serializing result")),
                        "embed_texts",
                    ),
                },
                Err(e) => render::render_with_kind::<()>(Err(e), "embed_texts"),
            }
        })
        .await
    }

    #[tool(
        name = "rerank_pairs",
        description = "Internal: score query/document pairs with the shared reranker session for the codesage CLI; not advertised to agents.",
        output_schema = schema_for_type::<RerankPairsResult>()
    )]
    async fn rerank_pairs_tool(
        &self,
        Parameters(params): Parameters<RerankPairsParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        self.blocking(
            move |s| match s.rerank_pairs_for(&params, || context.ct.is_cancelled()) {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(value) => CallToolResult::structured(value),
                    Err(error) => render::render_with_kind::<()>(
                        Err(anyhow::Error::new(error).context("rerank_pairs: serializing result")),
                        "rerank_pairs",
                    ),
                },
                Err(error) => render::render_with_kind::<()>(Err(error), "rerank_pairs"),
            },
        )
        .await
    }

    #[tool(
        name = "trace_call_path",
        description = "Trace a forward call chain between two symbols through resolved indexed edges. Use impact_analysis for reverse dependents or from_trace for an observed stack trace. Missing paths do not prove absence; bounded searches may omit paths or shorter alternatives.",
        output_schema = schema_for_type::<CallPathReport>()
    )]
    async fn trace_call_path_tool(
        &self,
        Parameters(params): Parameters<TracePathParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let (max_depth, depth_clamp) =
                capped_limit_tracked(params.max_depth, 6, MAX_MCP_IMPACT_DEPTH, "max_depth");
            let req = CallPathRequest {
                from: params.from.clone(),
                to: params.to.clone(),
                max_depth,
            };
            let result = s.render(
                &params.project,
                s.with_project_db(&params.project, |db| trace_call_path(db, &req)),
                "trace_call_path",
            );
            render::annotate_clamps(result, depth_clamp.as_slice())
        })
        .await
    }

    #[tool(
        name = "from_trace",
        description = "Map a pasted stack trace or sanitizer report to indexed source locations. Frames disclose resolved, ambiguous, or unresolved matches. Use trace_call_path to investigate graph paths between symbols; use find_references for callers. Paste the original trace without rewriting it.",
        output_schema = schema_for_type::<FromTraceReport>()
    )]
    async fn from_trace_tool(
        &self,
        Parameters(params): Parameters<FromTraceParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            // Unlike list_features, zero selects the default frame limit.
            let (limit, clamp) = capped_limit_tracked(
                params.limit.filter(|&l| l > 0),
                MAX_MCP_TRACE_FRAMES,
                MAX_MCP_TRACE_FRAMES,
                "limit",
            );
            let req = FromTraceRequest {
                trace: params.trace.clone(),
                limit: Some(limit),
            };
            let result = s.render(
                &params.project,
                // Rust module resolution needs the index root's Cargo.toml, not a supplied subdirectory.
                s.with_project_root_db(&params.project, |root, db| from_trace(db, root, &req)),
                "from_trace",
            );
            render::annotate_clamps(result, clamp.as_slice())
        })
        .await
    }

    #[tool(
        name = "impact_analysis",
        description = "Find transitive callers and importers affected by changing a symbol or file. Walks reverse indexed references. Use list_dependencies for immediate file imports or find_references for individual uses. Missing dependents do not prove absence; inspect completeness before treating the result as exhaustive.",
        output_schema = schema_for_type::<ImpactReport>()
    )]
    async fn impact_analysis_tool(
        &self,
        Parameters(params): Parameters<ImpactParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let (depth, depth_clamp) =
                capped_limit_tracked(params.depth, 2, MAX_MCP_IMPACT_DEPTH, "depth");
            let (limit, limit_clamp) =
                capped_optional_limit_tracked(params.limit, MAX_MCP_IMPACT_LIMIT, "limit");
            let clamps: Vec<render::ClampNote> =
                [depth_clamp, limit_clamp].into_iter().flatten().collect();
            let req = ImpactRequest {
                target: ImpactTarget::from_hint(params.target, params.is_file),
                depth,
                source_only: params.source_only.unwrap_or(false),
            };
            let opts = ImpactOptions {
                include_forward: params.include_forward.unwrap_or(false),
                include_siblings: params.include_siblings.unwrap_or(false),
                limit,
                summary_only: params.summary_only.unwrap_or(false),
            };
            let result = s.render(
                &params.project,
                s.with_project_db(&params.project, |db| {
                    impact_analysis_report(db, &req, &opts)
                }),
                "impact_analysis",
            );
            render::annotate_clamps(result, &clamps)
        })
        .await
    }

    #[tool(
        name = "export_context",
        description = "Bundle source and definitions for an entity or free-form question, optionally with callers and callees. Exact entities resolve before model inference; text uses semantic search. Use describe for a compact fact card or feature_bundle for a mapped feature. Resolve ambiguity with returned handles.",
        output_schema = schema_for_type::<ContextBundle>()
    )]
    async fn export_context_tool(
        &self,
        Parameters(params): Parameters<ExportContextParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let (limit, limit_clamp) =
                capped_limit_tracked(params.limit, 5, MAX_MCP_CONTEXT_LIMIT, "limit");
            let clamps = limit_clamp.as_slice();
            let req = ExportRequest::from_target(
                params.target,
                params.is_symbol.unwrap_or(false),
                limit,
                params.include_callers.unwrap_or(false),
                params.include_callees.unwrap_or(false),
            );
            let budget = s.bundle_budget_chars(&params.project);
            let mut semantic = false;
            let bundle = s
                .with_project_context_db(&params.project, |db| export_context_for_target(db, &req))
                .and_then(|bundle| match bundle {
                    Some(bundle) => Ok(bundle),
                    None => {
                        semantic = true;
                        let query = req.query.as_deref().unwrap_or_default();
                        s.with_project_query(&params.project, query, |db, emb, rr| {
                            export_context(db, emb, rr, &req)
                        })
                    }
                });
            let result = s.render_budget(&params.project, bundle, "export_context", budget);
            let result =
                render::annotate_test_override(result, semantic && Self::test_override_active());
            render::annotate_clamps(result, clamps)
        })
        .await
    }

    #[tool(
        name = "find_coupling",
        description = "Find files that historically change with one file, ranked by decayed co-change evidence. Use recommend_tests for patch test selection or assess_risk for a combined risk score. Missing history is unmeasured; recurring pairs offer stronger evidence than one-off changes.",
        output_schema = schema_for_type::<CouplingReport>()
    )]
    async fn find_coupling_tool(
        &self,
        Parameters(params): Parameters<CouplingParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let (limit, limit_clamp) =
                capped_limit_tracked(params.limit, 10, MAX_MCP_LIMIT, "limit");
            let result = s.render(
                &params.project,
                params.target_arg().and_then(|target| {
                    s.with_project_root_db(&params.project, |root, db| {
                        let file_path = dispatch::resolve_file_target(root, db, &target)?;
                        find_coupling(db, &file_path, limit)
                    })
                }),
                "find_coupling",
            );
            render::annotate_clamps(result, limit_clamp.as_slice())
        })
        .await
    }

    #[tool(
        name = "assess_risk",
        description = "Assess one file's change risk from history and structural signals. Missing history leaves a partial score, not evidence of safety. Use assess_risk_batch for independent file scores or assess_risk_diff for patch rollups. Inspect the evidence before deciding which checks to run.",
        output_schema = schema_for_type::<RiskAssessment>()
    )]
    async fn assess_risk_tool(&self, Parameters(params): Parameters<RiskParams>) -> CallToolResult {
        self.blocking(move |s| {
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                params
                    .target_arg()
                    .and_then(|target| {
                        s.with_project_root_db(&params.project, |root, db| {
                            let file_path = dispatch::resolve_file_target(root, db, &target)?;
                            assess_risk(db, &file_path)
                        })
                    })
                    .map(|mut a| {
                        a.set_verbose(verbose);
                        a
                    }),
                "assess_risk",
            )
        })
        .await
    }

    #[tool(
        name = "assess_risk_diff",
        description = "Aggregate risk across an explicit patch file set, including hotspots, test gaps, unscored files, and touched import cycles. Use assess_risk_batch for independent scores without patch rollups. Touched cycles may predate the patch; compare the base before claiming a new cycle.",
        output_schema = schema_for_type::<RiskDiffAssessment>()
    )]
    async fn assess_risk_diff_tool(
        &self,
        Parameters(params): Parameters<RiskDiffParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let fields = params.list_fields();
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                params
                    .target_args()
                    .and_then(|targets| {
                        validate_non_empty_file_list(&targets, "assess_risk_diff", fields)?;
                        s.with_project_root_db(&params.project, |root, db| {
                            let file_paths = dispatch::resolve_file_targets(root, db, &targets)?;
                            assess_risk_diff(db, &file_paths)
                        })
                    })
                    .map(|mut a| {
                        a.set_verbose(verbose);
                        a
                    }),
                "assess_risk_diff",
            )
        })
        .await
    }

    #[tool(
        name = "assess_risk_batch",
        description = "Score each file in a set independently, without patch aggregation. Use assess_risk for one file or assess_risk_diff for patch rollups and touched cycles. Inspect each file's unscored marker; missing history does not imply low risk.",
        output_schema = schema_for_type::<RiskBatchAssessment>()
    )]
    async fn assess_risk_batch_tool(
        &self,
        Parameters(params): Parameters<RiskBatchParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let fields = params.list_fields();
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                params
                    .target_args()
                    .and_then(|targets| {
                        validate_non_empty_file_list(&targets, "assess_risk_batch", fields)?;
                        s.with_project_root_db(&params.project, |root, db| {
                            let file_paths = dispatch::resolve_file_targets(root, db, &targets)?;
                            assess_risk_batch(db, &file_paths)
                        })
                    })
                    .map(|mut a| {
                        a.set_verbose(verbose);
                        a
                    }),
                "assess_risk_batch",
            )
        })
        .await
    }

    #[tool(
        name = "recommend_tests",
        description = "Recommend tests for changed files using naming conventions, co-change history, and bounded graph reachability. Put priority inputs first because they share a walk budget. Use assess_risk_diff for patch risk. These recommendations are a lower bound, not permission to skip required suites.",
        output_schema = schema_for_type::<TestRecommendations>()
    )]
    async fn recommend_tests_tool(
        &self,
        Parameters(params): Parameters<TestsForParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let fields = params.list_fields();
            s.render(
                &params.project,
                params.target_args().and_then(|targets| {
                    validate_non_empty_file_list(&targets, "recommend_tests", fields)?;
                    // Relativize absolute inputs against the index root, not a supplied subdirectory.
                    s.with_project_root_db(&params.project, |root, db| {
                        let file_paths = dispatch::resolve_file_targets(root, db, &targets)?;
                        let opts = codesage_graph::ReachabilityOptions {
                            project_root: Some(root.to_path_buf()),
                            ..codesage_graph::ReachabilityOptions::default()
                        };
                        recommend_tests_with_reachability(db, &file_paths, &opts)
                    })
                }),
                "recommend_tests",
            )
        })
        .await
    }

    #[tool(
        name = "session_start",
        description = "Save the indexed structural baseline before authorized edits, using a distinct session_id for concurrent sessions. Reusing an ID replaces its baseline. Pair with session_end after refreshing the index to inspect graph changes. Use project_overview for orientation without persisting a snapshot.",
        output_schema = schema_for_type::<SessionStartReport>()
    )]
    async fn session_start_tool(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> CallToolResult {
        if let Ok(request) = dispatch::CURRENT_REQUEST.try_with(Arc::clone) {
            return self
                .cached_session_start(
                    request,
                    params.project,
                    params.session_id.unwrap_or_else(|| "default".to_string()),
                )
                .await;
        }
        self.blocking(move |s| {
            let session_id = params
                .session_id
                .clone()
                .unwrap_or_else(|| "default".to_string());
            s.render(
                &params.project,
                s.with_project_root_db(&params.project, |root, db| {
                    let snapshot = session_start(root, db, &session_id)?;
                    Ok(session_start_report(root, snapshot))
                }),
                "session_start",
            )
        })
        .await
    }

    #[tool(
        name = "list_features",
        description = "Discover mapped behavior slices, filtered by kind, language, or tag. Each feature groups entrypoints, owned files, context, tests, and trust boundaries. Use find_feature to locate a file's features or feature_bundle to read a selected slice.",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    async fn list_features_tool(
        &self,
        Parameters(params): Parameters<ListFeaturesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let kind = params.kind;
            let language = params.language;
            let tag = params.tag.clone();
            let since = params.since.clone();
            // Limit zero remains unbounded; response budgeting still applies.
            let (limit, limit_clamp) =
                capped_limit_or_unbounded(params.limit, 100, MAX_MCP_FEATURE_LIMIT, "limit");
            let clamps = limit_clamp.as_slice();
            // SQL LIMIT precedes the diff filter: overfetch to the scan cap, then truncate.
            // Explicit limit zero also removes the scan cap.
            let query_limit = if since.is_some() && limit != 0 {
                MAX_MCP_FEATURE_SCAN_LIMIT
            } else {
                limit
            };
            let result = s.render(
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
            );
            render::annotate_clamps(result, clamps)
        })
        .await
    }

    #[tool(
        name = "find_feature",
        description = "Find mapped features containing a file as entrypoint, owned code, context, or test. An empty result means no mapped feature claims the file. Use list_features to discover slices or feature_bundle to read a matched feature.",
        output_schema = schema_for_type::<FeatureListResults>()
    )]
    async fn find_feature_tool(
        &self,
        Parameters(params): Parameters<FindFeatureParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            s.render(
                &params.project,
                params.target_arg().and_then(|target| {
                    s.with_project_root_db(&params.project, |root, db| {
                        let file = dispatch::resolve_file_target(root, db, &target)?;
                        db.features_for_file(&file)
                    })
                }),
                "find_feature",
            )
        })
        .await
    }

    #[tool(
        name = "feature_bundle",
        description = "Read source and definitions for a mapped feature's entrypoints, owned files, tests, and context. Use list_features or find_feature to discover the feature; use export_context for other targets. Inspect found and completeness before treating an empty bundle as a missing feature.",
        output_schema = schema_for_type::<ContextBundle>()
    )]
    async fn feature_bundle_tool(
        &self,
        Parameters(params): Parameters<FeatureBundleParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let include_callers = params.include_callers.unwrap_or(false);
            let include_callees = params.include_callees.unwrap_or(false);
            let (limit, limit_clamp) =
                capped_limit_tracked(params.limit, 5, MAX_MCP_CONTEXT_LIMIT, "limit");
            // Select the configured model's table; the default table may contain no chunks.
            let budget = s.bundle_budget_chars(&params.project);
            let result = s.render_budget(
                &params.project,
                params.target_arg().and_then(|feature_id| {
                    s.with_project_context_db(&params.project, |db| {
                        feature_bundle(db, &feature_id, include_callers, include_callees, limit)
                    })
                }),
                "feature_bundle",
                budget,
            );
            render::annotate_clamps(result, limit_clamp.as_slice())
        })
        .await
    }

    #[tool(
        name = "session_end",
        description = "Compare current indexed structure with a session_start baseline: cycles, risk changes, and added or removed files. Refresh the index after edits before comparing. Requires the matching saved session_id; the baseline remains available for repeated comparisons. Use review_rehearsal for patch objections without a baseline.",
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
            let result = s
                .with_project_root_db(&params.project, |root, db| {
                    session_end(root, db, &session_id)
                })
                .map_err(|e| missing_snapshot_error(&params.project, &session_id, e));
            s.render(&params.project, result, "session_end")
        })
        .await
    }
}

/// A missing snapshot file is the one `session_end` failure with a machine remedy.
fn missing_snapshot_error(project: &str, session_id: &str, error: anyhow::Error) -> anyhow::Error {
    let missing = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    });
    if !missing {
        return error;
    }
    error::McpError::new(
        error::ErrorCode::NotFound,
        format!("no session snapshot `{session_id}`; call session_start first"),
    )
    .remedy(error::Remedy::Call {
        tool: "session_start".to_owned(),
        arguments: serde_json::json!({ "project": project, "session_id": session_id }),
    })
    .source(error)
    .into()
}

/// Fill omitted/empty projects on tools/call messages and batches; preserve other lines.
pub(crate) fn inject_default_project_line(line: &str, default: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return line.to_string();
    }
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return line.to_string();
    };
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

/// Fill omitted/null arguments and missing/empty projects; leave malformed values for validation.
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

/// Forward bounded JSONL, supplying the optional default project on tools/call.
pub(crate) async fn pump_lines<R, W>(
    reader: R,
    mut writer: W,
    default_project: Option<String>,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(framing::FrameReader::new(reader)).lines();
    while let Some(line) = lines.next_line().await? {
        let out = match &default_project {
            Some(project) => inject_default_project_line(&line, project),
            None => line,
        };
        if out.len() > framing::MAX_FRAME_BYTES {
            return Err(framing::oversized_frame());
        }
        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

pub async fn run_mcp_server(default_project: Option<String>) -> Result<()> {
    let (mut feed, server_read) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        if let Err(error) = pump_lines(tokio::io::stdin(), &mut feed, default_project).await {
            tracing::error!(%error, "MCP input rejected");
            std::process::exit(1);
        }
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
    // Tokio's stdin read cannot be cancelled; runtime drop could wait forever.
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use codesage_protocol::SymbolKind;
    use codesage_storage::Database;
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn embed_texts_caps_bound_count_per_text_and_total_bytes() {
        let ok: Vec<String> = vec!["a".repeat(MAX_MCP_EMBED_TEXT_BYTES); 3];
        assert!(check_embed_texts_caps(&ok).is_ok());

        let too_many: Vec<String> = vec![String::from("x"); MAX_MCP_EMBED_TEXTS + 1];
        let err = check_embed_texts_caps(&too_many).unwrap_err();
        assert_eq!(err.code, error::ErrorCode::OverCap);
        let err = err.to_string();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("per-call cap"), "{err}");

        let one_big = vec![String::from("ok"), "b".repeat(MAX_MCP_EMBED_TEXT_BYTES + 1)];
        let err = check_embed_texts_caps(&one_big).unwrap_err().to_string();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("text 1 is"), "{err}");
        assert!(err.contains("per-text cap"), "{err}");

        let per_text = MAX_MCP_EMBED_TEXT_BYTES;
        let count = MAX_MCP_EMBED_TOTAL_BYTES / per_text + 1;
        assert!(
            count <= MAX_MCP_EMBED_TEXTS,
            "fixture must stay under the count cap"
        );
        let aggregate: Vec<String> = vec!["c".repeat(per_text); count];
        let err = check_embed_texts_caps(&aggregate).unwrap_err().to_string();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("in total"), "{err}");
    }

    #[test]
    fn typed_kind_params_advertise_enum_in_input_schema() {
        let server = CodeSageServer::new();
        let tools = server.tool_router.list_all();
        let find_symbol = tools
            .iter()
            .find(|t| t.name == "find_symbol")
            .expect("find_symbol registered");
        let schema = serde_json::to_string(&*find_symbol.input_schema).unwrap();
        assert!(
            schema.contains("namespace"),
            "find_symbol input schema must enumerate SymbolKind values, got: {schema}"
        );
    }

    #[tokio::test]
    async fn rehearsal_returns_branch_evidence_without_onboarding() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "base"]);
        std::fs::write(dir.path().join("shared.rs"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        git(&["checkout", "-b", "sibling"]);
        std::fs::write(dir.path().join("shared.rs"), "sibling\n").unwrap();
        git(&["commit", "-am", "sibling"]);
        git(&["checkout", "-b", "current", "base"]);
        std::fs::write(dir.path().join("current.rs"), "current\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "current"]);
        let server = CodeSageServer::new();
        let result = server
            .review_rehearsal_tool(Parameters(ReviewRehearsalParams {
                project: dir.path().to_string_lossy().into_owned(),
                file_paths: None,
                targets: Some(vec!["shared.rs".to_string()]),
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value = result.structured_content.unwrap();
        assert_eq!(value["objections"][0]["category"], "branch-overlap");
        assert!(
            value["summary_notes"][0]
                .as_str()
                .unwrap()
                .contains("did not run")
        );
        assert!(!dir.path().join(".codesage").exists());
    }

    /// Exercise rmcp's Parameters extractor over transport, including its tool-error envelope.
    #[tokio::test]
    async fn tools_call_with_unknown_argument_is_refused_naming_the_field() {
        use rmcp::ServiceExt;
        use rmcp::model::CallToolRequestParams;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = CodeSageServer::new().serve(server_io).await.unwrap();
            let _ = service.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();

        let mut args = serde_json::Map::new();
        args.insert("project".into(), json!("/nonexistent"));
        args.insert("query".into(), json!("auth"));
        args.insert("max_results".into(), json!(5));
        let result = client
            .call_tool(CallToolRequestParams::new("search").with_arguments(args))
            .await
            .expect("argument refusal is a tool result, not a protocol error");

        assert_eq!(
            result.is_error,
            Some(true),
            "unknown argument must fail the call instead of running with defaults: {result:?}"
        );
        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.starts_with("failed to deserialize parameters"),
            "refusal must come from parameter validation, not the tool body: {text:?}"
        );
        for needle in ["max_results", "expected", "`limit`"] {
            assert!(
                text.contains(needle),
                "refusal must contain {needle:?}, got: {text:?}"
            );
        }

        client.cancel().await.unwrap();
        server_task.await.unwrap();
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
                is_test: false,
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
                visibility: None,
                is_test: false,
                overloaded: false,
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
    fn embed_texts_is_routed_but_never_advertised() {
        let server = CodeSageServer::new();
        let routed = server.tool_router.list_all();
        assert!(
            routed.iter().any(|t| t.name.as_ref() == "embed_texts"),
            "router must dispatch embed_texts"
        );
        let advertised = server.advertised_tools();
        assert!(
            !advertised.iter().any(|t| t.name.as_ref() == "embed_texts"),
            "tools/list must not advertise embed_texts"
        );
        assert_eq!(advertised.len(), routed.len() - HIDDEN_TOOLS.len());
        assert!(advertised.iter().any(|t| t.name.as_ref() == "search"));
    }

    #[test]
    fn rerank_pairs_is_routed_but_never_advertised() {
        let server = CodeSageServer::new();
        assert!(
            server
                .tool_router
                .list_all()
                .iter()
                .any(|t| t.name.as_ref() == "rerank_pairs")
        );
        assert!(
            !server
                .advertised_tools()
                .iter()
                .any(|t| t.name.as_ref() == "rerank_pairs")
        );
    }

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
    fn over_max_limits_cap_with_a_requested_vs_applied_note() {
        let (applied, note) = capped_limit_tracked(Some(10_000), 10, MAX_MCP_LIMIT, "limit");
        assert_eq!(applied, MAX_MCP_LIMIT);
        let note = note.expect("over-max must produce a clamp note");
        assert_eq!(note.param, "limit");
        assert_eq!(note.requested, serde_json::json!(10_000));
        assert_eq!(note.applied, serde_json::json!(MAX_MCP_LIMIT));

        assert_eq!(capped_limit_tracked(None, 10, MAX_MCP_LIMIT, "limit").0, 10);
        assert!(
            capped_limit_tracked(None, 10, MAX_MCP_LIMIT, "limit")
                .1
                .is_none()
        );
        assert!(
            capped_limit_tracked(Some(50), 10, MAX_MCP_LIMIT, "limit")
                .1
                .is_none()
        );
        assert!(
            capped_limit_tracked(Some(100), 10, MAX_MCP_LIMIT, "limit")
                .1
                .is_none()
        );

        let (opt, note) = capped_optional_limit_tracked(Some(9999), MAX_MCP_IMPACT_LIMIT, "limit");
        assert_eq!(opt, Some(MAX_MCP_IMPACT_LIMIT));
        assert!(note.is_some());
        assert_eq!(
            capped_optional_limit_tracked(None, MAX_MCP_IMPACT_LIMIT, "limit").0,
            None
        );
    }

    #[test]
    fn list_features_limit_zero_is_unbounded() {
        assert_eq!(
            capped_limit_or_unbounded(None, 100, MAX_MCP_FEATURE_LIMIT, "limit").0,
            100
        );
        let (applied, note) =
            capped_limit_or_unbounded(Some(0), 100, MAX_MCP_FEATURE_LIMIT, "limit");
        assert_eq!(
            applied, 0,
            "limit 0 must stay unbounded, not reset to the default"
        );
        assert!(note.is_none());
        let (applied, note) =
            capped_limit_or_unbounded(Some(50), 100, MAX_MCP_FEATURE_LIMIT, "limit");
        assert_eq!(applied, 50);
        assert!(note.is_none());
        let (applied, note) =
            capped_limit_or_unbounded(Some(50_000), 100, MAX_MCP_FEATURE_LIMIT, "limit");
        assert_eq!(applied, MAX_MCP_FEATURE_LIMIT);
        assert!(note.is_some());
    }

    #[test]
    fn min_jaccard_clamps_to_unit_range_at_the_param_layer() {
        let (applied, note) = clamp_min_jaccard_tracked(None);
        assert_eq!(applied, 0.85);
        assert!(note.is_none());
        let (applied, note) = clamp_min_jaccard_tracked(Some(0.7));
        assert_eq!(applied, 0.7);
        assert!(note.is_none());
        let (applied, note) = clamp_min_jaccard_tracked(Some(1.5));
        assert_eq!(applied, 1.0);
        let note = note.expect("1.5 must clamp with a note");
        assert_eq!(note.param, "min_jaccard");
        assert_eq!(note.requested, serde_json::json!(1.5));
        assert_eq!(note.applied, serde_json::json!(1.0));
        let (applied, note) = clamp_min_jaccard_tracked(Some(-0.2));
        assert_eq!(applied, 0.0);
        assert!(note.is_some());
    }

    #[test]
    fn empty_file_lists_are_rejected_with_the_cli_sentence() {
        for tool in [
            "assess_risk_diff",
            "assess_risk_batch",
            "recommend_tests",
            "review_rehearsal",
        ] {
            let err = validate_non_empty_file_list(&[], tool, &["targets"])
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("no file paths provided") && err.contains("at least one file path"),
                "{tool}: got: {err}"
            );
        }
        assert!(
            validate_non_empty_file_list(&["a.rs".to_string()], "review_rehearsal", &["targets"])
                .is_ok()
        );
    }

    #[test]
    fn an_over_cap_retry_cuts_every_list_the_call_carried() {
        let paths: Vec<String> = (0..=MAX_MCP_FILE_PATHS)
            .map(|i| format!("f{i}.rs"))
            .collect();
        let params: TestsForParams =
            serde_json::from_value(json!({"project": "/p", "file_paths": paths, "targets": paths}))
                .unwrap();
        let error = validate_file_list_len(&paths, "recommend_tests", params.list_fields())
            .unwrap_err()
            .downcast::<error::McpError>()
            .expect("over-cap is an McpError");
        assert_eq!(error.code, error::ErrorCode::OverCap);
        let Some(error::Remedy::Retry { overrides }) = error.remedy else {
            panic!("over-cap must carry a retry remedy");
        };
        let cut = json!(paths[..MAX_MCP_FILE_PATHS]);
        assert_eq!(overrides.get("file_paths"), Some(&cut));
        assert_eq!(overrides.get("targets"), Some(&cut));

        // The retry the caller would actually make: cutting one of two equal
        // lists would leave them naming different sets, so the remedy for an
        // over-cap call would itself be E_PARAM.
        let mut retried = json!({"project": "/p", "file_paths": paths, "targets": paths});
        for (field, value) in overrides {
            retried[field] = value;
        }
        let retried: TestsForParams = serde_json::from_value(retried).unwrap();
        assert_eq!(
            retried.target_args().unwrap().len(),
            MAX_MCP_FILE_PATHS,
            "the remedy must resolve to one list at the cap"
        );
    }
}
