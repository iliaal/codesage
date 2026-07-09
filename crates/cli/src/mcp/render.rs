use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use rmcp::model::{CallToolResult, ContentBlock};

use super::CodeSageServer;

impl CodeSageServer {
    /// Char budget for a context-bundle response, sized to the project's
    /// indexed file count (see [`mcp_bundle_token_budget`]). Falls back to a
    /// mid-tier default — still honoring the `CODESAGE_BUNDLE_TOKEN_BUDGET`
    /// override — when the file count can't be read.
    pub(super) fn bundle_budget_chars(&self, project: &str) -> usize {
        let tokens = match self.with_project_db(project, |db| db.file_count()) {
            Ok(count) => mcp_bundle_token_budget(count),
            Err(_) => mcp_bundle_token_budget(1000),
        };
        tokens * MCP_CHARS_PER_TOKEN
    }

    /// Render a tool result, then annotate it with a staleness banner if any of
    /// the files it references have changed on disk since indexing. Handlers
    /// route through this instead of the free `render_with_kind` so the project
    /// context needed to stat files is available.
    pub(super) fn render<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
    ) -> CallToolResult {
        self.annotate_staleness(project, render_with_kind(r, kind))
    }

    /// [`Self::render`] with an explicit char budget (context-bundle tools).
    pub(super) fn render_budget<T: serde::Serialize>(
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
        content.push(ContentBlock::text(banner));
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
            let Some(path) = confined_project_path(&root, rel) else {
                stale.push(rel.clone());
                continue;
            };
            match std::fs::read(path) {
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
}

fn confined_project_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    let candidate = root.join(rel_path);
    match candidate.canonicalize() {
        Ok(canonical) if canonical.starts_with(&root) => Some(canonical),
        Ok(_) => None,
        // Missing indexed files are reported stale by the caller. Component
        // checks above already ruled out absolute paths and `..` escapes.
        Err(_) => Some(candidate),
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
            result.content = vec![ContentBlock::text(text)];
            result
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Error: {e:#}"))]),
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

/// Array fields that carry a documented per-element invariant and must not
/// be silently trimmed: `assess_risk_diff` promises one `files` entry per
/// patch file, and rollup arrays (`test_gap_files`, ...) cross-reference
/// `files` / `clustered_directories` by name. Budget truncation prefers any
/// other array; when a protected array is the only option, the dropped
/// element identifiers are recorded under `_meta.dropped_files` so the
/// invariant is at least visibly broken, never silently (CR-038).
const PROTECTED_TRUNCATION_KEYS: &[&str] = &["files", "clustered_directories"];

/// Best-effort path/name identifier for a truncated array element, used to
/// populate `_meta.dropped_files`.
fn element_identifier(item: &serde_json::Value) -> Option<String> {
    let obj = item.as_object()?;
    for key in ["file_path", "file", "path", "directory", "name"] {
        if let Some(serde_json::Value::String(s)) = obj.get(key) {
            return Some(s.clone());
        }
    }
    None
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
            // Pick the largest top-level array field and trim it. Protected
            // arrays (per-element invariants, see PROTECTED_TRUNCATION_KEYS)
            // are only eligible when no other array exists to trim.
            let mut largest_key: Option<String> = None;
            let mut largest_len = 0;
            let mut largest_protected_key: Option<String> = None;
            let mut largest_protected_len = 0;
            for (k, v) in &map {
                if let serde_json::Value::Array(arr) = v {
                    let s = serde_json::to_string(arr).map(|s| s.len()).unwrap_or(0);
                    if PROTECTED_TRUNCATION_KEYS.contains(&k.as_str()) {
                        if s > largest_protected_len {
                            largest_protected_len = s;
                            largest_protected_key = Some(k.clone());
                        }
                    } else if s > largest_len {
                        largest_len = s;
                        largest_key = Some(k.clone());
                    }
                }
            }
            let chosen = match (largest_key, largest_protected_key) {
                (Some(k), _) => Some((k, largest_len, false)),
                (None, Some(k)) => Some((k, largest_protected_len, true)),
                (None, None) => None,
            };
            if let Some((key, key_len, protected)) = chosen
                && let Some(serde_json::Value::Array(items)) = map.remove(&key)
            {
                let total = items.len();
                let other_chars = initial_len.saturating_sub(key_len);
                let remaining = budget_chars.saturating_sub(other_chars);
                // truncate_array keeps a prefix, so identifiers collected
                // up-front let us name exactly the dropped tail elements.
                let identifiers: Vec<Option<String>> = if protected {
                    items.iter().map(element_identifier).collect()
                } else {
                    Vec::new()
                };
                let kept = truncate_array(items, remaining);
                let returned = kept.len();
                map.insert(key.clone(), serde_json::Value::Array(kept));
                let mut meta = serde_json::json!({
                    "truncated": true,
                    "kind": kind,
                    "field": key,
                    "total_results": total,
                    "returned": returned,
                    "approx_tokens_budget": approx_tokens_budget,
                    "hint": "output exceeded budget; refine query or narrow scope",
                });
                if protected {
                    let dropped = &identifiers[returned..];
                    let named: Vec<&str> = dropped.iter().filter_map(|id| id.as_deref()).collect();
                    let meta_obj = meta.as_object_mut().expect("json! object");
                    if !named.is_empty() {
                        meta_obj.insert("dropped_files".to_string(), serde_json::json!(named));
                    }
                    if named.len() < dropped.len() {
                        meta_obj.insert(
                            "dropped_count".to_string(),
                            serde_json::json!(dropped.len()),
                        );
                    }
                }
                map.insert("_meta".to_string(), meta);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codesage_protocol::Language;
    use codesage_storage::Database;
    use serde_json::{Value, json};

    use super::*;
    use crate::mcp::CodeSageServerState;

    fn fat_string(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn cap_passes_through_when_under_budget() {
        let v = json!([{"name": "a"}, {"name": "b"}]);
        let out = cap_to_budget_with(v.clone(), "test", MCP_BUDGET_CHARS);
        assert_eq!(out, v);
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
    fn cap_prefers_unprotected_array_over_protected_files() {
        // assess_risk_diff shape: `files` carries the one-entry-per-patch-file
        // invariant. Even when `files` is the LARGEST array, truncation must
        // trim another array instead (CR-038) — rollups like test_gap_files
        // cross-reference `files` by name.
        let files: Vec<Value> = (0..40)
            .map(|i| json!({"file": format!("src/f{i}.rs"), "blob": fat_string(1000)}))
            .collect();
        let notes: Vec<Value> = (0..20).map(|i| json!(format!("note {i}"))).collect();
        let v = json!({
            "files": files,
            "summary_notes": notes,
            "max_score": 0.9,
        });
        let out = cap_to_budget_with(v, "assess_risk_diff", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        assert_eq!(
            obj["files"].as_array().unwrap().len(),
            40,
            "protected `files` must survive intact while another array exists"
        );
        assert_eq!(obj["_meta"]["field"], json!("summary_notes"));
        assert_eq!(obj["_meta"]["truncated"], json!(true));
    }

    #[test]
    fn cap_records_dropped_files_when_protected_array_is_only_option() {
        // When `files` is the only truncatable array, it may be trimmed —
        // but the dropped entries must be named in _meta.dropped_files so
        // the broken invariant is visible, never silent.
        let files: Vec<Value> = (0..40)
            .map(|i| json!({"file": format!("src/f{i}.rs"), "blob": fat_string(1000)}))
            .collect();
        let v = json!({ "files": files, "max_score": 0.9 });
        let out = cap_to_budget_with(v, "assess_risk_diff", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["field"], json!("files"));
        let returned = meta["returned"].as_u64().unwrap() as usize;
        assert!(returned > 0 && returned < 40);
        let dropped = meta["dropped_files"].as_array().expect("dropped_files");
        assert_eq!(dropped.len(), 40 - returned, "every dropped file named");
        assert_eq!(dropped[0], json!(format!("src/f{returned}.rs")));
        assert!(
            meta.get("dropped_count").is_none(),
            "all elements had identifiers, no count fallback needed"
        );
    }

    #[test]
    fn cap_counts_dropped_elements_without_identifiers() {
        // Protected-array elements with no path/name field fall back to a
        // dropped_count so the truncation is still visible.
        let files: Vec<Value> = (0..40).map(|_| json!({"blob": fat_string(1000)})).collect();
        let v = json!({ "files": files });
        let out = cap_to_budget_with(v, "assess_risk_diff", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        let returned = meta["returned"].as_u64().unwrap() as usize;
        assert_eq!(meta["dropped_count"], json!(40 - returned));
        assert!(meta.get("dropped_files").is_none());
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
    fn confined_project_path_rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        assert!(confined_project_path(&root, "../secret.rs").is_none());
        assert!(confined_project_path(&root, "src/lib.rs").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn confined_project_path_canonicalizes_symlink_root() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        std::fs::create_dir_all(real_root.join("src")).unwrap();
        std::fs::write(real_root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let link_root = dir.path().join("link");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        let resolved = confined_project_path(&link_root, "src/lib.rs").unwrap();

        assert_eq!(
            resolved,
            real_root.join("src/lib.rs").canonicalize().unwrap()
        );
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

    #[test]
    fn staleness_refuses_absolute_indexed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let outside = dir.path().join("secret.rs");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(&outside, "fn secret() {}\n").unwrap();
        let outside_path = outside.to_string_lossy().into_owned();
        let db = Database::open(&root.join(".codesage/index.db")).unwrap();
        db.upsert_file(&codesage_protocol::FileInfo {
            path: outside_path.clone(),
            language: Language::Rust,
            content_hash: codesage_parser::discover::content_hash(b"fn secret() {}\n"),
        })
        .unwrap();
        drop(db);

        let server = CodeSageServer::with_state(Arc::new(CodeSageServerState::new()));
        let stale = server
            .compute_stale_files(root.to_str().unwrap(), std::slice::from_ref(&outside_path))
            .unwrap();

        assert_eq!(stale, vec![outside_path]);
    }
}
