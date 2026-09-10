use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use codesage_protocol::work::{StopReason, WorkControl};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use serde_json::json;

use super::CodeSageServer;
use super::diagnostics::{ExecutionTicket, RequestTicket};
use super::work::{ExecutionLease, WorkClass};

tokio::task_local! {
    pub(super) static CURRENT_REQUEST: Arc<ToolRequest>;
}

#[derive(Clone)]
pub(super) struct ToolRequest {
    pub(super) control: WorkControl,
    pub(super) ticket: Arc<RequestTicket>,
    pub(super) project: PathBuf,
    pub(super) tool: String,
    pub(super) class: WorkClass,
}

struct CancelOnDrop(WorkControl);

#[derive(Debug)]
struct NeedsAnalysis;

impl std::fmt::Display for NeedsAnalysis {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cached ranking requires analysis")
    }
}

impl std::error::Error for NeedsAnalysis {}

enum OverviewAttempt {
    Complete(CallToolResult),
    NeedsAnalysis,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotStage {
    BeforeOpen,
    BeforePin,
    AfterPin,
    AfterPersist,
}

#[derive(Default)]
struct SnapshotHooks {
    #[cfg(test)]
    boundary: Option<Box<dyn Fn(SnapshotStage) + Send + Sync>>,
}

impl SnapshotHooks {
    fn reached(&self, _stage: SnapshotStage) {
        #[cfg(test)]
        if let Some(boundary) = &self.boundary {
            boundary(_stage);
        }
    }
}

fn persist_snapshot(
    root: &Path,
    snapshot: &codesage_protocol::SessionSnapshot,
    ticket: &RequestTicket,
    hooks: &SnapshotHooks,
) -> anyhow::Result<()> {
    codesage_graph::persist_session_snapshot(root, snapshot)?;
    hooks.reached(SnapshotStage::AfterPersist);
    ticket.mark_persisted();
    Ok(())
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel(StopReason::ClientCancelled);
    }
}

struct ExecutionResources {
    _lease: Arc<ExecutionLease>,
    ticket: Arc<ExecutionTicket>,
    control: WorkControl,
    outcome: parking_lot::Mutex<&'static str>,
    _registration: parking_lot::Mutex<Option<codesage_protocol::work::CancelRegistration>>,
}

impl Drop for ExecutionResources {
    fn drop(&mut self) {
        let outcome = self
            .control
            .reason()
            .map(StopReason::as_str)
            .unwrap_or(*self.outcome.lock());
        self.ticket.finish(outcome);
    }
}

fn class_for(tool: &str) -> WorkClass {
    match tool {
        "search" | "export_context" | "embed_texts" | "rerank_pairs" => WorkClass::Native,
        "project_overview" | "review_rehearsal" | "assess_risk" | "assess_risk_batch"
        | "assess_risk_diff" | "session_start" | "session_end" | "impact_analysis"
        | "recommend_tests" | "trace_call_path" | "find_similar" => WorkClass::Analysis,
        _ => WorkClass::Interactive,
    }
}

fn validate_arguments(request: &CallToolRequestParams) -> Result<(), serde_json::Error> {
    use super::params::*;
    let value = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
    macro_rules! validate {
        ($ty:ty) => {
            serde_json::from_value::<$ty>(value).map(|_| ())
        };
    }
    match request.name.as_ref() {
        "project_overview" => validate!(ProjectOverviewParams),
        "review_rehearsal" => validate!(ReviewRehearsalParams),
        "edit_check" => validate!(super::edit_check::EditCheckParams),
        "find_symbol" => validate!(FindSymbolParams),
        "find_references" => validate!(FindReferencesParams),
        "find_similar" => validate!(FindSimilarParams),
        "list_dependencies" => validate!(ListDependenciesParams),
        "search" => validate!(SearchParams),
        "embed_texts" => validate!(EmbedTextsParams),
        "rerank_pairs" => validate!(RerankPairsParams),
        "trace_call_path" => validate!(TracePathParams),
        "from_trace" => validate!(FromTraceParams),
        "impact_analysis" => validate!(ImpactParams),
        "export_context" => validate!(ExportContextParams),
        "find_coupling" => validate!(CouplingParams),
        "assess_risk" => validate!(RiskParams),
        "assess_risk_diff" => validate!(RiskDiffParams),
        "assess_risk_batch" => validate!(RiskBatchParams),
        "recommend_tests" => validate!(TestsForParams),
        "session_start" | "session_end" => validate!(SessionParams),
        "list_features" => validate!(ListFeaturesParams),
        "find_feature" => validate!(FindFeatureParams),
        "feature_bundle" => validate!(FeatureBundleParams),
        _ => Ok(()),
    }
}

fn class_name(class: WorkClass) -> &'static str {
    match class {
        WorkClass::Interactive => "interactive",
        WorkClass::Analysis => "analysis",
        WorkClass::Native => "native",
    }
}

fn stopped_result(status: &str, ticket: &RequestTicket) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(
        json!({"status":status}).to_string(),
    )]);
    normalize_error(&mut result, ticket);
    result
}

fn client_cancelled_result(control: &WorkControl, ticket: &RequestTicket) -> CallToolResult {
    control.cancel(StopReason::ClientCancelled);
    let status = control
        .reason()
        .map(StopReason::as_str)
        .unwrap_or("cancelled");
    ticket.finish(status);
    stopped_result(status, ticket)
}

fn result_outcome(result: &CallToolResult) -> &'static str {
    if result.is_error != Some(true) {
        return "success";
    }
    for content in &result.content {
        let Some(text) = content.as_text() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text.text) else {
            continue;
        };
        match value.get("status").and_then(|v| v.as_str()) {
            Some("timeout") => return "timeout",
            Some("cancelled") => return "cancelled",
            Some("saturated") => return "saturated",
            Some("shutdown") => return "shutdown",
            Some("incomplete") => return "incomplete",
            Some("database-busy") => return "database-busy",
            _ => {}
        }
    }
    "error"
}

fn normalize_error(result: &mut CallToolResult, ticket: &RequestTicket) {
    if result.is_error != Some(true) {
        return;
    }
    let status = result_outcome(result);
    let (phase, continuing) = ticket.work_state();
    let metadata = json!({"status":status,"complete":false,"phase":phase,
        "work_continuing":continuing,"next":null,"request_id":ticket.id(),
        "persistence_committed":ticket.persistence_committed()});
    for content in &mut result.content {
        if let Some(text) = content.as_text()
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text.text)
            && value.get("status").is_some()
        {
            *content = ContentBlock::text(metadata.to_string());
            return;
        }
    }
    result
        .content
        .push(ContentBlock::text(metadata.to_string()));
}

