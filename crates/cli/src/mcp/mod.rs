pub(crate) mod params;
mod render;
mod schema;
mod state;

pub(crate) use state::{CodeSageServerState, WATCHER_STOP_WAIT};

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use codesage_graph::{
    assess_risk, assess_risk_batch, assess_risk_diff, export_context, export_context_for_symbol,
    feature_bundle, find_coupling, find_references, find_similar, find_symbol,
    impact_analysis_report, list_dependencies, recommend_tests, search, session_end, session_start,
    trace_call_path,
};
use codesage_protocol::{
    CallPathReport, CallPathRequest, ContextBundle, CouplingReport, DependencyEntry, ExportRequest,
    FeatureListResults, FindReferencesRequest, FindReferencesResults, FindSimilarResults,
    FindSymbolRequest, FindSymbolResults, ImpactOptions, ImpactReport, ImpactRequest, ImpactTarget,
    ProjectOverview, ReviewRehearsal, RiskAssessment, RiskBatchAssessment, RiskDiffAssessment,
    SearchRequest, SearchResults, SessionDiff, SessionStartReport, TestRecommendations,
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::schema_for_type, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerInfo},
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
/// Per-call ceiling for the hidden `embed_texts` tool. The CLI sends one
/// commit batch (50 files) per call, so this is headroom, not a target.
pub(crate) const MAX_MCP_EMBED_TEXTS: usize = 4_096;
/// Per-text byte ceiling for `embed_texts`. A chunk is at most
/// `DEFAULT_CHUNK_SIZE` characters plus its overlap and symbol header; 32×
/// the chunk size in bytes covers four-byte UTF-8 and a dense header many
/// times over, while a whole file pasted as one text is refused.
pub(crate) const MAX_MCP_EMBED_TEXT_BYTES: usize = 32 * codesage_embed::chunk::DEFAULT_CHUNK_SIZE;
/// Aggregate byte ceiling per `embed_texts` call: every text at four bytes
/// per chunk character, times the count cap (24 MiB at the defaults). The
/// count cap alone let one call carry gigabytes.
pub(crate) const MAX_MCP_EMBED_TOTAL_BYTES: usize =
    MAX_MCP_EMBED_TEXTS * 4 * codesage_embed::chunk::DEFAULT_CHUNK_SIZE;
/// Prefix of every `embed_texts` cap refusal. The CLI client keys its
/// private-embedder fallback on it, so a daemon of another build still
/// speaks the same refusal.
pub(crate) const EMBED_TEXTS_OVER_CAP: &str = "embed_texts: over cap:";

/// Refuse an `embed_texts` request whose count, any single text, or total
/// bytes exceed the caps. The message names the cap that was hit.
fn check_embed_texts_caps(texts: &[String]) -> Result<(), String> {
    if texts.len() > MAX_MCP_EMBED_TEXTS {
        return Err(format!(
            "{EMBED_TEXTS_OVER_CAP} {} texts exceeds the per-call cap of {MAX_MCP_EMBED_TEXTS}",
            texts.len()
        ));
    }
    let mut total = 0usize;
    for (i, text) in texts.iter().enumerate() {
        if text.len() > MAX_MCP_EMBED_TEXT_BYTES {
            return Err(format!(
                "{EMBED_TEXTS_OVER_CAP} text {i} is {} bytes, over the per-text cap of {MAX_MCP_EMBED_TEXT_BYTES}",
                text.len()
            ));
        }
        total += text.len();
    }
    if total > MAX_MCP_EMBED_TOTAL_BYTES {
        return Err(format!(
            "{EMBED_TEXTS_OVER_CAP} {total} bytes in total, over the per-call cap of {MAX_MCP_EMBED_TOTAL_BYTES}"
        ));
    }
    Ok(())
}

/// Tools the router dispatches but `tools/list` never advertises: the CLI
/// calls them over the daemon socket so that one process holds the model
/// (and on a GPU device, the one CUDA context), and an agent has no use for
/// raw vectors.
const HIDDEN_TOOLS: &[&str] = &["embed_texts"];

fn capped_limit(value: Option<usize>, default: usize, max: usize) -> usize {
    value.unwrap_or(default).min(max)
}

fn capped_optional_limit(value: Option<usize>, max: usize) -> Option<usize> {
    value.map(|n| n.min(max))
}

