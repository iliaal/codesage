//! One response envelope shared by every successful `tools/call` result.
//!
//! The envelope keys (`tool`, `index`, `completeness`, `cost`, `target`) are
//! added alongside the unchanged legacy payload. Silence means the good case:
//! `completeness` is absent when the answer is exact, `index.structural` and
//! `index.semantic` are absent when fresh, `index.dirty_paths` is absent when
//! nothing in the response changed on disk, and `target` is absent unless the
//! tool resolved its input to several entities.
//!
//! `completeness` is derived here from the legacy incompleteness fields the
//! tools already emit, so one vocabulary covers all of them. The legacy fields
//! keep being emitted this release; the next minor removes them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codesage_protocol::Handle;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};

use super::CodeSageServer;

/// Tools whose payload is machine-internal — raw vectors, reranker scores,
/// daemon counters — and whose only consumers are CodeSage's own CLI paths.
/// `daemon_stats` also returns before the envelope step in `dispatch_tool`.
const UNENVELOPED_TOOLS: &[&str] = &["embed_texts", "rerank_pairs", "daemon_stats"];

/// Recompute the per-project index facts at most this often. The generation
/// alone cannot expire them: moving HEAD does not touch `index.db`, so a
/// commit without a reindex would otherwise keep reporting `fresh`.
const INDEX_FACTS_TTL: Duration = Duration::from_secs(30);

/// Bound the facts cache; the daemon serves many projects over its lifetime.
const INDEX_FACTS_MAX_PROJECTS: usize = 64;

/// Cap a `recover` payload that echoes caller arguments.
const MAX_RECOVER_BYTES: usize = 1024;

/// Keep per-input reason lists from re-inflating a response the budget trimmed.
const MAX_RECOVER_LIST: usize = 20;

/// `CODESAGE_ENVELOPE=legacy` suppresses every envelope key. Read once, when
/// the daemon builds its server state, like `RUST_LOG`.
pub(crate) fn enabled_from(value: Option<&str>) -> bool {
    value != Some("legacy")
}

/// One incompleteness class. Ordered most to least severe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Truncated,
    Bounded,
    Partial,
    Clamped,
    Unscored,
    Floor,
}

impl Kind {
    /// Severity order, most severe first: a truncated page dropped rows that
    /// exist, a bounded walk stopped early, a partial answer skipped inputs, a
    /// clamped parameter still answered the question asked, an unscored file
    /// lacks one term, and a floor is exact about everything it found.
    const SEVERITY: [Kind; 6] = [
        Kind::Truncated,
        Kind::Bounded,
        Kind::Partial,
        Kind::Clamped,
        Kind::Unscored,
        Kind::Floor,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Kind::Truncated => "truncated",
            Kind::Bounded => "bounded",
            Kind::Partial => "partial",
            Kind::Clamped => "clamped",
            Kind::Unscored => "unscored",
            Kind::Floor => "floor",
        }
    }
}

/// Everything the payload scan found, kept as evidence for `recover`.
#[derive(Default)]
pub(super) struct Signals {
    kinds: Vec<Kind>,
    total_results: Option<u64>,
    returned: Option<u64>,
    clamps: Vec<Value>,
    reason_lists: Vec<(&'static str, Vec<Value>)>,
    coverage: bool,
    floor_reason: Option<&'static str>,
    stale_paths: Vec<String>,
    ambiguous: bool,
    candidates_total: Option<u64>,
}

impl Signals {
    fn mark(&mut self, kind: Kind) {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
    }