fn disclose_ranking_recomputation(result: &mut CallToolResult) {
    let Some(value) = result.structured_content.as_mut() else {
        return;
    };
    value["_meta"]["ranking_recomputed"] = json!(true);
    for content in &mut result.content {
        if let Some(text) = content.as_text()
            && serde_json::from_str::<serde_json::Value>(&text.text).is_ok_and(|v| v.is_object())
        {
            *content = ContentBlock::text(value.to_string());
            break;
        }
    }
    result.content.push(ContentBlock::text("Cached ranking could not be reused; ranking was recomputed for this request's read snapshot."));
}

async fn stopped(control: &WorkControl) {
    let notify = Arc::new(tokio::sync::Notify::new());
    let wake = notify.clone();
    let _registration = control.on_cancel(Arc::new(move || wake.notify_one()));
    loop {
        let notified = notify.notified();
        if control.reason().is_some() {
            return;
        }
        if let Some(deadline) = control.deadline() {
            tokio::select! {
                () = notified => {},
                () = tokio::time::sleep_until(deadline.into()) => {
                    control.cancel(StopReason::DeadlineExceeded);
                }
            }
        } else {
            notified.await;
        }
    }
}

impl CodeSageServer {
    fn snapshot_generation(
        &self,
        root: &Path,
        path: &Path,
        control: &WorkControl,
    ) -> anyhow::Result<Option<super::overview_cache::Generation>> {
        match self
            .state
            .overview_cache
            .generation_in_execution(root, path, control)
        {
            Ok(generation) => Ok(Some(generation)),
            Err(error) if super::overview_cache::is_cache_unavailable(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn with_cached_snapshot<R>(
        &self,
        request: &ToolRequest,
        cached: &super::overview_cache::CachedRanking,
        hooks: &SnapshotHooks,
        assemble: impl FnOnce(
            &Path,
            &codesage_storage::Database,
            &codesage_graph::CompleteRiskRanking,
            bool,
        ) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let path = request.project.join(".codesage/index.db");
        let before = self.snapshot_generation(&request.project, &path, &request.control)?;
        hooks.reached(SnapshotStage::BeforeOpen);
        let state = self.resolve_project(&request.project.to_string_lossy())?;
        let root = state
            .db_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("could not derive project root from db path"))?;
        let db = codesage_storage::Database::open_read_only_strict(&state.db_path)?;
        hooks.reached(SnapshotStage::BeforePin);
        let _snapshot = db.read_snapshot()?;
        hooks.reached(SnapshotStage::AfterPin);
        let after = self.snapshot_generation(root, &path, &request.control)?;
        let changed = !cached.stable
            || before.as_ref() != Some(&cached.generation)
            || before != after
            || !db.path_still_matches_open_file(&state.db_path)?;
        let ranking = if changed {
            if request.class == WorkClass::Interactive {
                return Err(NeedsAnalysis.into());
            }
            Arc::new(codesage_graph::top_risk_ranking(&db)?)
        } else {
            cached.ranking.clone()
        };
        assemble(root, &db, &ranking, changed)
    }

    async fn ranking_for(
        &self,
        request: &ToolRequest,
    ) -> anyhow::Result<super::overview_cache::CachedRanking> {
        self.state
            .overview_cache
            .get(
                &request.project,
                &request.project.join(".codesage/index.db"),
                &request.control,
                super::overview_cache::CacheRequest {
                    coordinator: &self.state.work,
                    diagnostics: &self.state.diagnostics,
                    request: &request.ticket,
                    producer_deadline: Instant::now() + Duration::from_secs(25),
                },
            )
            .await
    }

    pub(super) async fn cached_overview(
        &self,
        request: Arc<ToolRequest>,
        project: String,
    ) -> CallToolResult {
        if !self.state.overview_cache_enabled {
            return self.uncached_overview(request, project).await;
        }
        let cached = match self.ranking_for(&request).await {
            Ok(cached) => cached,
            Err(error) if super::overview_cache::is_cache_unavailable(&error) => {
                return self.uncached_overview(request, project).await;
            }
            Err(error) => {
                return self.render::<codesage_protocol::ProjectOverview>(
                    &project,
                    Err(error),
                    "project_overview",
                );
            }
        };
        self.overview_from_ranking(request, project, cached, SnapshotHooks::default())
            .await
    }

    async fn overview_from_ranking(
        &self,
        request: Arc<ToolRequest>,
        project: String,
        cached: super::overview_cache::CachedRanking,
        hooks: SnapshotHooks,
    ) -> CallToolResult {
        let mut interactive = (*request).clone();
        interactive.class = WorkClass::Interactive;
        let interactive = Arc::new(interactive);
        let snapshot_request = interactive.clone();
        let server = self.clone();
        let render_project = project.clone();
        let attempt = self
            .run_controlled(
                interactive,
                move || {
                    let result = server.with_cached_snapshot(
                        &snapshot_request,
                        &cached,
                        &hooks,
                        |root, db, ranking, _| {
                            codesage_graph::build_project_overview_with_top_risk(root, db, ranking)
                        },
                    );
                    match result {
                        Ok(overview) => Ok(OverviewAttempt::Complete(server.render(
                            &render_project,
                            Ok(overview),
                            "project_overview",
                        ))),
                        Err(error) if error.is::<NeedsAnalysis>() => {
                            Ok(OverviewAttempt::NeedsAnalysis)
                        }
                        Err(error) => Err(error),
                    }
                },
                |attempt| match attempt {
                    OverviewAttempt::Complete(result) => result_outcome(result),
                    OverviewAttempt::NeedsAnalysis => "success",
                },
            )
            .await;
        match attempt {
            Ok(OverviewAttempt::Complete(result)) => result,
            Ok(OverviewAttempt::NeedsAnalysis) => {
                let mut result = self.uncached_overview(request, project).await;
                disclose_ranking_recomputation(&mut result);
                result
            }
            Err(error) => self.render::<codesage_protocol::ProjectOverview>(
                &project,
                Err(error),
                "project_overview",
            ),
        }
    }

    pub(super) async fn cached_session_start(
        &self,
        request: Arc<ToolRequest>,
        project: String,
        session_id: String,
    ) -> CallToolResult {
        if !self.state.overview_cache_enabled {
            return self
                .uncached_session_start(request, project, session_id)
                .await;
        }
        let cached = match self.ranking_for(&request).await {
            Ok(cached) => cached,
            Err(error) if super::overview_cache::is_cache_unavailable(&error) => {
                return self
                    .uncached_session_start(request, project, session_id)
                    .await;
            }
            Err(error) => {
                return self.render::<codesage_protocol::SessionStartReport>(
                    &project,
                    Err(error),
                    "session_start",
                );
            }
        };
        self.session_from_ranking(
            request,
            project,
            session_id,
            cached,
            SnapshotHooks::default(),
        )
        .await
    }

    async fn session_from_ranking(
        &self,
        request: Arc<ToolRequest>,
        project: String,
        session_id: String,
        cached: super::overview_cache::CachedRanking,
        hooks: SnapshotHooks,
    ) -> CallToolResult {
        let snapshot_request = request.clone();
        let ticket = request.ticket.clone();
        self.controlled_blocking(request, move |server| {
            let mut recomputed = false;
            let result = server
                .with_cached_snapshot(
                    &snapshot_request,
                    &cached,
                    &hooks,
                    |root, db, ranking, changed| {
                        recomputed = changed;
                        codesage_graph::build_session_snapshot_with_top_risk(
                            root,
                            db,
                            &session_id,
                            ranking,
                        )
                    },
                )
                .and_then(|snapshot| {
                    persist_snapshot(&snapshot_request.project, &snapshot, &ticket, &hooks)?;
                    Ok(super::session_start_report(
                        &snapshot_request.project,
                        snapshot,
                    ))
                });
            let mut rendered = server.render(&project, result, "session_start");
            if recomputed {
                disclose_ranking_recomputation(&mut rendered);
            }
            rendered
        })
        .await
    }

    async fn uncached_overview(
        &self,
        request: Arc<ToolRequest>,
        project: String,
    ) -> CallToolResult {
        let canonical = request.project.to_string_lossy().into_owned();
        self.controlled_blocking(request, move |server| {
            server.render(
                &project,
                server.with_project_root_db(&canonical, codesage_graph::build_project_overview),
                "project_overview",
            )
        })
        .await
    }

    async fn uncached_session_start(
        &self,
        request: Arc<ToolRequest>,
        project: String,
        session_id: String,
    ) -> CallToolResult {
        let canonical = request.project.to_string_lossy().into_owned();
        let ticket = request.ticket.clone();
        self.controlled_blocking(request, move |server| {
            let result = server.with_project_root_db(&canonical, |root, db| {
                let snapshot = codesage_graph::session_start(root, db, &session_id)?;
                ticket.mark_persisted();
                Ok(super::session_start_report(root, snapshot))
            });
            server.render(&project, result, "session_start")
        })
        .await
    }

    pub(super) async fn dispatch_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == "daemon_stats" {
            let recent = request.arguments.as_ref().and_then(|a| a.get("recent"));
            let recent = match recent {
                None => 20,
                Some(value) => match value.as_u64().filter(|v| *v <= 256) {
                    Some(value) => value as usize,
                    None => {
                        return Ok(CallToolResult::error(vec![ContentBlock::text(
                            "daemon_stats recent must be an integer from 0 to 256",
                        )])
                        .into());
                    }
                },
            };
            let mut snapshot = self.state.diagnostics.snapshot(recent);
            snapshot["overview_cache_enabled"] = json!(self.state.overview_cache_enabled);
            snapshot["work"] = serde_json::to_value(self.state.work.snapshot())
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::structured(snapshot).into());
        }
        let tool = request.name.to_string();
        let class = class_for(&tool);
        let ticket = Arc::new(self.state.diagnostics.begin_request(&tool, None));
        ticket.set_class(class_name(class));
        let timeout = if class == WorkClass::Native {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(25)
        };
        let control = WorkControl::new(Some(Instant::now() + timeout));
        let mut logical = match self.state.work.try_request(&control) {
            Ok(lease) => lease,
            Err(error) => {
                let mut result = super::render::render_with_kind::<()>(Err(error.into()), "");
                normalize_error(&mut result, &ticket);
                ticket.finish(result_outcome(&result));
                return Ok(result.into());
            }
        };
        if let Err(error) = validate_arguments(&request) {
            ticket.finish("error");
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to deserialize parameters: {error}"
            ))])
            .into());
        }
        let _cancel_on_drop = CancelOnDrop(control.clone());
        let raw_project = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("project"))
            .and_then(|p| p.as_str())
            .map(str::to_owned);
        let request_control = control.clone();
        let request_ticket = ticket.clone();
        let operation = async {
            let project = if let Some(raw_project) = raw_project {
                let preflight = Arc::new(ToolRequest {
                    control: request_control.clone(),
                    ticket: request_ticket.clone(),
                    project: PathBuf::from("<preflight>"),
                    tool: tool.clone(),
                    class: WorkClass::Interactive,
                });
                let server = self.clone();
                let evidence_only = matches!(tool.as_str(), "edit_check" | "review_rehearsal");
                let project = self
                    .run_controlled(
                        preflight,
                        move || {
                            if evidence_only {
                                crate::evidence_root(Path::new(&raw_project))
                            } else {
                                let state = server.resolve_project_inner(&raw_project)?;
                                state
                                    .db_path
                                    .parent()
                                    .and_then(Path::parent)
                                    .map(Path::to_path_buf)
                                    .ok_or_else(|| anyhow::anyhow!("invalid project database path"))
                            }
                        },
                        |_| "success",
                    )
                    .await;
                match project {
                    Ok(project) => project,
                    Err(error) => {
                        return Ok(super::render::render_with_kind::<()>(Err(error), "").into());
                    }
                }
            } else {
                PathBuf::from("<unresolved>")
            };
            if let Err(error) = logical.attach_project(&project) {
                return Ok(super::render::render_with_kind::<()>(Err(error.into()), "").into());
            }
            request_ticket.set_project(&project.to_string_lossy());
            let scope = Arc::new(ToolRequest {
                control: request_control,
                ticket: request_ticket,
                project,
                tool,
                class,
            });
            CURRENT_REQUEST
                .scope(
                    scope,
                    self.tool_router
                        .call(ToolCallContext::new(self, request, context.clone())),
                )
                .await
        };
        tokio::select! {
            result = operation => {
                if let Some(reason) = control.reason() {
                    ticket.finish(reason.as_str());
                    return Ok(stopped_result(reason.as_str(), &ticket).into());
                }
                let mut result = result;
                if let Ok(CallToolResponse::Complete(response)) = &mut result {
                    normalize_error(response, &ticket);
                }
                let outcome = match &result {
                    Ok(CallToolResponse::Complete(result)) => result_outcome(result),
                    _ => "error",
                };
                ticket.finish(outcome);
                result
            }
            () = context.ct.cancelled() => {
                Ok(client_cancelled_result(&control, &ticket).into())
            }
            () = stopped(&control) => {
                let status = control.reason().map(StopReason::as_str).unwrap_or("cancelled");
                ticket.finish(status);
                Ok(stopped_result(status, &ticket).into())
            }
        }
    }

    pub(super) async fn run_controlled<R, F>(
        &self,
        request: Arc<ToolRequest>,
        operation: F,
        classify: fn(&R) -> &'static str,
    ) -> anyhow::Result<R>
    where
        R: Send + 'static,
        F: FnOnce() -> anyhow::Result<R> + Send + 'static,
    {
        let ticket = Arc::new(
            self.state
                .diagnostics
                .begin_execution(&request.tool, Some(&request.project.to_string_lossy())),
        );
        ticket.set_class(class_name(request.class));
        request.ticket.link_execution_ticket(&ticket);
        let lease = match self
            .state
            .work
            .acquire_execution(&request.project, request.class, &request.control)
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                let error = anyhow::Error::new(error);
                ticket.finish(super::render::error_status(&error).unwrap_or("error"));
                return Err(error);
            }
        };
        if let Err(error) = request.control.check() {
            ticket.finish(error.reason.as_str());
            return Err(error.into());
        }
        let resources = Arc::new(ExecutionResources {
            _lease: lease,
            ticket,
            control: request.control.clone(),
            outcome: parking_lot::Mutex::new("panic"),
            _registration: parking_lot::Mutex::new(None),
        });
        let weak = Arc::downgrade(&resources);
        let registration = request.control.on_cancel(Arc::new(move || {
            if let Some(resources) = weak.upgrade() {
                resources.ticket.set_consumers(0);
            }
        }));
        *resources._registration.lock() = Some(registration);
        let erased: Arc<dyn Send + Sync> = resources.clone();
        request.control.set_work_lease(Arc::downgrade(&erased));
        drop(erased);
        tokio::task::spawn_blocking(move || {
            let _scope = request.control.enter();
            let phase = if request.class == WorkClass::Native {
                "native"
            } else {
                "running"
            };
            resources.ticket.set_phase(phase);
            request.ticket.set_phase(phase);
            request.control.check()?;
            let result = operation();
            *resources.outcome.lock() = match &result {
                Ok(value) => classify(value),
                Err(error) => super::render::error_status(error).unwrap_or("error"),
            };
            result
        })
        .await
        .map_err(|error| anyhow::anyhow!("tool worker failed: {error}"))?
    }

    pub(super) async fn controlled_blocking<F>(
        &self,
        request: Arc<ToolRequest>,
        f: F,
    ) -> CallToolResult
    where
        F: FnOnce(&Self) -> CallToolResult + Send + 'static,
    {
        let server = self.clone();
        match self
            .run_controlled(request, move || Ok(f(&server)), result_outcome)
            .await
        {
            Ok(result) => result,
            Err(error) => super::render::render_with_kind::<()>(Err(error), ""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServiceExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn index_fixture(directory: &Path, prefix: &str, count: usize) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("config.toml"), "[index]\nwatch = false\n").unwrap();
        let path = directory.join("index.db");
        drop(codesage_storage::Database::open(&path).unwrap());
        let db = rusqlite::Connection::open(path).unwrap();
        for id in 1..=count {
            let file_id = i64::try_from(id).unwrap();
            let file = format!("{prefix}/{id}.rs");
            let name = format!("{prefix}_{id}");
            db.execute(
                "INSERT INTO files(id,path,language,content_hash) VALUES(?1,?2,'rust','x')",
                rusqlite::params![file_id, file],
            )
            .unwrap();
            db.execute("INSERT INTO symbols(file_id,name,qualified_name,kind,line_start,line_end,col_start,col_end) VALUES(?1,?2,?2,'function',1,1,0,1)", rusqlite::params![file_id, name]).unwrap();
            db.execute(
                "INSERT INTO git_files(path,churn_score,total_commits) VALUES(?1,10,10)",
                [file],
            )
            .unwrap();
            let target = format!("{prefix}_{}", id % count + 1);
            db.execute(
                "INSERT INTO refs(from_file_id,to_name,kind,line,col) VALUES(?1,?2,'import',1,0)",
                rusqlite::params![file_id, target],
            )
            .unwrap();
        }
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
            .unwrap();
    }

    fn fixture_request(server: &CodeSageServer, root: &Path, tool: &str) -> Arc<ToolRequest> {
        Arc::new(ToolRequest {
            control: WorkControl::new(None),
            ticket: Arc::new(server.state.diagnostics.begin_request(tool, root.to_str())),
            project: root.to_path_buf(),
            tool: tool.into(),
            class: WorkClass::Analysis,
        })
    }

    async fn replacement_snapshot(stage: SnapshotStage, session: bool) {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "old", 2);
        index_fixture(&root.path().join("replacement"), "new", 3);
        let server = CodeSageServer::new();
        let request = fixture_request(&server, root.path(), "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        assert!(cached.stable);
        assert_eq!(cached.ranking.rows().len(), 2);
        let paths = root.path().to_path_buf();
        let swaps = Arc::new(AtomicUsize::new(0));
        let observed_swaps = swaps.clone();
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |boundary| {
                if boundary == stage {
                    std::fs::rename(paths.join(".codesage"), paths.join("retired")).unwrap();
                    std::fs::rename(paths.join("replacement"), paths.join(".codesage")).unwrap();
                    observed_swaps.fetch_add(1, Ordering::SeqCst);
                }
            })),
        };
        let expected_prefix = if stage == SnapshotStage::BeforeOpen {
            "new"
        } else {
            "old"
        };
        let expected_count = if stage == SnapshotStage::BeforeOpen {
            3
        } else {
            2
        };
        let expected_files: Vec<String> = (1..=expected_count)
            .map(|id| format!("{expected_prefix}/{id}.rs"))
            .collect();
        if session {
            let snapshot = server
                .with_cached_snapshot(&request, &cached, &hooks, |root, db, ranking, changed| {
                    assert!(changed);
                    codesage_graph::build_session_snapshot_with_top_risk(
                        root,
                        db,
                        "replacement",
                        ranking,
                    )
                })
                .unwrap();
            assert_eq!(snapshot.files, expected_files);
            assert_eq!(snapshot.cycles, vec![expected_files.clone()]);
            assert_eq!(snapshot.file_count as usize, expected_count);
            assert_eq!(snapshot.symbol_count as usize, expected_count);
            let mut ranked: Vec<_> = snapshot
                .top_risk_files
                .iter()
                .map(|row| row.file.clone())
                .collect();
            ranked.sort();
            assert_eq!(ranked, expected_files);
            persist_snapshot(
                root.path(),
                &snapshot,
                &request.ticket,
                &SnapshotHooks::default(),
            )
            .unwrap();
            let persisted: codesage_protocol::SessionSnapshot = serde_json::from_slice(
                &std::fs::read(root.path().join(".codesage/sessions/replacement.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(persisted.files, snapshot.files);
            assert_eq!(persisted.cycles, snapshot.cycles);
        } else {
            let overview = server
                .with_cached_snapshot(&request, &cached, &hooks, |root, db, ranking, changed| {
                    assert!(changed);
                    codesage_graph::build_project_overview_with_top_risk(root, db, ranking)
                })
                .unwrap();
            assert_eq!(overview.file_count, expected_count);
            assert_eq!(overview.symbol_count, expected_count);
            let mut ranked: Vec<_> = overview
                .top_risk_files
                .iter()
                .map(|row| row.file.clone())
                .collect();
            ranked.sort();
            assert_eq!(ranked, expected_files);
        }
        assert_eq!(swaps.load(Ordering::SeqCst), 1);
        let current = codesage_storage::Database::open_read_only_strict(
            &root.path().join(".codesage/index.db"),
        )
        .unwrap();
        assert_eq!(
            current.all_file_paths().unwrap(),
            ["new/1.rs", "new/2.rs", "new/3.rs"]
        );
    }

    #[tokio::test]
    async fn overview_replacement_before_open_uses_new_snapshot() {
        replacement_snapshot(SnapshotStage::BeforeOpen, false).await;
    }

    #[tokio::test]
    async fn overview_replacement_before_pin_keeps_opened_snapshot() {
        replacement_snapshot(SnapshotStage::BeforePin, false).await;
    }

    #[tokio::test]
    async fn overview_replacement_after_pin_keeps_pinned_snapshot() {
        replacement_snapshot(SnapshotStage::AfterPin, false).await;
    }

    #[tokio::test]
    async fn session_replacement_before_open_persists_new_snapshot() {
        replacement_snapshot(SnapshotStage::BeforeOpen, true).await;
    }

    #[tokio::test]
    async fn session_replacement_before_pin_persists_opened_snapshot() {
        replacement_snapshot(SnapshotStage::BeforePin, true).await;
    }

    #[tokio::test]
    async fn session_replacement_after_pin_persists_pinned_snapshot() {
        replacement_snapshot(SnapshotStage::AfterPin, true).await;
    }

    #[tokio::test]
    async fn unobserved_path_swap_cannot_mix_ranking_with_another_open_inode() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "cached", 2);
        index_fixture(&root.path().join("replacement"), "opened", 3);
        let server = CodeSageServer::new();
        let request = fixture_request(&server, root.path(), "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        let paths = root.path().to_path_buf();
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |stage| match stage {
                SnapshotStage::BeforeOpen => {
                    std::fs::rename(paths.join(".codesage"), paths.join("cached-index")).unwrap();
                    std::fs::rename(paths.join("replacement"), paths.join(".codesage")).unwrap();
                }
                SnapshotStage::BeforePin => {
                    std::fs::rename(paths.join(".codesage"), paths.join("opened-index")).unwrap();
                    std::fs::rename(paths.join("cached-index"), paths.join(".codesage")).unwrap();
                }
                _ => {}
            })),
        };
        let (overview, snapshot) = server
            .with_cached_snapshot(&request, &cached, &hooks, |root, db, ranking, changed| {
                assert!(
                    changed,
                    "the pathname generation alone misses the opened-inode mismatch"
                );
                Ok((
                    codesage_graph::build_project_overview_with_top_risk(root, db, ranking)?,
                    codesage_graph::build_session_snapshot_with_top_risk(root, db, "aba", ranking)?,
                ))
            })
            .unwrap();
        let after = server
            .state
            .overview_cache
            .generation_in_execution(
                root.path(),
                &root.path().join(".codesage/index.db"),
                &request.control,
            )
            .unwrap();
        assert!(
            after == cached.generation,
            "the observer must not have witnessed the intermediate index"
        );
        let expected = ["opened/1.rs", "opened/2.rs", "opened/3.rs"];
        assert_eq!(overview.file_count, 3);
        assert_eq!(overview.symbol_count, 3);
        let mut overview_files: Vec<_> = overview
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        overview_files.sort();
        assert_eq!(overview_files, expected);
        assert_eq!(snapshot.files, expected);
        assert_eq!(snapshot.cycles, vec![expected.to_vec()]);
        let mut snapshot_files: Vec<_> = snapshot
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        snapshot_files.sort();
        assert_eq!(snapshot_files, expected);
    }

    #[tokio::test]
    async fn project_alias_retargeting_keeps_the_canonical_request_snapshot() {
        let fixture = tempfile::tempdir().unwrap();
        let original = fixture.path().join("original");
        let replacement = fixture.path().join("replacement");
        let alias = fixture.path().join("alias");
        index_fixture(&original.join(".codesage"), "original", 2);
        index_fixture(&replacement.join(".codesage"), "replacement", 3);
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let server = CodeSageServer::new();
        let state = server
            .resolve_project_inner(alias.to_str().unwrap())
            .unwrap();
        let canonical = state.db_path.parent().unwrap().parent().unwrap();
        assert_eq!(canonical, original);
        let request = fixture_request(&server, canonical, "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        let transitions = Arc::new(AtomicUsize::new(0));
        let observed_transitions = transitions.clone();
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |stage| match stage {
                SnapshotStage::BeforeOpen => {
                    std::fs::remove_file(&alias).unwrap();
                    std::os::unix::fs::symlink(&replacement, &alias).unwrap();
                    assert_eq!(alias.canonicalize().unwrap(), replacement);
                    observed_transitions.fetch_add(1, Ordering::SeqCst);
                }
                SnapshotStage::BeforePin => {
                    assert_eq!(alias.canonicalize().unwrap(), replacement);
                    std::fs::remove_file(&alias).unwrap();
                    std::os::unix::fs::symlink(&original, &alias).unwrap();
                    observed_transitions.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            })),
        };
        let (overview, snapshot) = server
            .with_cached_snapshot(
                &request,
                &cached,
                &hooks,
                |root, db, ranking, recomputed| {
                    assert!(!recomputed);
                    Ok((
                        codesage_graph::build_project_overview_with_top_risk(root, db, ranking)?,
                        codesage_graph::build_session_snapshot_with_top_risk(
                            root, db, "alias", ranking,
                        )?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(transitions.load(Ordering::SeqCst), 2);
        let expected = ["original/1.rs", "original/2.rs"];
        assert_eq!(overview.file_count, 2);
        assert_eq!(overview.symbol_count, 2);
        let mut overview_files: Vec<_> = overview
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        overview_files.sort();
        assert_eq!(overview_files, expected);
        assert_eq!(snapshot.files, expected);
        assert_eq!(snapshot.cycles, vec![expected.to_vec()]);
        let mut snapshot_files: Vec<_> = snapshot
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        snapshot_files.sort();
        assert_eq!(snapshot_files, expected);
    }

    #[tokio::test]
    async fn unobserved_project_ancestor_symlink_swap_cannot_mix_cached_ranking() {
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let replacement = fixture.path().join("replacement");
        let retired = fixture.path().join("retired");
        index_fixture(&project.join(".codesage"), "cached", 2);
        index_fixture(&replacement.join(".codesage"), "opened", 3);
        std::fs::remove_file(replacement.join(".codesage/config.toml")).unwrap();
        std::fs::hard_link(
            project.join(".codesage/config.toml"),
            replacement.join(".codesage/config.toml"),
        )
        .unwrap();
        let server = CodeSageServer::new();
        server
            .resolve_project_inner(project.to_str().unwrap())
            .unwrap();
        let request = fixture_request(&server, &project, "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        let hook_project = project.clone();
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |stage| match stage {
                SnapshotStage::BeforeOpen => {
                    std::fs::rename(&hook_project, &retired).unwrap();
                    std::os::unix::fs::symlink(&replacement, &hook_project).unwrap();
                    assert!(
                        !std::fs::symlink_metadata(hook_project.join(".codesage"))
                            .unwrap()
                            .file_type()
                            .is_symlink()
                    );
                }
                SnapshotStage::BeforePin => {
                    assert_eq!(hook_project.canonicalize().unwrap(), replacement);
                    std::fs::remove_file(&hook_project).unwrap();
                    std::fs::rename(&retired, &hook_project).unwrap();
                }
                _ => {}
            })),
        };
        let (overview, snapshot) = server.with_cached_snapshot(&request, &cached, &hooks, |root, db, ranking, recomputed| {
            assert!(recomputed, "the resolved connection pathname must be compared with the expected index pathname");
            Ok((codesage_graph::build_project_overview_with_top_risk(root, db, ranking)?,
                codesage_graph::build_session_snapshot_with_top_risk(root, db, "ancestor", ranking)?))
        }).unwrap();
        let after = server
            .state
            .overview_cache
            .generation_in_execution(
                &project,
                &project.join(".codesage/index.db"),
                &request.control,
            )
            .unwrap();
        assert!(after == cached.generation);
        let expected = ["opened/1.rs", "opened/2.rs", "opened/3.rs"];
        assert_eq!(overview.file_count, 3);
        assert_eq!(overview.symbol_count, 3);
        let mut overview_files: Vec<_> = overview
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        overview_files.sort();
        assert_eq!(overview_files, expected);
        assert_eq!(snapshot.files, expected);
        assert_eq!(snapshot.cycles, vec![expected.to_vec()]);
        let mut snapshot_files: Vec<_> = snapshot
            .top_risk_files
            .iter()
            .map(|row| row.file.as_str())
            .collect();
        snapshot_files.sort();
        assert_eq!(snapshot_files, expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_session_is_recorded_after_cancelled_response() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "retained", 2);
        let server = CodeSageServer::new();
        let request = fixture_request(&server, root.path(), "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        let snapshot = server
            .with_cached_snapshot(
                &request,
                &cached,
                &SnapshotHooks::default(),
                |root, db, ranking, _| {
                    codesage_graph::build_session_snapshot_with_top_risk(root, db, "late", ranking)
                },
            )
            .unwrap();
        let (committed, wait_committed) = tokio::sync::oneshot::channel();
        let committed = parking_lot::Mutex::new(Some(committed));
        let (release, wait_release) = std::sync::mpsc::channel();
        let wait_release = parking_lot::Mutex::new(wait_release);
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |stage| {
                if stage == SnapshotStage::AfterPersist {
                    committed.lock().take().unwrap().send(()).unwrap();
                    wait_release
                        .lock()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            })),
        };
        let worker_request = request.clone();
        let worker_server = server.clone();
        let task = tokio::spawn(async move {
            worker_server
                .run_controlled(
                    worker_request.clone(),
                    move || {
                        persist_snapshot(
                            &worker_request.project,
                            &snapshot,
                            &worker_request.ticket,
                            &hooks,
                        )
                    },
                    |_| "success",
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), wait_committed)
            .await
            .unwrap()
            .unwrap();
        let persisted: codesage_protocol::SessionSnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(".codesage/sessions/late.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.files, ["retained/1.rs", "retained/2.rs"]);
        request.control.cancel(StopReason::ClientCancelled);
        request.ticket.finish("cancelled");
        let response = status(&stopped_result("cancelled", &request.ticket));
        assert_eq!(response["work_continuing"], true);
        assert_eq!(response["persistence_committed"], false);
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(request.ticket.persistence_committed());
        let diagnostics = server.state.diagnostics.snapshot(256);
        let retained = diagnostics["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == request.ticket.id())
            .unwrap();
        assert_eq!(retained["persistence_committed"], true);
        assert_eq!(retained["outcome"], "cancelled");
        assert_eq!(
            status(&stopped_result("cancelled", &request.ticket))["work_continuing"],
            false
        );
    }

    #[tokio::test]
    async fn cache_bypass_keeps_overview_and_session_work_without_observers() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "uncached", 2);
        let mut state = super::super::state::CodeSageServerState::new();
        state.overview_cache_enabled = false;
        state.diagnostics = super::super::diagnostics::Diagnostics::default();
        let server = CodeSageServer::with_state(Arc::new(state));
        let overview_request = fixture_request(&server, root.path(), "project_overview");
        let overview = server
            .cached_overview(overview_request, root.path().to_string_lossy().into_owned())
            .await;
        assert_ne!(overview.is_error, Some(true), "{overview:?}");
        assert_eq!(
            overview.structured_content.as_ref().unwrap()["file_count"],
            2
        );
        let session_request = fixture_request(&server, root.path(), "session_start");
        let session = server
            .cached_session_start(
                session_request.clone(),
                root.path().to_string_lossy().into_owned(),
                "uncached".into(),
            )
            .await;
        assert_ne!(session.is_error, Some(true), "{session:?}");
        assert!(session_request.ticket.persistence_committed());
        let snapshot: codesage_protocol::SessionSnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(".codesage/sessions/uncached.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.files, ["uncached/1.rs", "uncached/2.rs"]);
        assert_eq!(snapshot.top_risk_files.len(), 2);
        let diagnostics = server.state.diagnostics.snapshot(256);
        assert_eq!(diagnostics["counters"]["executions_started"], 2);
        assert_eq!(diagnostics["gauges"]["cache"], json!({}));
    }

    fn single_slot_server() -> CodeSageServer {
        let mut state = super::super::state::CodeSageServerState::new();
        let mut limits = super::super::work::WorkLimits::default();
        limits.interactive.running = 1;
        limits.analysis.running = 1;
        state.work = super::super::work::WorkCoordinator::new(limits).unwrap();
        state.overview_cache_enabled = true;
        state.diagnostics = super::super::diagnostics::Diagnostics::default();
        CodeSageServer::with_state(Arc::new(state))
    }

    #[tokio::test]
    async fn stable_overview_hit_finishes_while_same_project_analysis_is_occupied() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "warm", 2);
        let server = single_slot_server();
        server
            .resolve_project_inner(root.path().to_str().unwrap())
            .unwrap();
        let request = fixture_request(&server, root.path(), "project_overview");
        server.ranking_for(&request).await.unwrap();
        let analysis = server
            .state
            .work
            .acquire_execution(root.path(), WorkClass::Analysis, &WorkControl::new(None))
            .await
            .unwrap();
        let before = server.state.diagnostics.snapshot(256)["counters"]["executions_started"]
            .as_u64()
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.cached_overview(request, root.path().to_string_lossy().into_owned()),
        )
        .await
        .expect("a warm overview must not wait for the occupied analysis slot");
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(result.structured_content.as_ref().unwrap()["file_count"], 2);
        assert_eq!(server.state.work.snapshot().running, [0, 1, 0]);
        let diagnostics = server.state.diagnostics.snapshot(256);
        assert_eq!(
            diagnostics["counters"]["executions_started"]
                .as_u64()
                .unwrap()
                - before,
            2
        );
        assert_eq!(
            diagnostics["recent_executions"][0]["work_class"],
            "interactive"
        );
        assert_eq!(
            diagnostics["recent_executions"][1]["work_class"],
            "interactive"
        );
        drop(analysis);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalidated_overview_releases_interactive_snapshot_before_analysis() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "old", 2);
        let server = single_slot_server();
        server
            .resolve_project_inner(root.path().to_str().unwrap())
            .unwrap();
        let request = fixture_request(&server, root.path(), "project_overview");
        let cached = server.ranking_for(&request).await.unwrap();
        let analysis = server
            .state
            .work
            .acquire_execution(root.path(), WorkClass::Analysis, &WorkControl::new(None))
            .await
            .unwrap();
        let path = root.path().join(".codesage/index.db");
        let mutation_path = path.clone();
        let hooks = SnapshotHooks {
            boundary: Some(Box::new(move |stage| {
                if stage == SnapshotStage::BeforePin {
                    rusqlite::Connection::open(&mutation_path).unwrap().execute("INSERT INTO files(path,language,content_hash) VALUES('new.rs','rust','new')", []).unwrap();
                }
            })),
        };
        let before = server.state.diagnostics.snapshot(256)["counters"]["executions_started"]
            .as_u64()
            .unwrap();
        let worker = server.clone();
        let project = root.path().to_string_lossy().into_owned();
        let task = tokio::spawn(async move {
            worker
                .overview_from_ranking(request, project, cached, hooks)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(
                    !task.is_finished(),
                    "invalidated overview must defer instead of scoring in the interactive lane"
                );
                if server.state.work.snapshot().queued[1] == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(server.state.work.snapshot().running, [0, 1, 0]);
        let interactive = tokio::time::timeout(
            Duration::from_secs(5),
            server.state.work.acquire_execution(
                root.path(),
                WorkClass::Interactive,
                &WorkControl::new(None),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        drop(interactive);
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer
            .execute(
                "UPDATE files SET content_hash='after-deferral' WHERE path='new.rs'",
                [],
            )
            .expect("the deferred interactive snapshot must no longer hold a read transaction");
        let checkpoint: (i64, i64, i64) = writer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            checkpoint,
            (0, 0, 0),
            "the deferred interactive snapshot must not pin WAL history"
        );
        drop(writer);
        drop(analysis);
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value = result.structured_content.as_ref().unwrap();
        assert_eq!(value["file_count"], 3);
        assert_eq!(value["_meta"]["ranking_recomputed"], true);
        let diagnostics = server.state.diagnostics.snapshot(256);
        assert_eq!(
            diagnostics["counters"]["executions_started"]
                .as_u64()
                .unwrap()
                - before,
            2
        );
        assert_eq!(
            diagnostics["recent_executions"][0]["work_class"],
            "analysis"
        );
        assert_eq!(
            diagnostics["recent_executions"][1]["work_class"],
            "interactive"
        );
        assert_eq!(diagnostics["recent_executions"][1]["outcome"], "success");
    }

    #[tokio::test]
    async fn session_recomputation_disclosure_matches_persisted_snapshot_and_text() {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "session", 2);
        let server = single_slot_server();
        let request = fixture_request(&server, root.path(), "session_start");
        let cached = server.ranking_for(&request).await.unwrap();
        rusqlite::Connection::open(root.path().join(".codesage/index.db"))
            .unwrap()
            .execute(
                "INSERT INTO files(path,language,content_hash) VALUES('later.rs','rust','new')",
                [],
            )
            .unwrap();
        let result = server
            .session_from_ranking(
                request,
                root.path().to_string_lossy().into_owned(),
                "recomputed".into(),
                cached,
                SnapshotHooks::default(),
            )
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value = result.structured_content.as_ref().unwrap();
        assert_eq!(value["file_count"], 3);
        assert_eq!(value["_meta"]["ranking_recomputed"], true);
        let text_value = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text.text).ok())
            .find(|value| value.get("session_id").is_some())
            .unwrap();
        assert_eq!(&text_value, value);
        let persisted: codesage_protocol::SessionSnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(".codesage/sessions/recomputed.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.file_count, 3);
        assert_eq!(persisted.top_risk_files.len(), 3);
        assert_eq!(
            server.state.diagnostics.snapshot(1)["recent_executions"][0]["work_class"],
            "analysis"
        );
    }

    async fn unavailable_consumer_config(after_first_probe: bool) {
        let root = tempfile::tempdir().unwrap();
        index_fixture(&root.path().join(".codesage"), "fallback", 2);
        let server = CodeSageServer::new();
        let request = fixture_request(&server, root.path(), "project_overview");
        let cached = server.ranking_for(&request).await.unwrap();
        let config = root.path().join(".codesage/config.toml");
        let invalidate = move || {
            std::fs::remove_file(&config).unwrap();
            std::fs::create_dir(&config).unwrap();
        };
        let hooks = if after_first_probe {
            SnapshotHooks {
                boundary: Some(Box::new(move |stage| {
                    if stage == SnapshotStage::BeforeOpen {
                        invalidate();
                    }
                })),
            }
        } else {
            invalidate();
            SnapshotHooks::default()
        };
        let result = server
            .with_cached_snapshot(&request, &cached, &hooks, |root, db, ranking, changed| {
                assert!(changed);
                codesage_graph::build_project_overview_with_top_risk(root, db, ranking)
            })
            .unwrap();
        assert_eq!(result.file_count, 2);
        assert_eq!(result.top_risk_files.len(), 2);
        assert_eq!(result.top_risk_files[0].file, "fallback/1.rs");
        request.control.cancel(StopReason::ClientCancelled);
        let error = server
            .snapshot_generation(
                root.path(),
                &root.path().join(".codesage/index.db"),
                &request.control,
            )
            .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<codesage_protocol::work::WorkStopped>())
        );
    }

    #[tokio::test]
    async fn unavailable_config_before_consumer_probe_recomputes_on_snapshot() {
        unavailable_consumer_config(false).await;
    }

    #[tokio::test]
    async fn unavailable_config_after_consumer_probe_recomputes_on_snapshot() {
        unavailable_consumer_config(true).await;
    }

    fn status(result: &CallToolResult) -> serde_json::Value {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text.text).ok())
            .find(|value| value.get("status").is_some())
            .expect("error status")
    }

    #[tokio::test]
    async fn shutdown_refusal_is_not_reported_as_saturation() {
        let server = CodeSageServer::new();
        server.state.work.shutdown();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(client_io).await.unwrap();
        let response = client
            .call_tool(
                CallToolRequestParams::new("project_overview").with_arguments(
                    json!({"project":"/nonexistent"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        client.cancel().await.unwrap();
        task.await.unwrap();
        assert_eq!(status(&response)["status"], "shutdown");
    }

    #[tokio::test]
    async fn preflight_preserves_nested_error_outcome() {
        let server = CodeSageServer::new();
        let ticket = Arc::new(server.state.diagnostics.begin_request("find_symbol", None));
        let request = Arc::new(ToolRequest {
            control: WorkControl::new(None),
            ticket,
            project: PathBuf::from("/fixture"),
            tool: "find_symbol".into(),
            class: WorkClass::Interactive,
        });
        let error = server
            .run_controlled::<(), _>(
                request,
                || {
                    Err(anyhow::Error::new(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                        None,
                    )))
                },
                |_| "success",
            )
            .await
            .unwrap_err();
        let result = super::super::render::render_with_kind::<()>(Err(error), "");
        assert_eq!(status(&result)["status"], "database-busy");
        let snapshot = server.state.diagnostics.snapshot(10);
        assert_eq!(
            snapshot["counters"]["execution_outcomes"]["database-busy"],
            1
        );
    }

    #[test]
    fn cancellation_response_keeps_persistence_and_physical_state() {
        let diagnostics = super::super::diagnostics::Diagnostics::default();
        let request = diagnostics.begin_request("session_start", None);
        let execution = Arc::new(diagnostics.begin_execution("session_start", None));
        execution.set_phase("running");
        request.link_execution_ticket(&execution);
        request.finish("cancelled");
        request.mark_persisted();
        let metadata = status(&stopped_result("cancelled", &request));
        assert_eq!(metadata["request_id"], request.id());
        assert_eq!(metadata["persistence_committed"], true);
        assert_eq!(metadata["phase"], "running");
        assert_eq!(metadata["work_continuing"], true);
        execution.finish("cancelled");
        assert_eq!(
            status(&stopped_result("cancelled", &request))["work_continuing"],
            false
        );
    }

    #[test]
    fn client_cancellation_preserves_prior_shutdown_in_response_and_diagnostics() {
        let diagnostics = super::super::diagnostics::Diagnostics::default();
        let request = diagnostics.begin_request("project_overview", None);
        let control = WorkControl::new(None);
        control.cancel(StopReason::Shutdown);
        let result = client_cancelled_result(&control, &request);
        assert_eq!(status(&result)["status"], "shutdown");
        let snapshot = diagnostics.snapshot(1);
        assert_eq!(snapshot["recent_requests"][0]["outcome"], "shutdown");
        assert_eq!(snapshot["counters"]["request_outcomes"]["shutdown"], 1);
        assert!(
            snapshot["counters"]["request_outcomes"]
                .get("cancelled")
                .is_none()
        );
    }

    #[test]
    fn ranking_recomputation_disclosure_preserves_text_structured_parity() {
        let mut result = CallToolResult::structured(json!({"top_risk_files": [], "next":null,
            "_meta":{"truncated":true}}));
        result
            .content
            .insert(0, ContentBlock::text("Existing freshness warning"));
        disclose_ranking_recomputation(&mut result);
        let value = result.structured_content.as_ref().unwrap();
        assert_eq!(value["_meta"]["ranking_recomputed"], true);
        assert!(
            value["_meta"]
                .get("index_changed_during_analysis")
                .is_none()
        );
        assert_eq!(value["_meta"]["truncated"], true);
        let text_value: serde_json::Value =
            serde_json::from_str(&result.content[1].as_text().unwrap().text).unwrap();
        assert_eq!(&text_value, value);
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "Existing freshness warning"
        );
    }
}