fn validate_file_list_len(paths: &[String], tool: &str) -> Result<()> {
    if paths.len() > MAX_MCP_FILE_PATHS {
        anyhow::bail!(
            "{tool} accepts at most {MAX_MCP_FILE_PATHS} file paths per call (got {})",
            paths.len()
        );
    }
    Ok(())
}

fn validate_non_empty_file_list(paths: &[String], tool: &str) -> Result<()> {
    validate_file_list_len(paths, tool)?;
    if paths.is_empty() {
        anyhow::bail!("{tool} requires at least one file path");
    }
    Ok(())
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
    /// and the daemon stops answering even `initialize`/`ping`.
    /// Offloading to the blocking pool keeps the async workers free.
    ///
    /// `spawn_blocking` also gives a panic boundary: rmcp dispatches tool calls
    /// with no `catch_unwind`, so a panic in a handler is otherwise silently
    /// swallowed and the client hangs forever waiting for a reply that never
    /// comes. Here a panic surfaces as a `JoinError` we turn into an
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
                CallToolResult::error(vec![ContentBlock::text(format!(
                    "internal error: the tool handler panicked ({join_err}); see the daemon log"
                ))])
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

    // Override the macro-generated `list_tools` to strip schemars' non-standard
    // numeric `format` annotations (so strict MCP clients don't log "unknown
    // format" warnings) and stamp read-only / closed-world tool annotations (so
    // read-only-gated clients can call the surface). The macro only generates
    // `list_tools` when the impl doesn't already define one.
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
    /// The `tools/list` payload: every routed tool except [`HIDDEN_TOOLS`],
    /// finalized for listing.
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
                    codesage_graph::build_project_overview(root, db)
                }),
                "project_overview",
            )
        })
        .await
    }

    #[tool(
        name = "review_rehearsal",
        description = "Predict the objections a reviewer will likely raise against a patch, BEFORE committing. Input is the patch's file list (e.g. `git diff --name-only`). Returns severity-ranked objections — missing tests, high-risk files, wide blast radius, fix-prone files, churn hotspots, import cycles touched, trust-boundary expansion (≥3 boundaries), feature-test gaps (changed a feature's core files but none of its mapped tests), and scope-spread when a patch touches many unrelated feature areas — each with concrete evidence and the files it concerns, plus paste-ready `summary_notes` (objection counts + risk summary + the exact tests to run). Pure composition of `assess_risk_diff`, `recommend_tests`, index-drift, and feature mapping — read-only, no AI prose. Use as the last step before a commit: fix or consciously accept each objection.",
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
                validate_file_list_len(&file_paths, "review_rehearsal").and_then(|()| {
                    s.with_project_root_db(&params.project, |root, db| {
                        codesage_graph::build_review_rehearsal(root, db, &file_paths)
                    })
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
            let req = FindSymbolRequest {
                name: params.name,
                kind: params.kind,
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
        description = "Find all references to a symbol across the codebase. **Prefer this over Grep for 'where is X called / imported / instantiated?'** — returns structured {file, line, kind, from_symbol} rows with the reference type (call/import/import_binding/inheritance/instantiation/type_hint) already classified — `import` names the module (`./foo.js`) while `import_binding` names the symbol that import introduces (`Foo`), so a JS/TS file that imports a symbol and only uses it as `Foo.method()` or `x instanceof Foo` still appears — instead of raw grep hits that mix definitions, comments, and string literals together. `from_symbol` names the enclosing symbol that makes each reference (the caller), so you get caller→callee edges without re-deriving them from line numbers; it is null for references at file scope (e.g. top-level imports) or in files with no extracted symbols. For the definition itself use `find_symbol`; for transitive blast radius (callers of callers) use `impact_analysis`.",
        output_schema = schema_for_type::<FindReferencesResults>()
    )]
    async fn find_references_tool(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let req = FindReferencesRequest {
                symbol_name: params.name,
                kind: params.kind,
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
        name = "find_similar",
        description = "Find functions/methods structurally similar to a named one (near-clone detection via MinHash over AST shape; identifiers and literals are ignored). Use before editing a function to find its copies so a fix lands everywhere, to spot divergent forks of a helper, or to locate copy-paste during review. Returns {name, file_path, line_start, line_end, kind, jaccard} ranked by similarity (1.0 = structurally identical body). Test files are excluded. Tune `min_jaccard` up for exact clones, down for looser matches.",
        output_schema = schema_for_type::<FindSimilarResults>()
    )]
    async fn find_similar_tool(
        &self,
        Parameters(params): Parameters<FindSimilarParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let min_jaccard = params.min_jaccard.unwrap_or(0.85);
            let limit = capped_limit(params.limit, 20, MAX_MCP_LIMIT);
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| {
                    find_similar(db, &params.name, min_jaccard, limit)
                }),
                "find_similar",
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
        description = "Semantic code search (embedding-based + cross-encoder reranking). **Prefer this over Grep when you don't know the exact symbol name** — useful for queries like 'where is auth handled', 'error handling in the session pipeline', 'database connection pooling', 'where do we validate inputs'. Grep needs the literal token already; `search` lets the agent ask by intent. For exact identifier lookups with a known name, use `find_symbol` or `find_references` instead. `paths` filters are applied after bounded KNN retrieval so large MCP calls stay responsive; recall is approximate when a filter excludes many near-neighbor chunks.",
        output_schema = schema_for_type::<SearchResults>()
    )]
    async fn search_tool(&self, Parameters(params): Parameters<SearchParams>) -> CallToolResult {
        self.blocking(move |s| {
            let languages = params.language.map(|l| vec![l]);
            let req = SearchRequest {
                query: params.query,
                limit: Some(capped_limit(params.limit, 10, MAX_MCP_LIMIT)),
                offset: Some(capped_limit(params.offset, 0, MAX_MCP_OFFSET)),
                languages,
                paths: params.paths,
            };
            let query_for_embed = req.query.clone();
            // A page past the end, or a zero limit, empties the result by
            // request. Coverage would report that as "no matches", which is
            // false about the corpus.
            let paged = req.offset.unwrap_or(0) > 0 || req.limit == Some(0);
            s.render_coverage_gated(
                &params.project,
                req.paths
                    .as_ref()
                    .map(|paths| validate_file_list_len(paths, "search.paths"))
                    .unwrap_or(Ok(()))
                    .and_then(|()| {
                        s.with_project_query(&params.project, &query_for_embed, |db, emb, rr| {
                            search(db, emb, rr, &req)
                        })
                    }),
                "search",
                !paged,
            )
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
                return CallToolResult::error(vec![ContentBlock::text(refusal)]);
            }
            // Raw vectors, not a rendered digest: the budget cap and coverage
            // annotation the other tools go through would corrupt them.
            match s.embed_texts_for(&params.project, &params.model, &params.texts) {
                Ok(result) => match serde_json::to_value(&result) {
                    Ok(value) => CallToolResult::structured(value),
                    Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                        "embed_texts: serializing result: {e}"
                    ))]),
                },
                Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
            }
        })
        .await
    }

    #[tool(
        name = "trace_call_path",
        description = "Shortest call chain from one symbol to another: how does `from` end up calling `to`? Breadth-first over resolved callee edges, so `steps` is a shortest path, each entry naming the symbol, its file:line, and `call_line` — the line in the PREVIOUS step's body where it is invoked. Answers the question `find_references` needs N manual round-trips for, and that `impact_analysis` cannot answer at all because it returns an unordered set rather than a route. Use for `how does request input reach this exec/query/write?` during security review, and for orienting in an unfamiliar callstack. On `found: false` read `note` and `bounded`: `bounded: true` means the search stopped at its depth or breadth limit, so a longer chain may still exist and a bigger `max_depth` may find it — an empty result is NOT proof no path exists. `max_depth` is capped at 6 over MCP, so a `bounded` miss at that depth cannot be retried deeper here — the `codesage trace` CLI searches further. Direction matters: this walks callee edges forward from `from`, so swap the arguments to ask the reverse question.",
        output_schema = schema_for_type::<CallPathReport>()
    )]
    async fn trace_call_path_tool(
        &self,
        Parameters(params): Parameters<TracePathParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let req = CallPathRequest {
                from: params.from.clone(),
                to: params.to.clone(),
                max_depth: capped_limit(params.max_depth, 6, MAX_MCP_IMPACT_DEPTH),
            };
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| trace_call_path(db, &req)),
                "trace_call_path",
            )
        })
        .await
    }

    #[tool(
        name = "impact_analysis",
        description = "Estimate which files are affected by changing a symbol or file. Walks the **reverse** reference graph up to `depth` hops (default 2) — i.e., callers/importers of the target and transitively their callers/importers — reports affected files in `results` ranked by distance and reference count. **Multi-hop blast radius from the target outward to its dependents.** `results` is `[]` for leaf files nothing imports/calls. Opt-in extras (all default off): `include_forward` adds the target's own forward dependencies in `forward_dependencies`; `include_siblings` adds same-file symbols in `sibling_symbols`; `limit` caps `results` (sets `truncated`); `summary_only` drops per-reason detail and attaches a `summary` rollup for wide blast radii. For single-hop importer/importee of one file use `list_dependencies`; for raw call sites of a specific symbol use `find_references`.",
        output_schema = schema_for_type::<ImpactReport>()
    )]
    async fn impact_analysis_tool(
        &self,
        Parameters(params): Parameters<ImpactParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let req = ImpactRequest {
                target: ImpactTarget::from_hint(params.target, params.is_file),
                depth: capped_limit(params.depth, 2, MAX_MCP_IMPACT_DEPTH),
                source_only: params.source_only.unwrap_or(false),
            };
            let opts = ImpactOptions {
                include_forward: params.include_forward.unwrap_or(false),
                include_siblings: params.include_siblings.unwrap_or(false),
                limit: capped_optional_limit(params.limit, MAX_MCP_IMPACT_LIMIT),
                summary_only: params.summary_only.unwrap_or(false),
            };
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| {
                    impact_analysis_report(db, &req, &opts)
                }),
                "impact_analysis",
            )
        })
        .await
    }

    #[tool(
        name = "export_context",
        description = "Build a curated context bundle for a free-form **query** or a single **symbol**: semantic search results, overlapping symbol definitions, and optionally caller/callee code, all wrapped as a structured bundle ready for LLM consumption. Use when the anchor is a phrase ('error handling in the parser') or one named symbol. For an already-mapped feature slice (entrypoint + owned files + tests + context already resolved), use `feature_bundle` instead — that anchors on `feature_id` and avoids re-running semantic search. Symbol entries inside the bundle carry `rationale[]` when the author left `WHY:` / `NOTE:` / `IMPORTANT:` / `FIXME:` / `HACK:` / `XXX:` / `TODO:` comments — preserve these in any synthesis the agent performs from the bundle. Currently extracted for Rust and Python. A missing symbol target sets top-level `found: false` (with an empty bundle) — check that flag, not the `target_description` text.",
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
                capped_limit(params.limit, 5, MAX_MCP_CONTEXT_LIMIT),
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
        description = "Files that historically change together with the given file, ranked by exponentially-decayed weight (τ=180d). Backed by git history. Use when planning a change to know which OTHER files (especially tests) tend to need updates too. Response is `{found: bool, coupled: [...], file_indexed: bool, file_commits: u32, note?: string}` — check `found` (false when the file has no git-history row), then read `coupled` for the ranked list. When `coupled` is empty, `note` disambiguates: file never indexed vs. file has history but no pair above the min-count=3 threshold vs. path shape mismatch. Index into `.coupled`, not the response directly. For the patch-level question 'which tests should I run after editing these files?' use `recommend_tests` instead (resolves test conventions + co-change in one call). For the single-file risk score that already folds in coupling pressure use `assess_risk`.",
        output_schema = schema_for_type::<CouplingReport>()
    )]
    async fn find_coupling_tool(
        &self,
        Parameters(params): Parameters<CouplingParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let limit = capped_limit(params.limit, 10, MAX_MCP_LIMIT);
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
        description = "Risk score for changing one file: blends seven signals — churn percentile, fix ratio, blast radius (depth-2 reverse deps), historical coupling, test-gap, import-cycle membership, and trust-boundary count — into a 0..1 score. Response carries `score`, `notes[]`, the `trust_boundaries[]` list, `top_symbols[]` (up to 5 symbols inside the file ranked by line count + reference count + cycle membership), and `cycle_files[]` when the file sits in an import cycle. Notes are paste-ready for PR descriptions: the `crosses N trust boundaries` line fires when ≥3 boundaries cross, and the `in import cycle of N files` line names up to five members. Pass `verbose: true` to also get the per-signal decomposition (`churn_score`, `churn_percentile`, `fix_ratio`, `total_commits`, `fix_count`, `dependent_files`, `coupled_files`, `test_gap`, `in_cycle`, `cycle_size`) and `top_coupled[]`; the default omits them. Use BEFORE writing a patch to calibrate caution and BEFORE submitting to flag concerns. For per-file scoring across N files in one call use `assess_risk_batch`; for patch-level aggregation (max/mean, summary_notes, cycles touching the patch) use `assess_risk_diff`.",
        output_schema = schema_for_type::<RiskAssessment>()
    )]
    async fn assess_risk_tool(&self, Parameters(params): Parameters<RiskParams>) -> CallToolResult {
        self.blocking(move |s| {
            let file_path = params.file_path.clone();
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                s.with_project_db(&params.project, |db| assess_risk(db, &file_path))
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
        description = "Aggregate risk for a SET of files (the file list of a patch or PR). Returns per-file entries (same shape as `assess_risk`, including its `verbose` gating) plus rollups: max_score, mean_score, max_risk_file, and lists of files in each risk category (test_gap, hotspot, fix-heavy, wide blast radius). Use BEFORE submitting a patch: if max_score is high or any test_gap_files exist, add tests, split the patch, or flag concerns. summary_notes are paste-ready for a PR description. On large patches that touch ≥5 files from one directory, per-file entries for that directory move from `files` into a `clustered_directories[]` entry (top-3 by score preserved in detail, rest by name); rollup arrays still list every clustered file by name, so cross-referencing still works. `cycles_touching_patch[]` lists import cycles (files that mutually depend via import/include/inheritance/trait_use) that include at least one patch file, each with `members`, `size`, and `max_churn_file` (best refactor target). Honest caveat: we can't distinguish cycles the patch introduced from cycles that already existed; phrase PR feedback as 'this patch touches an existing cycle' unless you've verified the base branch.",
        output_schema = schema_for_type::<RiskDiffAssessment>()
    )]
    async fn assess_risk_diff_tool(
        &self,
        Parameters(params): Parameters<RiskDiffParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                validate_non_empty_file_list(&file_paths, "assess_risk_diff")
                    .and_then(|()| {
                        s.with_project_db(&params.project, |db| assess_risk_diff(db, &file_paths))
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
        description = "Risk score for EACH of N files, returned per-file with no patch-level aggregation. Use when you have a list of files (impact analysis output, coupling neighbours, the files of a feature you're touching one-by-one) and want each individual score — cuts the per-file MCP round-trip overhead vs calling `assess_risk` N times. Each entry is a RiskAssessment with the same shape as `assess_risk`, including its `verbose` gating of the decomposition scalars and `top_coupled`. The response also includes a top-level `_legend` short-code map: when ≥3 files in the batch share a categorical note (test-gap, no-git-history), per-file `notes[]` entries are aliased to short codes (e.g. `\"T\"`, `\"NG\"`) and the legend resolves them. For patch-level aggregation (max/mean, hotspot/test-gap rollups, cycles), use `assess_risk_diff` instead — they answer different questions.",
        output_schema = schema_for_type::<RiskBatchAssessment>()
    )]
    async fn assess_risk_batch_tool(
        &self,
        Parameters(params): Parameters<RiskBatchParams>,
    ) -> CallToolResult {
        self.blocking(move |s| {
            let file_paths = params.file_paths.clone();
            let verbose = params.verbose.unwrap_or(false);
            s.render(
                &params.project,
                validate_non_empty_file_list(&file_paths, "assess_risk_batch")
                    .and_then(|()| {
                        s.with_project_db(&params.project, |db| assess_risk_batch(db, &file_paths))
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
                validate_non_empty_file_list(&file_paths, "recommend_tests").and_then(|()| {
                    s.with_project_db(&params.project, |db| recommend_tests(db, &file_paths))
                }),
                "recommend_tests",
            )
        })
        .await
    }

    #[tool(
        name = "session_start",
        description = "Snapshot the project's structural state at the START of an editing session. Persists the full snapshot (file count, symbol count, full file list, all import cycles, and the top-50 highest-risk files with scores) to `.codesage/sessions/<session_id>.json`, but returns only a compact summary plus `snapshot_path` over MCP so the wire response is never budget-truncated. Pair with `session_end` using the same `session_id` to detect new cycles, removed/added files, or risk regressions on hot files introduced during the session. `session_id` defaults to \"default\" — use a distinct id when running multiple parallel sessions. Re-running `session_start` overwrites the snapshot (useful for resetting a baseline mid-session).",
        output_schema = schema_for_type::<SessionStartReport>()
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
        description = "List feature slices in the project, optionally filtered by kind, language, or tag. A feature is a behavior-keyed bundle (entrypoint + owned files + context + tests + trust boundaries) — e.g. \"Laravel route POST /api/login\", \"Rust binary `codesage`\", \"php-src extension `iconv`\", \"CMake binary `myapp`\". Use this to discover the agent-facing surface area of the project before deep-diving into a specific slice. Pair with `find_feature` (file → features) and `assess_risk` (per-file scoring inside a feature).",
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
            let limit = match params.limit {
                Some(0) | None => 100,
                Some(n) => n.min(MAX_MCP_FEATURE_LIMIT),
            };
            // With `since`, fetch unbounded then cap after the changed-file
            // intersection, mirroring the CLI: the SQL LIMIT runs before the
            // diff filter, so a pre-filter limit would truncate candidates.
            let query_limit = if since.is_some() {
                MAX_MCP_FEATURE_SCAN_LIMIT
            } else {
                limit
            };
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
        description = "Curated code bundle for one feature_id. Same shape as `export_context` but anchored on the feature's already-resolved file list (entry + owned + tests + context) instead of semantic search results. `primary[]` carries chunks from owned/entry files, `related[]` carries tests and context. Set `include_callers` / `include_callees` to also expand the entry symbol's callers/callees into `related[]` (reuses the symbol graph used by `export_context`). Use after `list_features` / `find_feature` to get all the code an agent needs to review or modify the slice in one MCP call — avoids fan-out Read calls per file. Empty bundle with `target_description` ending `(not found)` means the feature_id doesn't exist — that case also sets top-level `found: false`, so check the flag rather than substring-matching. Empty bundle with `found: true` means the feature exists but no files have been semantically indexed yet (run `codesage index`).",
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
            let limit = capped_limit(params.limit, 5, MAX_MCP_CONTEXT_LIMIT);
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
    use codesage_protocol::SymbolKind;
    use codesage_storage::Database;
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn embed_texts_caps_bound_count_per_text_and_total_bytes() {
        let ok: Vec<String> = vec!["a".repeat(MAX_MCP_EMBED_TEXT_BYTES); 3];
        assert_eq!(check_embed_texts_caps(&ok), Ok(()));

        let too_many: Vec<String> = vec![String::from("x"); MAX_MCP_EMBED_TEXTS + 1];
        let err = check_embed_texts_caps(&too_many).unwrap_err();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("per-call cap"), "{err}");

        // One text over the per-text cap is refused even though the count is
        // tiny: the count cap alone let a single call carry a whole file.
        let one_big = vec![String::from("ok"), "b".repeat(MAX_MCP_EMBED_TEXT_BYTES + 1)];
        let err = check_embed_texts_caps(&one_big).unwrap_err();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("text 1 is"), "{err}");
        assert!(err.contains("per-text cap"), "{err}");

        // Every text under the per-text cap, but together over the aggregate.
        let per_text = MAX_MCP_EMBED_TEXT_BYTES;
        let count = MAX_MCP_EMBED_TOTAL_BYTES / per_text + 1;
        assert!(
            count <= MAX_MCP_EMBED_TEXTS,
            "fixture must stay under the count cap"
        );
        let aggregate: Vec<String> = vec!["c".repeat(per_text); count];
        let err = check_embed_texts_caps(&aggregate).unwrap_err();
        assert!(err.starts_with(EMBED_TEXTS_OVER_CAP), "{err}");
        assert!(err.contains("in total"), "{err}");
    }

    #[test]
    fn typed_kind_params_advertise_enum_in_input_schema() {
        // The whole point of the typed params: the generated inputSchema
        // must carry the legal values so agents see them before calling.
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

    /// Every registered MCP tool must carry a valid output schema. Catches
    /// the regression where a tool ships without `output_schema = ...` (then
    /// agents have to guess the response shape) and where the schema's root
    /// is not a JSON object (which the MCP spec requires; rmcp rejects it
    /// at registration time but the assertion here makes the contract
    /// explicit in test output).
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