    /// Most severe applicable kind; `None` when the answer is exact.
    fn severest(&self) -> Option<Kind> {
        Kind::SEVERITY
            .iter()
            .copied()
            .find(|kind| self.kinds.contains(kind))
    }
}

/// Bound the walk; response payloads are shallow, a cycle cannot occur in JSON.
const MAX_SCAN_DEPTH: usize = 16;

fn non_empty_array(value: &Value) -> Option<&Vec<Value>> {
    value.as_array().filter(|items| !items.is_empty())
}

fn is_true(value: &Value) -> bool {
    value.as_bool() == Some(true)
}

/// Collect every incompleteness signal the payload already carries.
/// Keys are matched at any depth: `unscored` is top-level on `assess_risk` and
/// per row on `assess_risk_batch`, and `_meta` nests `truncated` and `clamps`.
pub(super) fn scan(payload: &Map<String, Value>, out: &mut Signals) {
    scan_object_at(payload, 0, out);
}

fn scan_at(value: &Value, depth: usize, out: &mut Signals) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => scan_object_at(map, depth, out),
        Value::Array(items) => {
            for item in items {
                scan_at(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn scan_object_at(map: &Map<String, Value>, depth: usize, out: &mut Signals) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    for (key, child) in map {
        match key.as_str() {
            "counts_floor" if is_true(child) => {
                out.mark(Kind::Floor);
                out.floor_reason.get_or_insert("name_based_edges");
            }
            "unmodelled" if child.as_u64().is_some_and(|n| n > 0) => {
                out.mark(Kind::Floor);
                out.floor_reason.get_or_insert("unmodelled_test_edges");
            }
            "bounded" | "reach_walk_capped" | "frontier_capped" | "callers_truncated"
            | "capped"
                if is_true(child) =>
            {
                out.mark(Kind::Bounded);
            }
            "truncated" | "reachable_capped" if is_true(child) => {
                out.mark(Kind::Truncated);
            }
            "total_results" => out.total_results = out.total_results.or(child.as_u64()),
            "returned" => out.returned = out.returned.or(child.as_u64()),
            "clamps" => {
                if let Some(items) = non_empty_array(child) {
                    out.mark(Kind::Clamped);
                    out.clamps = items.clone();
                }
            }
            "unscored" if is_true(child) => out.mark(Kind::Unscored),
            "unscored_files" if non_empty_array(child).is_some() => {
                out.mark(Kind::Unscored);
            }
            "unwalked_files" | "partial_files" | "unindexed_files" | "no_symbol_files" => {
                if let Some(items) = non_empty_array(child) {
                    out.mark(Kind::Partial);
                    let name = match key.as_str() {
                        "unwalked_files" => "unwalked_files",
                        "partial_files" => "partial_files",
                        "unindexed_files" => "unindexed_files",
                        _ => "no_symbol_files",
                    };
                    out.reason_lists
                        .push((name, items.iter().take(MAX_RECOVER_LIST).cloned().collect()));
                }
            }
            "coverage" if coverage_gap(child) => {
                out.mark(Kind::Partial);
                out.coverage = true;
            }
            "stale_files" => {
                if let Some(items) = non_empty_array(child) {
                    out.stale_paths
                        .extend(items.iter().filter_map(|v| v.as_str()).map(str::to_owned));
                }
            }
            "ambiguous" if is_true(child) => out.ambiguous = true,
            "definition_count" | "candidates_total" => {
                out.candidates_total = out.candidates_total.or(child.as_u64());
            }
            _ => {}
        }
        scan_at(child, depth + 1, out);
    }
}

/// `_meta.coverage` rides along on every empty `search` / `find_symbol` /
/// `find_references` / `find_similar` page as an explanatory note, so its mere
/// presence is not an incompleteness. Only a measured gap is: nothing indexed
/// at all, or an active-model semantic set that does not cover the indexed
/// files.
fn coverage_gap(coverage: &Value) -> bool {
    let Some(indexed) = coverage
        .as_object()
        .and_then(|map| map.get("indexed_files"))
        .and_then(Value::as_u64)
    else {
        return false;
    };
    if indexed == 0 {
        return true;
    }
    coverage
        .get("semantically_indexed_files")
        .and_then(Value::as_u64)
        .is_some_and(|semantic| semantic < indexed)
}

/// Broader CLI equivalent of an MCP tool. MCP caps are tighter than the
/// operator surface, so the CLI is the documented remedy when a walk or a page
/// stopped at an MCP bound. No argument is interpolated into the string.
fn cli_command(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "project_overview" => "codesage overview",
        "search" => "codesage search",
        "find_symbol" => "codesage find-symbol",
        "find_references" => "codesage find-references",
        "find_similar" => "codesage similar",
        "list_dependencies" => "codesage dependencies",
        "impact_analysis" => "codesage impact",
        "trace_call_path" => "codesage trace",
        "from_trace" => "codesage from-trace",
        "export_context" => "codesage export",
        "find_coupling" => "codesage coupling",
        "assess_risk" => "codesage risk",
        "assess_risk_batch" => "codesage risk-batch",
        "assess_risk_diff" => "codesage risk-diff",
        "recommend_tests" => "codesage tests-for",
        "review_rehearsal" => "codesage rehearse",
        "session_start" => "codesage session-start",
        "session_end" => "codesage session-end",
        "list_features" => "codesage features-list",
        "find_feature" => "codesage feature-for",
        "feature_bundle" => "codesage feature-bundle",
        _ => return None,
    })
}

/// Tools that accept `offset`, the only ones for which advancing the page is
/// a satisfiable retry. Mirrors `render::OFFSET_PAGED_KINDS`.
fn offset_paged(tool: &str) -> bool {
    super::render::OFFSET_PAGED_KINDS.contains(&tool)
}

fn fits(value: &Value) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= MAX_RECOVER_BYTES)
}

/// What the caller can do about the selected kind. Never prose.
fn recover(
    kind: Kind,
    tool: &str,
    arguments: &Map<String, Value>,
    signals: &Signals,
) -> Option<Value> {
    match kind {
        Kind::Truncated => {
            let mut out = Map::new();
            if let Some(total) = signals.total_results {
                out.insert("total".into(), json!(total));
            }
            if let Some(returned) = signals.returned {
                out.insert("returned".into(), json!(returned));
            }
            if offset_paged(tool)
                && let Some(returned) = signals.returned
                && returned > 0
            {
                let offset = arguments
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(returned);
                let mut retry = arguments.clone();
                retry.insert("offset".into(), json!(offset));
                let retry = json!({"tool": tool, "arguments": retry});
                if fits(&retry) {
                    out.insert("arguments".into(), retry["arguments"].clone());
                }
            }
            (!out.is_empty()).then_some(Value::Object(out))
        }
        Kind::Bounded => match cli_command(tool) {
            Some(command) => Some(json!({"command": command})),
            None => {
                let retry = json!({"tool": tool, "arguments": arguments});
                fits(&retry).then_some(retry)
            }
        },
        Kind::Clamped => {
            let mut requested = Map::new();
            let mut applied = Map::new();
            for clamp in &signals.clamps {
                let Some(param) = clamp.get("param").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(value) = clamp.get("requested") {
                    requested.insert(param.to_owned(), value.clone());
                }
                if let Some(value) = clamp.get("applied") {
                    applied.insert(param.to_owned(), value.clone());
                }
            }
            (!requested.is_empty() || !applied.is_empty())
                .then(|| json!({"requested": requested, "applied": applied}))
        }
        Kind::Unscored => Some(json!({"command": "codesage git-index"})),
        Kind::Partial => {
            let mut out = Map::new();
            for (name, items) in &signals.reason_lists {
                out.insert((*name).to_owned(), Value::Array(items.clone()));
            }
            if signals.coverage {
                out.insert("command".into(), json!("codesage index"));
            }
            (!out.is_empty()).then_some(Value::Object(out))
        }
        Kind::Floor => signals.floor_reason.map(|reason| json!({"reason": reason})),
    }
}

fn completeness(tool: &str, arguments: &Map<String, Value>, signals: &Signals) -> Option<Value> {
    let kind = signals.severest()?;
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind.as_str()));
    let kinds: Vec<Value> = Kind::SEVERITY
        .iter()
        .filter(|k| signals.kinds.contains(k))
        .map(|k| json!(k.as_str()))
        .collect();
    out.insert("kinds".into(), Value::Array(kinds));
    if let Some(recover) = recover(kind, tool, arguments, signals) {
        out.insert("recover".into(), recover);
    }
    Some(Value::Object(out))
}

/// Per-project index state, recomputed at most once per [`INDEX_FACTS_TTL`].
#[derive(Clone)]
pub(super) struct IndexFacts {
    generation: Option<u64>,
    /// Short indexed SHA; `None` when the index carries no stamp.
    head: Option<String>,
    /// The indexed SHA is not HEAD (behind, or an unrelated ancestor).
    behind: bool,
    /// `Some("partial" | "none")`; `None` means every indexed file has chunks.
    semantic: Option<&'static str>,
    computed_at: Instant,
}

/// Cache key is the canonical project root.
#[derive(Default)]
pub(crate) struct IndexFactsCache {
    entries: parking_lot::Mutex<HashMap<PathBuf, IndexFacts>>,
}

impl IndexFactsCache {
    fn get(&self, root: &Path, generation: Option<u64>) -> Option<IndexFacts> {
        let entries = self.entries.lock();
        let facts = entries.get(root)?;
        (facts.generation == generation && facts.computed_at.elapsed() < INDEX_FACTS_TTL)
            .then(|| facts.clone())
    }

    fn put(&self, root: &Path, facts: IndexFacts) {
        let mut entries = self.entries.lock();
        if entries.len() >= INDEX_FACTS_MAX_PROJECTS && !entries.contains_key(root) {
            let victim = entries
                .iter()
                .min_by_key(|(_, facts)| facts.computed_at)
                .map(|(path, _)| path.clone());
            if let Some(victim) = victim {
                entries.remove(&victim);
            }
        }
        entries.insert(root.to_owned(), facts);
    }
}

/// 12 hex characters, matching `codesage_graph::drift`'s display form.
/// A malformed stamp is left verbatim rather than silently reshaped.
fn short_sha(sha: &str) -> String {
    if sha.len() > 12 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        sha[..12].to_string()
    } else {
        sha.to_string()
    }
}

/// Files carrying chunks against the indexed file set, for the paths where
/// per-file semantic freshness is unavailable.
fn coverage_of(semantic: usize, files: usize) -> Option<&'static str> {
    if semantic == 0 {
        Some("none")
    } else if semantic < files {
        Some("partial")
    } else {
        None
    }
}

impl CodeSageServer {
    /// Measure drift and semantic coverage once per generation (or per TTL),
    /// so the per-call envelope costs a stat and a `PRAGMA data_version`.
    fn index_facts(&self, root: &Path, db_path: &Path, generation: Option<u64>) -> IndexFacts {
        if let Some(cached) = self.state.index_facts.get(root, generation) {
            return cached;
        }
        let mut facts = IndexFacts {
            generation,
            head: None,
            behind: false,
            semantic: None,
            computed_at: Instant::now(),
        };
        match codesage_storage::Database::open_existing(db_path) {
            Ok(db) => {
                let drift = codesage_graph::drift::check_drift(root, &db);
                facts.head = drift.stored_sha.as_deref().map(short_sha);
                facts.behind = matches!(
                    drift.kind,
                    codesage_graph::drift::DriftKind::BehindHead
                        | codesage_graph::drift::DriftKind::UnrelatedAncestor
                );
                facts.semantic = self.semantic_state(root, db_path, &db);
            }
            Err(error) => {
                tracing::debug!(error = %error, "envelope index facts skipped");
            }
        }
        self.state.index_facts.put(root, facts.clone());
        facts
    }

    /// Semantic coverage of the model this project searches with. A model
    /// switch leaves the previous model's rows in place, so a count across
    /// every chunk table would report coverage that `search` cannot use, and a
    /// file reindexed structurally but not semantically would stay invisible.
    fn semantic_state(
        &self,
        root: &Path,
        db_path: &Path,
        db: &codesage_storage::Database,
    ) -> Option<&'static str> {
        let files = db.file_count().unwrap_or(0);
        if files == 0 {
            return None;
        }
        let Some(model) = self.configured_model(root) else {
            // Nothing to scope by: every chunk table is equally plausible.
            return coverage_of(db.semantic_file_count().unwrap_or(0), files);
        };
        match codesage_storage::Database::open_for_existing_model(db_path, &model)
            .and_then(|scoped| scoped.semantic_freshness())
        {
            Ok(None) => Some("none"),
            Ok(Some(freshness)) if freshness.indexed_files == 0 => Some("none"),
            Ok(Some(freshness)) if !freshness.is_fresh() => Some("partial"),
            Ok(Some(_)) => None,
            Err(error) => {
                // Ambiguous or pre-registry chunk tables: the fallback count is
                // coarser but stays scoped to the configured model.
                tracing::debug!(error = %error, "envelope semantic freshness fell back to a count");
                coverage_of(db.semantic_file_count_for_model(&model).unwrap_or(0), files)
            }
        }
    }

    /// The project's configured embedding model, without the watcher side
    /// effect `resolve_project` carries: `edit_check` is enveloped too.
    fn configured_model(&self, root: &Path) -> Option<String> {
        self.resolve_project_inner(&root.to_string_lossy())
            .ok()
            .map(|state| state.embedding_config.model)
            .filter(|model| !model.is_empty())
    }

    /// Index generation. Equal values mean two responses describe the same
    /// index state, and are only meaningful within one daemon's lifetime.
    fn index_generation(&self, root: &Path, db_path: &Path) -> Option<u64> {
        let control = codesage_protocol::work::WorkControl::new(None);
        // The probe stats the index and reads `PRAGMA data_version` on the
        // cache's own read-only connection; it holds no execution lease.
        self.state
            .overview_cache
            .generation_in_execution(root, db_path, &control)
            .ok()
            .map(|generation| generation.id())
    }

    fn index_block(&self, project: Option<&str>, stale: &[String]) -> Option<Value> {
        let root = crate::evidence_root(Path::new(project?)).ok()?;
        let db_path = crate::db_path(&root);
        if !db_path.is_file() {
            return None;
        }
        let generation = self.index_generation(&root, &db_path);
        let facts = self.index_facts(&root, &db_path, generation);
        let mut out = Map::new();
        if let Some(generation) = generation {
            out.insert("generation".into(), json!(generation));
        }
        if let Some(head) = &facts.head {
            out.insert("head".into(), json!(head));
        }
        // `dirty` is the stronger claim about *this* response, so it wins over
        // a repository-wide `behind`.
        let structural = if !stale.is_empty() {
            Some("dirty")
        } else if facts.behind {
            Some("behind")
        } else {
            None
        };
        if let Some(structural) = structural {
            out.insert("structural".into(), json!(structural));
        }
        if let Some(semantic) = facts.semantic {
            out.insert("semantic".into(), json!(semantic));
        }
        if !stale.is_empty() {
            out.insert("dirty_paths".into(), json!(stale));
        }
        (!out.is_empty()).then_some(Value::Object(out))
    }

    /// Add the envelope keys to a successful result. Best effort: a failure to
    /// read the index leaves the payload intact rather than failing the call.
    pub(super) fn annotate_envelope(
        &self,
        mut result: CallToolResult,
        tool: &str,
        arguments: &Map<String, Value>,
        elapsed: Duration,
    ) -> CallToolResult {
        if !self.state.envelope_enabled
            || result.is_error == Some(true)
            || UNENVELOPED_TOOLS.contains(&tool)
        {
            return result;
        }
        let Some(Value::Object(payload)) = result.structured_content.as_ref() else {
            return result;
        };
        let bytes = serde_json::to_vec(payload).map(|b| b.len()).unwrap_or(0);
        let mut signals = Signals::default();
        scan(payload, &mut signals);

        let project = arguments.get("project").and_then(Value::as_str);
        let index = self.index_block(project, &signals.stale_paths);
        let completeness = completeness(tool, arguments, &signals);
        let target = signals.ambiguous.then(|| {
            let mut out = Map::new();
            out.insert("ambiguous".into(), json!(true));
            if let Some(total) = signals.candidates_total {
                out.insert("candidates_total".into(), json!(total));
            }
            Value::Object(out)
        });

        let Some(Value::Object(payload)) = result.structured_content.as_mut() else {
            return result;
        };
        insert_absent(payload, "tool", json!(tool));
        if let Some(index) = index {
            insert_absent(payload, "index", index);
        }
        if let Some(target) = target {
            insert_absent(payload, "target", target);
        }
        if let Some(completeness) = completeness {
            insert_absent(payload, "completeness", completeness);
        }
        insert_absent(
            payload,
            "cost",
            json!({"ms": elapsed.as_millis() as u64, "bytes": bytes}),
        );
        super::render::rerender_json_text(&mut result);
        result
    }
}

/// A tool that already owns one of these names keeps its own value; the
/// envelope never overwrites payload evidence.
fn insert_absent(payload: &mut Map<String, Value>, key: &str, value: Value) {
    if payload.contains_key(key) {
        tracing::debug!(key, "envelope key already present in payload");
        return;
    }
    payload.insert(key.to_owned(), value);
}

/// Schema fragment advertised on every tool's `outputSchema`.
pub(super) fn schema_properties() -> Vec<(&'static str, Value)> {
    vec![
        (
            "tool",
            json!({
                "type": "string",
                "description": "Name of the tool that produced this response."
            }),
        ),
        (
            "index",
            json!({
                "type": "object",
                "description": "Which index state this answer describes. `structural`, `semantic`, and `dirty_paths` are omitted when everything is fresh. Successor of `_meta.stale_files` / `_meta.stale_warning`.",
                "properties": {
                    "generation": {"type": "integer", "minimum": 0, "description": "48-bit digest of the index state: file identity, observer epoch, `data_version`, config digest, and coupling policy. Unequal values prove the state changed; equal values are only meaningful within one daemon's lifetime, since a restarted daemon can reproduce an earlier value."},
                    "head": {"type": "string", "description": "Short SHA the structural index was built against."},
                    "structural": {"enum": ["behind", "dirty"], "description": "`behind`: the indexed SHA is not HEAD. `dirty`: a path in this response differs on disk from the index. Absent when fresh."},
                    "semantic": {"enum": ["partial", "none"], "description": "Semantic coverage of the indexed file set by the configured embedding model: none when that model has no chunks, partial when some indexed files lack chunks for it or their chunks predate the current content. Absent when the configured model covers every indexed file."},
                    "dirty_paths": {"type": "array", "items": {"type": "string"}, "description": "Paths in this response that changed on disk since indexing."}
                }
            }),
        ),
        (
            "target",
            json!({
                "type": "object",
                "description": "Present only when the input resolved to several entities.",
                "properties": {
                    "ambiguous": {"type": "boolean"},
                    "candidates_total": {"type": "integer", "minimum": 0}
                }
            }),
        ),
        (
            "completeness",
            json!({
                "type": "object",
                "description": "Absent when the answer is exact. One vocabulary replacing `counts_floor`, `bounded`, `truncated`, `_meta.truncated`, `callers_truncated`, `reachable_capped`, `reach_walk_capped`, `unmodelled`, `unscored`, `_meta.clamps`, `_meta.coverage`, and the per-input reason lists, all of which are deprecated and removed in the next minor. `kind` is the most severe of `kinds` in the order truncated > bounded > partial > clamped > unscored > floor.",
                "properties": {
                    "kind": {"enum": ["truncated", "bounded", "partial", "clamped", "unscored", "floor"]},
                    "kinds": {"type": "array", "items": {"enum": ["truncated", "bounded", "partial", "clamped", "unscored", "floor"]}},
                    "recover": {"type": "object", "description": "What removes the limitation: `{total, returned, arguments}` for truncated, `{tool, arguments}` or `{command}` for bounded, `{requested, applied}` for clamped, `{command}` for unscored, the per-input reason lists for partial, `{reason}` for floor. Never prose."}
                }
            }),
        ),
        (
            "cost",
            json!({
                "type": "object",
                "description": "What this call spent: wall time from request receipt to response, including admission wait, and the serialized payload size before the envelope was added.",
                "properties": {
                    "ms": {"type": "integer", "minimum": 0},
                    "bytes": {"type": "integer", "minimum": 0}
                }
            }),
        ),
    ]
}

/// Repository-relative paths named by any handle anywhere in the payload.
/// Handle-carrying rows cannot drift out of staleness checking the way a
/// hand-maintained key allowlist can.
pub(super) fn collect_handle_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if let Some(path) =
                Handle::parse(text).and_then(|handle| handle.path().map(str::to_owned))
            {
                out.push(path);
            }
        }
        Value::Object(map) => {
            for child in map.values() {
                collect_handle_paths(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_handle_paths(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codesage_protocol::{FileInfo, Language};
    use codesage_storage::Database;
    use tempfile::TempDir;

    use super::*;
    use crate::mcp::{CodeSageServerState, render::render_with_kind};

    /// Envelope keys, in the order [`CodeSageServer::annotate_envelope`]
    /// inserts them.
    const ENVELOPE_KEYS: &[&str] = &["tool", "index", "target", "completeness", "cost"];

    fn signals_of(payload: Value) -> Signals {
        let mut signals = Signals::default();
        scan(payload.as_object().expect("object payload"), &mut signals);
        signals
    }

    fn envelope_of(payload: Value, tool: &str, arguments: Value) -> Value {
        let arguments = arguments.as_object().cloned().unwrap_or_default();
        let mut signals = Signals::default();
        scan(payload.as_object().expect("object payload"), &mut signals);
        let mut out = Map::new();
        if let Some(completeness) = completeness(tool, &arguments, &signals) {
            out.insert("completeness".into(), completeness);
        }
        Value::Object(out)
    }

    #[test]
    fn legacy_mode_is_the_only_recognized_opt_out() {
        assert!(!enabled_from(Some("legacy")));
        assert!(enabled_from(None));
        assert!(enabled_from(Some("")));
        assert!(enabled_from(Some("1")));
    }

    #[test]
    fn an_exact_payload_derives_no_completeness_block() {
        let signals = signals_of(json!({"results": [{"file_path": "src/a.rs"}]}));
        assert!(signals.severest().is_none());
        assert_eq!(
            envelope_of(json!({"results": []}), "search", json!({})),
            json!({})
        );
    }

    #[test]
    fn each_legacy_field_maps_onto_its_completeness_kind() {
        let cases: Vec<(Value, &str)> = vec![
            (json!({"counts_floor": true}), "floor"),
            (json!({"unmodelled": 45}), "floor"),
            (json!({"bounded": true}), "bounded"),
            (json!({"reach_walk_capped": true}), "bounded"),
            (json!({"callers_truncated": true}), "bounded"),
            (
                json!({"to_resolution": {"resolved_pairs": 256, "capped": true}}),
                "bounded",
            ),
            (json!({"truncated": true}), "truncated"),
            (json!({"_meta": {"truncated": true}}), "truncated"),
            (json!({"reachable_capped": true}), "truncated"),
            (
                json!({"_meta": {"clamps": [{"param": "limit", "requested": 500, "applied": 100}]}}),
                "clamped",
            ),
            (json!({"unscored": true}), "unscored"),
            (json!({"unscored_files": ["src/new.rs"]}), "unscored"),
            (json!({"unwalked_files": ["src/a.rs"]}), "partial"),
            (json!({"partial_files": ["src/a.rs"]}), "partial"),
            (json!({"unindexed_files": ["src/a.rs"]}), "partial"),
            (json!({"no_symbol_files": ["src/a.rs"]}), "partial"),
            (
                json!({"_meta": {"coverage": {"indexed_files": 0}}}),
                "partial",
            ),
        ];
        for (payload, expected) in cases {
            let envelope = envelope_of(payload.clone(), "find_references", json!({}));
            assert_eq!(
                envelope["completeness"]["kind"], expected,
                "payload {payload} should derive {expected}"
            );
        }
    }

    #[test]
    fn an_explanatory_coverage_note_on_a_full_index_is_not_an_incompleteness() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let srv = server(true);
        let arguments = json!({"project": project}).as_object().cloned().unwrap();

        let symbols = srv.render(&project, Ok(json!({"results": []})), "find_symbol");
        let symbols = srv
            .annotate_envelope(symbols, "find_symbol", &arguments, Duration::from_millis(1))
            .structured_content
            .unwrap();
        assert!(
            symbols["_meta"]["coverage"]["indexed_files"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{symbols}"
        );
        assert!(symbols.get("completeness").is_none(), "{symbols}");

        let references = srv.render(
            &project,
            Ok(json!({"results": [], "counts_floor": true})),
            "find_references",
        );
        let references = srv
            .annotate_envelope(
                references,
                "find_references",
                &arguments,
                Duration::from_millis(1),
            )
            .structured_content
            .unwrap();
        assert!(references["_meta"]["coverage"].is_object(), "{references}");
        assert_eq!(references["completeness"]["kind"], "floor", "{references}");
        assert_eq!(references["completeness"]["kinds"], json!(["floor"]));
    }

    #[test]
    fn a_measured_coverage_gap_is_still_partial() {
        for coverage in [
            json!({"indexed_files": 0, "indexed_by_language": {}}),
            json!({"indexed_files": 3, "semantically_indexed_files": 0}),
            json!({"indexed_files": 3, "semantically_indexed_files": 2}),
        ] {
            let envelope = envelope_of(
                json!({"results": [], "_meta": {"coverage": coverage.clone()}}),
                "search",
                json!({}),
            );
            assert_eq!(envelope["completeness"]["kind"], "partial", "{coverage}");
            assert_eq!(
                envelope["completeness"]["recover"],
                json!({"command": "codesage index"}),
                "{coverage}"
            );
        }
        let covered = envelope_of(
            json!({"results": [], "_meta": {"coverage":
                {"indexed_files": 3, "semantically_indexed_files": 3}}}),
            "search",
            json!({}),
        );
        assert_eq!(covered, json!({}));
    }

    #[test]
    fn an_empty_reason_list_is_not_an_incompleteness() {
        let signals = signals_of(json!({
            "unwalked_files": [], "unscored_files": [], "counts_floor": false, "bounded": false
        }));
        assert!(signals.severest().is_none());
    }

    #[test]
    fn kind_is_the_most_severe_and_kinds_lists_every_applicable_one() {
        let envelope = envelope_of(
            json!({
                "counts_floor": true,
                "unscored": true,
                "bounded": true,
                "unwalked_files": ["src/a.rs"],
                "_meta": {"truncated": true, "total_results": 69, "returned": 5,
                          "clamps": [{"param": "limit", "requested": 500, "applied": 100}]}
            }),
            "recommend_tests",
            json!({}),
        );
        assert_eq!(envelope["completeness"]["kind"], "truncated");
        assert_eq!(
            envelope["completeness"]["kinds"],
            json!([
                "truncated",
                "bounded",
                "partial",
                "clamped",
                "unscored",
                "floor"
            ])
        );
    }

    #[test]
    fn severity_order_holds_pairwise() {
        let pairs = [
            (json!({"truncated": true, "bounded": true}), "truncated"),
            (json!({"bounded": true, "unwalked_files": ["a"]}), "bounded"),
            (
                json!({"unwalked_files": ["a"], "_meta": {"clamps": [{"param": "limit"}]}}),
                "partial",
            ),
            (
                json!({"_meta": {"clamps": [{"param": "limit"}]}, "unscored": true}),
                "clamped",
            ),
            (json!({"unscored": true, "counts_floor": true}), "unscored"),
        ];
        for (payload, expected) in pairs {
            assert_eq!(
                envelope_of(payload.clone(), "search", json!({}))["completeness"]["kind"],
                expected,
                "{payload}"
            );
        }
    }

    #[test]
    fn truncated_recover_advances_the_offset_only_for_paged_tools() {
        let payload = json!({"_meta": {"truncated": true, "total_results": 69, "returned": 5}});
        let paged = envelope_of(
            payload.clone(),
            "search",
            json!({"project": "/p", "query": "q", "offset": 10, "limit": 5}),
        );
        let recover = &paged["completeness"]["recover"];
        assert_eq!(recover["total"], 69);
        assert_eq!(recover["returned"], 5);
        assert_eq!(recover["arguments"]["offset"], 15);
        assert_eq!(recover["arguments"]["query"], "q");

        let unpaged = envelope_of(payload, "assess_risk_diff", json!({"project": "/p"}));
        let recover = &unpaged["completeness"]["recover"];
        assert_eq!(recover["total"], 69);
        assert!(recover.get("arguments").is_none());
    }

    #[test]
    fn bounded_recover_names_the_broader_cli_surface() {
        let envelope = envelope_of(json!({"bounded": true}), "trace_call_path", json!({}));
        assert_eq!(
            envelope["completeness"]["recover"],
            json!({"command": "codesage trace"})
        );
        let envelope = envelope_of(
            json!({"callers_truncated": true}),
            "edit_check",
            json!({"project": "/p", "file_path": "src/a.rs"}),
        );
        assert_eq!(envelope["completeness"]["recover"]["tool"], "edit_check");
        assert_eq!(
            envelope["completeness"]["recover"]["arguments"]["file_path"],
            "src/a.rs"
        );
    }

    #[test]
    fn clamped_recover_pairs_requested_with_applied_for_every_param() {
        let envelope = envelope_of(
            json!({"_meta": {"clamps": [
                {"param": "limit", "requested": 500, "applied": 100},
                {"param": "offset", "requested": 5000, "applied": 1000}
            ]}}),
            "search",
            json!({}),
        );
        assert_eq!(
            envelope["completeness"]["recover"],
            json!({
                "requested": {"limit": 500, "offset": 5000},
                "applied": {"limit": 100, "offset": 1000}
            })
        );
    }

    #[test]
    fn unscored_and_partial_and_floor_carry_actionable_recovery() {
        assert_eq!(
            envelope_of(json!({"unscored": true}), "assess_risk", json!({}))["completeness"]["recover"],
            json!({"command": "codesage git-index"})
        );
        assert_eq!(
            envelope_of(
                json!({"unwalked_files": ["src/a.rs"], "no_symbol_files": ["src/b.rs"]}),
                "recommend_tests",
                json!({})
            )["completeness"]["recover"],
            json!({"unwalked_files": ["src/a.rs"], "no_symbol_files": ["src/b.rs"]})
        );
        assert_eq!(
            envelope_of(json!({"counts_floor": true}), "find_references", json!({}))["completeness"]
                ["recover"],
            json!({"reason": "name_based_edges"})
        );
    }

    #[test]
    fn handles_anywhere_in_a_payload_yield_their_paths() {
        let payload = json!({
            "entrypoints": [{"entry_path": "src/main.rs", "handle": "file:src/main.rs"}],
            "top_coupled": [{"file": "src/b.rs", "handle": "file:src/b.rs"}],
            "steps": [{"handle": "sym:src/c.rs#Worker::run"}],
            "results": [{"handle": "chunk:src/d.rs:10-20"}],
            "clustered_directories": [{"handle": "dir:crates/graph"}],
            "feature": "feat_0123456789abcdef",
            "prose": "see src/not_a_handle.rs for details"
        });
        let mut paths = Vec::new();
        collect_handle_paths(&payload, &mut paths);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "crates/graph",
                "src/b.rs",
                "src/c.rs",
                "src/d.rs",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn a_handle_naming_an_escaping_path_is_not_collected() {
        let mut paths = Vec::new();
        collect_handle_paths(
            &json!({"handle": "file:../outside.rs", "other": "file:/etc/passwd"}),
            &mut paths,
        );
        assert!(paths.is_empty());
    }

    #[test]
    fn ambiguity_mirrors_onto_target() {
        let signals = signals_of(json!({"ambiguous": true, "definition_count": 3}));
        assert!(signals.ambiguous);
        assert_eq!(signals.candidates_total, Some(3));
        let signals = signals_of(json!({"ambiguous": false, "definition_count": 1}));
        assert!(!signals.ambiguous);
    }

    #[test]
    fn stale_meta_feeds_dirty_paths() {
        let signals = signals_of(json!({"_meta": {"stale_files": ["src/a.rs", "src/b.rs"]}}));
        assert_eq!(signals.stale_paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn short_sha_trims_only_hex_stamps() {
        assert_eq!(
            short_sha("4cff7e281f4a9be9fb32dd1ea326bf9d"),
            "4cff7e281f4a"
        );
        assert_eq!(short_sha("not-a-sha"), "not-a-sha");
    }

    #[test]
    fn envelope_keys_are_the_ones_the_schema_advertises() {
        let advertised: Vec<&str> = schema_properties().iter().map(|(name, _)| *name).collect();
        assert_eq!(advertised, ENVELOPE_KEYS);
    }

    /// An indexed temp project whose files match disk, so freshness is the
    /// default and any `index.structural` is a real finding.
    fn indexed_project(files: &[(&str, &str)]) -> (TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codesage = root.join(".codesage");
        std::fs::create_dir_all(&codesage).unwrap();
        let db = Database::open(&codesage.join("index.db")).unwrap();
        for (rel, body) in files {
            let abs = root.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, body).unwrap();
            db.upsert_file(&FileInfo {
                path: (*rel).to_string(),
                language: Language::Rust,
                content_hash: codesage_parser::discover::content_hash(body.as_bytes()),
                is_test: false,
            })
            .unwrap();
        }
        drop(db);
        let project = root.to_str().unwrap().to_owned();
        (dir, project)
    }

    /// Two registered models over one indexed file; only `chunks/populated`
    /// holds semantic rows, and `active` is what the project config selects.
    fn project_with_two_models(active: &str) -> (TempDir, String) {
        const BODY: &str = "fn a() {}";
        let (dir, project) = indexed_project(&[("src/a.rs", BODY)]);
        let db_path = Path::new(&project).join(".codesage").join("index.db");
        drop(Database::open_for_model(&db_path, "chunks/empty", 4).unwrap());
        let populated = Database::open_for_model(&db_path, "chunks/populated", 4).unwrap();
        populated
            .upsert_semantic_file_hash(
                "src/a.rs",
                &codesage_parser::discover::content_hash(BODY.as_bytes()),
            )
            .unwrap();
        drop(populated);
        std::fs::write(
            Path::new(&project).join(".codesage").join("config.toml"),
            format!("[embedding]\nmodel = \"{active}\"\ndevice = \"cpu\"\n"),
        )
        .unwrap();
        (dir, project)
    }

    fn semantic_state_for(active: &str) -> Value {
        let (_dir, project) = project_with_two_models(active);
        let out = enveloped(
            &server(true),
            "find_symbol",
            &project,
            json!({"results": [{"file_path": "src/a.rs"}]}),
        );
        out["index"].clone()
    }

    #[test]
    fn semantic_coverage_reads_the_active_model_not_every_chunk_table() {
        // A model switch without a reindex leaves the retired model's rows
        // covering every file; the active model has none, and `search` with it
        // returns nothing.
        let index = semantic_state_for("chunks/empty");
        assert_eq!(index["semantic"], "none", "{index}");
    }

    #[test]
    fn a_model_whose_chunks_cover_the_index_reports_no_semantic_gap() {
        let index = semantic_state_for("chunks/populated");
        assert!(index.get("semantic").is_none(), "{index}");
    }

    fn server(envelope_enabled: bool) -> CodeSageServer {
        let mut state = CodeSageServerState::new();
        state.envelope_enabled = envelope_enabled;
        CodeSageServer::with_state(Arc::new(state))
    }

    fn enveloped(server: &CodeSageServer, tool: &str, project: &str, payload: Value) -> Value {
        let arguments = json!({"project": project}).as_object().cloned().unwrap();
        let result = server.annotate_envelope(
            render_with_kind(Ok(payload), tool),
            tool,
            &arguments,
            Duration::from_millis(11),
        );
        result.structured_content.unwrap()
    }

    #[test]
    fn a_fresh_exact_response_adds_only_tool_generation_and_cost() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let srv = server(true);
        let payload = json!({"results": [{"file_path": "src/a.rs", "line": 1}]});
        let before = serde_json::to_vec(&payload).unwrap().len();
        let out = enveloped(&srv, "find_symbol", &project, payload);

        assert_eq!(out["tool"], "find_symbol");
        assert!(out["index"]["generation"].is_u64());
        assert!(out["index"].get("structural").is_none(), "{out}");
        assert!(out["index"].get("dirty_paths").is_none(), "{out}");
        assert!(out.get("completeness").is_none(), "{out}");
        assert!(out.get("target").is_none(), "{out}");
        assert_eq!(out["cost"]["ms"], 11);
        assert!(out["cost"]["bytes"].as_u64().unwrap() > 0);

        let mut keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["cost", "index", "results", "tool"]);
        let after = serde_json::to_vec(&out).unwrap().len();
        assert!(
            after - before < 120,
            "envelope added {} bytes over {before}",
            after - before
        );
    }

    #[test]
    fn legacy_mode_suppresses_every_envelope_key() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let payload = json!({"results": [], "counts_floor": true, "_meta": {"truncated": true}});
        let out = enveloped(&server(false), "find_references", &project, payload.clone());
        for key in ["tool", "index", "target", "completeness", "cost"] {
            assert!(
                out.get(key).is_none(),
                "`{key}` survived legacy mode: {out}"
            );
        }
        assert_eq!(out["counts_floor"], true);

        let on = enveloped(&server(true), "find_references", &project, payload);
        assert_eq!(on["completeness"]["kind"], "truncated");
    }

    #[test]
    fn internal_tools_keep_their_raw_payload() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        for tool in ["embed_texts", "rerank_pairs"] {
            let out = enveloped(&server(true), tool, &project, json!({"scores": [0.5]}));
            assert!(out.get("tool").is_none(), "{tool} was enveloped: {out}");
            assert!(out.get("cost").is_none(), "{tool} was enveloped: {out}");
        }
    }

    #[test]
    fn edit_check_carries_the_index_block() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let out = enveloped(
            &server(true),
            "edit_check",
            &project,
            json!({"file_path": "src/a.rs", "symbol_name": "a", "callers": []}),
        );
        assert_eq!(out["tool"], "edit_check");
        assert!(out["index"]["generation"].is_u64(), "{out}");
    }

    #[test]
    fn a_project_without_an_index_still_reports_tool_and_cost() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let out = enveloped(
            &server(true),
            "edit_check",
            &project,
            json!({"found": true}),
        );
        assert_eq!(out["tool"], "edit_check");
        assert!(out.get("index").is_none(), "{out}");
        assert!(out["cost"]["bytes"].as_u64().is_some());
    }

    #[test]
    fn a_changed_file_becomes_a_dirty_path_and_marks_the_index_dirty() {
        let (dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        std::fs::write(dir.path().join("src/a.rs"), "fn a() { edited() }").unwrap();
        let srv = server(true);
        // `annotate_staleness` is what populates `_meta.stale_files`; the
        // envelope promotes it to `index.dirty_paths`.
        let rendered = srv.annotate_staleness(
            &project,
            render_with_kind(
                Ok(json!({"results": [{"file_path": "src/a.rs"}]})),
                "search",
            ),
        );
        let arguments = json!({"project": project}).as_object().cloned().unwrap();
        let out = srv
            .annotate_envelope(rendered, "search", &arguments, Duration::from_millis(1))
            .structured_content
            .unwrap();
        assert_eq!(out["index"]["dirty_paths"], json!(["src/a.rs"]));
        assert_eq!(out["index"]["structural"], "dirty");
    }

    #[test]
    fn handle_free_path_fields_and_handles_both_reach_staleness() {
        let (dir, project) = indexed_project(&[
            ("src/entry.rs", "fn main() {}"),
            ("tests/reach.rs", "fn t() {}"),
            ("src/coupled.rs", "fn c() {}"),
        ]);
        for rel in ["src/entry.rs", "tests/reach.rs", "src/coupled.rs"] {
            std::fs::write(dir.path().join(rel), "// edited\n").unwrap();
        }
        let srv = server(true);
        // `entry_path` (project_overview) and `via` (recommend_tests) carry no
        // handle; `top_coupled[].file` carries one.
        let payload = json!({
            "entrypoints": [{"entry_path": "src/entry.rs", "feature_id": "feat_0123456789abcdef"}],
            "reachable": [{"path": "tests/other.rs", "via": "tests/reach.rs"}],
            "top_coupled": [{"handle": "file:src/coupled.rs"}],
            "snapshot_path": dir.path().join(".codesage/sessions/default.json").to_string_lossy(),
        });
        let out = srv
            .annotate_staleness(&project, render_with_kind(Ok(payload), "project_overview"))
            .structured_content
            .unwrap();
        let stale = out["_meta"]["stale_files"].as_array().unwrap();
        let stale: Vec<&str> = stale.iter().filter_map(Value::as_str).collect();
        assert!(stale.contains(&"src/entry.rs"), "{stale:?}");
        assert!(stale.contains(&"tests/reach.rs"), "{stale:?}");
        assert!(stale.contains(&"src/coupled.rs"), "{stale:?}");
    }

    #[test]
    fn an_ambiguous_result_mirrors_onto_the_target_block() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let out = enveloped(
            &server(true),
            "find_references",
            &project,
            json!({"results": [], "counts_floor": true, "ambiguous": true, "definition_count": 3}),
        );
        assert_eq!(
            out["target"],
            json!({"ambiguous": true, "candidates_total": 3})
        );
        assert_eq!(out["completeness"]["kind"], "floor");
    }

    #[test]
    fn an_envelope_key_already_owned_by_a_payload_is_never_overwritten() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let out = enveloped(
            &server(true),
            "search",
            &project,
            json!({"tool": "payload owns this", "results": []}),
        );
        assert_eq!(out["tool"], "payload owns this");
    }

    #[test]
    fn the_text_block_clients_read_carries_the_envelope() {
        let (_dir, project) = indexed_project(&[("src/a.rs", "fn a() {}")]);
        let arguments = json!({"project": project}).as_object().cloned().unwrap();
        let result = server(true).annotate_envelope(
            render_with_kind(Ok(json!({"results": []})), "search"),
            "search",
            &arguments,
            Duration::from_millis(3),
        );
        let text = &result.content.last().unwrap().as_text().unwrap().text;
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["tool"], "search");
        assert_eq!(parsed["cost"]["ms"], 3);
    }
}
