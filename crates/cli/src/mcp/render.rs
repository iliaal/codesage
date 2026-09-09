use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use rmcp::model::{CallToolResult, ContentBlock};

use super::CodeSageServer;

impl CodeSageServer {
    /// Scale bundle budgets by indexed file count; failed counts use the mid-tier default.
    pub(super) fn bundle_budget_chars(&self, project: &str) -> usize {
        let tokens = match self.with_project_db(project, |db| db.file_count()) {
            Ok(count) => mcp_bundle_token_budget(count),
            Err(_) => mcp_bundle_token_budget(1000),
        };
        tokens * MCP_CHARS_PER_TOKEN
    }

    /// Render with project context for coverage and on-disk staleness checks.
    pub(super) fn render<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
    ) -> CallToolResult {
        self.render_coverage_gated(project, r, kind, true)
    }

    /// Suppress coverage notes for intentionally empty pages (zero limit or exhausted offset).
    pub(super) fn render_coverage_gated<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
        allow_coverage: bool,
    ) -> CallToolResult {
        let rendered = render_with_kind(r, kind);
        let covered = if allow_coverage {
            self.annotate_coverage(project, kind, rendered)
        } else {
            rendered
        };
        super::next::annotate(project, kind, self.annotate_staleness(project, covered))
    }

    /// [`Self::render`] with an explicit char budget (context-bundle tools).
    pub(super) fn render_budget<T: serde::Serialize>(
        &self,
        project: &str,
        r: Result<T>,
        kind: &str,
        budget_chars: usize,
    ) -> CallToolResult {
        super::next::annotate(
            project,
            kind,
            self.annotate_staleness(project, render_with_budget(r, kind, budget_chars)),
        )
    }

    /// Exclude impact_analysis: no dependents can be a correct leaf result.
    const COVERAGE_ANNOTATED_TOOLS: [&str; 4] =
        ["search", "find_symbol", "find_references", "find_similar"];

    /// Disclose index coverage on ambiguous empty results; annotation failures leave the result intact.
    fn annotate_coverage(
        &self,
        project: &str,
        kind: &str,
        mut result: CallToolResult,
    ) -> CallToolResult {
        if result.is_error == Some(true) || !Self::COVERAGE_ANNOTATED_TOOLS.contains(&kind) {
            return result;
        }
        let Some(structured) = result.structured_content.as_ref() else {
            return result;
        };
        if !has_empty_results(structured) {
            return result;
        }
        let counts = match self.with_project_db(project, |db| db.file_counts_by_language()) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "coverage annotation skipped");
                return result;
            }
        };
        let total: usize = counts.iter().map(|(_, n)| n).sum();
        // Search sees only semantic files for the active model; structural or all-model counts overstate coverage.
        let semantic_files = if kind == "search" {
            self.resolve_project(project).ok().and_then(|st| {
                let model = st.embedding_config.model.clone();
                if model.is_empty() {
                    return None;
                }
                self.with_project_db(project, |db| db.semantic_file_count_for_model(&model))
                    .ok()
            })
        } else {
            None
        };
        if let Some(mut structured) = result.structured_content.take() {
            merge_coverage_meta(&mut structured, total, &counts, semantic_files);
            result.structured_content = Some(structured);
        }
        let note = if total == 0 {
            "No matches, and nothing is indexed: this project has zero indexed files, so the \
             empty result says nothing about the code. Run `codesage index`."
                .to_string()
        } else {
            let breakdown = counts
                .iter()
                .map(|(lang, n)| format!("{lang} {n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let semantic_note = match semantic_files {
                Some(0) => " None of them have semantic chunks, so this search matched nothing \
                             because nothing was semantically indexed — run `codesage index` \
                             without `--no-semantic`."
                    .to_string(),
                Some(n) if n < total => {
                    format!(
                        " Only {n} of them are semantically indexed, and `search` sees only those."
                    )
                }
                _ => String::new(),
            };
            format!(
                "No matches. The index holds {total} file(s): {breakdown}.{semantic_note} An empty \
                 result means no match *within* that set — it is not evidence the code is absent. \
                 If the language or directory you expected is missing above, it was never indexed: \
                 check `[index] exclude_patterns` in .codesage/config.toml, run `codesage coverage` \
                 to see what indexing cannot reach, and `codesage index` to refresh."
            )
        };
        let existing = std::mem::take(&mut result.content);
        let mut content = Vec::with_capacity(existing.len() + 1);
        content.push(ContentBlock::text(note));
        content.extend(existing);
        result.content = content;
        result
    }

    /// Best-effort stale-path annotation; unreadable metadata must not fail the tool call.
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

    /// Changed, missing, or unreadable indexed paths are stale; unindexed references are skipped.
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

/// Leave headroom below client response limits.
const MCP_TOKEN_BUDGET: usize = 8000;
/// Approximate token cost for response caps.
const MCP_CHARS_PER_TOKEN: usize = 4;
const MCP_BUDGET_CHARS: usize = MCP_TOKEN_BUDGET * MCP_CHARS_PER_TOKEN;

/// Scale bundle budgets monotonically with repository size; the environment can override.
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

/// Return both readable JSON and structured content; errors retain the full anyhow cause chain.
pub(super) fn render_with_kind<T: serde::Serialize>(r: Result<T>, kind: &str) -> CallToolResult {
    render_with_budget(r, kind, MCP_BUDGET_CHARS)
}

fn render_with_budget<T: serde::Serialize>(
    r: Result<T>,
    kind: &str,
    budget_chars: usize,
) -> CallToolResult {
    match r {
        Ok(v) => {
            let value = serde_json::to_value(&v).unwrap_or(serde_json::Value::Null);
            let capped = cap_to_budget_with(value, kind, budget_chars);
            // MCP requires a structured-content object; normalize arrays regardless of budget.
            let structured = match capped {
                serde_json::Value::Array(items) => serde_json::json!({ "results": items }),
                other => other,
            };
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            let mut result = CallToolResult::structured(structured);
            // Override rmcp's compact JSON with readable transcript output.
            result.content = vec![ContentBlock::text(text)];
            result
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Error: {e:#}"))]),
    }
}

/// Bound per-response disk hashing even for unusually broad results.
const STALENESS_MAX_FILES: usize = 50;

/// Path-valued fields, including nested arrays. `imports` contains module names,
/// while `imported_by` contains file paths; `source` is a mapper token.
/// Omitted files and directory-only fields are not checked.
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
    "unscored_files",
    // `files` may contain paths or records; record fields are visited recursively.
    "files",
    "imported_by",
    "primary",
    "new_cycles",
    "resolved_cycles",
];

/// Staleness checking is on by default; `CODESAGE_STALENESS_CHECK` set to a
/// falsey value disables it (per-response stat+hash of referenced files).
fn staleness_enabled() -> bool {
    !matches!(
        std::env::var("CODESAGE_STALENESS_CHECK").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Recurse through path arrays, not objects: record strings may be prose.
/// The caller visits records' own path-valued fields separately.
fn push_path_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                push_path_strings(item, out);
            }
        }
        _ => {}
    }
}

fn collect_referenced_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if PATH_KEYS.contains(&k.as_str()) {
                    push_path_strings(v, out);
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

/// Only an empty `results` array qualifies; absent keys represent other response shapes.
fn has_empty_results(structured: &serde_json::Value) -> bool {
    structured
        .get("results")
        .and_then(|r| r.as_array())
        .is_some_and(|a| a.is_empty())
}

/// Record index composition under `_meta.coverage`, merging into any existing
/// `_meta` rather than overwriting it.
fn merge_coverage_meta(
    structured: &mut serde_json::Value,
    total: usize,
    counts: &[(String, usize)],
    semantic_files: Option<usize>,
) {
    let serde_json::Value::Object(map) = structured else {
        return;
    };
    let meta = map
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let serde_json::Value::Object(meta) = meta else {
        return;
    };
    let by_language: serde_json::Map<String, serde_json::Value> = counts
        .iter()
        .map(|(lang, n)| (lang.clone(), serde_json::Value::from(*n)))
        .collect();
    let mut coverage = serde_json::Map::new();
    coverage.insert("indexed_files".to_string(), serde_json::Value::from(total));
    coverage.insert(
        "indexed_by_language".to_string(),
        serde_json::Value::Object(by_language),
    );
    if let Some(n) = semantic_files {
        coverage.insert(
            "semantically_indexed_files".to_string(),
            serde_json::Value::from(n),
        );
    }
    coverage.insert(
        "note".to_string(),
        serde_json::Value::String(
            "empty result means no match within the indexed set, not that the code is absent; \
             a language missing from indexed_by_language was never indexed"
                .to_string(),
        ),
    );
    meta.insert("coverage".to_string(), serde_json::Value::Object(coverage));
}

/// Merge staleness with existing envelope metadata.
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

/// Disclose requested-versus-applied numeric caps in `_meta.clamps`.
#[derive(Debug, Clone)]
pub(super) struct ClampNote {
    pub(super) param: &'static str,
    pub(super) requested: serde_json::Value,
    pub(super) applied: serde_json::Value,
}

/// Merge clamp notes into successful structured results; leave unclamped results unchanged.
pub(super) fn annotate_clamps(mut result: CallToolResult, notes: &[ClampNote]) -> CallToolResult {
    if notes.is_empty() || result.is_error == Some(true) {
        return result;
    }
    let Some(structured) = result.structured_content.as_mut() else {
        return result;
    };
    let serde_json::Value::Object(map) = structured else {
        return result;
    };
    let meta = map
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let serde_json::Value::Object(meta) = meta else {
        return result;
    };
    meta.insert(
        "clamps".to_string(),
        serde_json::Value::Array(
            notes
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "param": n.param,
                        "requested": n.requested,
                        "applied": n.applied,
                    })
                })
                .collect(),
        ),
    );
    result
}

/// Mark successful debug-override responses without replacing other metadata.
pub(super) fn annotate_test_override(mut result: CallToolResult, active: bool) -> CallToolResult {
    if !active || result.is_error == Some(true) {
        return result;
    }
    let Some(structured) = result.structured_content.as_mut() else {
        return result;
    };
    let serde_json::Value::Object(map) = structured else {
        return result;
    };
    let meta = map
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let serde_json::Value::Object(meta) = meta else {
        return result;
    };
    meta.insert("test_override".to_string(), serde_json::Value::Bool(true));
    result
}

/// These arrays promise per-element coverage or support cross-references.
/// Prefer trimming elsewhere; if unavoidable, disclose dropped identities.
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

/// Trim oversized arrays and disclose truncation. Protected arrays can force budget overshoot.
fn cap_to_budget_with(
    value: serde_json::Value,
    kind: &str,
    budget_chars: usize,
) -> serde_json::Value {
    let approx_tokens_budget = budget_chars / MCP_CHARS_PER_TOKEN;
    let hint = budget_hint(kind);
    let initial_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if initial_len <= budget_chars {
        return value;
    }

    match value {
        serde_json::Value::Array(items) => {
            let total = items.len();
            let (kept, nested) = truncate_array_reporting(items, budget_chars);
            let returned = kept.len();
            let mut out = serde_json::json!({
                "results": kept,
                "_meta": {
                    "truncated": true,
                    "kind": kind,
                    "total_results": total,
                    "returned": returned,
                    "approx_tokens_budget": approx_tokens_budget,
                    "hint": hint,
                }
            });
            if let Some(nested) = nested {
                let meta = &mut out["_meta"];
                meta.as_object_mut().expect("json! object").insert(
                    "also_truncated_fields".to_string(),
                    serde_json::json!([nested.label("results")]),
                );
                merge_dropped_identities(meta, &nested.dropped_named, nested.dropped_total);
            }
            out
        }
        serde_json::Value::Object(mut map) => {
            // Several arrays may exceed the budget; trimming only the largest is insufficient.
            let mut meta: Option<serde_json::Value> = None;
            let mut also_truncated: Vec<String> = Vec::new();
            let mut trimmed: Vec<String> = Vec::new();
            let mut nested_dropped: Vec<String> = Vec::new();
            let mut nested_dropped_total = 0usize;
            loop {
                let current_len = serde_json::to_string(&map).map(|s| s.len()).unwrap_or(0);
                if meta.is_some() && current_len <= budget_chars {
                    break;
                }
                let Some((key, key_len, protected)) = largest_trimmable_array(&map, &trimmed)
                else {
                    break;
                };
                // After any unprotected trim, preserving per-element invariants outranks the budget.
                if protected && meta.is_some() {
                    break;
                }
                let Some(serde_json::Value::Array(items)) = map.remove(&key) else {
                    break;
                };
                let total = items.len();
                let other_chars = current_len.saturating_sub(key_len);
                let remaining = budget_chars.saturating_sub(other_chars);
                // Prefix truncation lets original positions identify the dropped tail.
                let identifiers: Vec<Option<String>> = if protected {
                    items.iter().map(element_identifier).collect()
                } else {
                    Vec::new()
                };
                let (kept, nested) = truncate_array_reporting(items, remaining);
                let returned = kept.len();
                map.insert(key.clone(), serde_json::Value::Array(kept));
                trimmed.push(key.clone());
                if let Some(nested) = nested {
                    also_truncated.push(nested.label(&key));
                    nested_dropped.extend(nested.dropped_named);
                    nested_dropped_total += nested.dropped_total;
                }
                if meta.is_some() {
                    // Keep headline counts scoped to the first trimmed field; report additional cuts separately.
                    also_truncated.push(format!("{key} ({returned}/{total})"));
                    continue;
                }
                let mut first = serde_json::json!({
                    "truncated": true,
                    "kind": kind,
                    "field": key,
                    "total_results": total,
                    "returned": returned,
                    "approx_tokens_budget": approx_tokens_budget,
                    "hint": hint,
                });
                if protected {
                    let dropped = &identifiers[returned..];
                    let named: Vec<&str> = dropped.iter().filter_map(|id| id.as_deref()).collect();
                    let meta_obj = first.as_object_mut().expect("json! object");
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
                meta = Some(first);
            }
            if let Some(mut meta) = meta {
                if !also_truncated.is_empty() {
                    meta.as_object_mut().expect("json! object").insert(
                        "also_truncated_fields".to_string(),
                        serde_json::json!(also_truncated),
                    );
                }
                merge_dropped_identities(&mut meta, &nested_dropped, nested_dropped_total);
                map.insert("_meta".to_string(), meta);
            }
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

/// Tools whose params carry an `offset` argument; only their truncation hint
/// may advise paging, since the advice is unsatisfiable anywhere else.
pub(super) const OFFSET_PAGED_KINDS: &[&str] = &["search"];

/// Truncation hint keyed on the tool, independent of whether its payload is a
/// bare array or an object envelope.
fn budget_hint(kind: &str) -> &'static str {
    if OFFSET_PAGED_KINDS.contains(&kind) {
        "output exceeded budget; refine query, narrow scope (paths/language), or call with offset to paginate"
    } else {
        "output exceeded budget; refine query or narrow scope"
    }
}

/// Largest top-level array in `map` that is not already in `skip`, as
/// `(key, serialized_len, is_protected)`. Unprotected arrays win outright;
/// a protected one is returned only when no unprotected array is left.
fn largest_trimmable_array(
    map: &serde_json::Map<String, serde_json::Value>,
    skip: &[String],
) -> Option<(String, usize, bool)> {
    let mut open: Option<(String, usize)> = None;
    let mut protected: Option<(String, usize)> = None;
    for (k, v) in map {
        if k == "_meta" || skip.iter().any(|s| s == k) {
            continue;
        }
        let serde_json::Value::Array(arr) = v else {
            continue;
        };
        let len = serde_json::to_string(arr).map(|s| s.len()).unwrap_or(0);
        let slot = if PROTECTED_TRUNCATION_KEYS.contains(&k.as_str()) {
            &mut protected
        } else {
            &mut open
        };
        if slot.as_ref().is_none_or(|(_, best)| len > *best) {
            *slot = Some((k.clone(), len));
        }
    }
    match (open, protected) {
        (Some((k, len)), _) => Some((k, len, false)),
        (None, Some((k, len))) => Some((k, len, true)),
        (None, None) => None,
    }
}

/// Disclose truncation inside a surviving element, not just dropped top-level rows.
struct NestedTrim {
    index: usize,
    field: String,
    kept: usize,
    total: usize,
    /// Protected nested arrays follow the top-level identity-disclosure contract.
    dropped_named: Vec<String>,
    /// How many entries were dropped from a protected nested array; 0 when the
    /// trimmed array was unprotected.
    dropped_total: usize,
}

impl NestedTrim {
    /// `files[0].cycle_files (50/4200)` — the outer array's key, the element
    /// index, and the nested field's kept/total counts.
    fn label(&self, outer_key: &str) -> String {
        format!(
            "{outer_key}[{}].{} ({}/{})",
            self.index, self.field, self.kept, self.total
        )
    }
}

/// Keep the longest prefix of `items` that fits in `budget_chars`. Returns the
/// kept prefix plus any nested trim performed on a surviving element.
fn truncate_array_reporting(
    items: Vec<serde_json::Value>,
    budget_chars: usize,
) -> (Vec<serde_json::Value>, Option<NestedTrim>) {
    let mut kept = Vec::new();
    let mut used = 0;
    let mut nested = None;
    for mut item in items {
        let s = serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
        if used + s > budget_chars {
            if !kept.is_empty() {
                break;
            }
            // Keep at least one result, shrinking its content before nested arrays.
            let remaining = budget_chars.saturating_sub(used);
            shrink_content_field(&mut item, remaining);
            let shrunk = serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
            if shrunk > remaining {
                // Large nested arrays (such as an import cycle) can dominate a single row.
                // Trim one level down, then retry content with the freed space.
                nested = shrink_largest_nested_array(&mut item, remaining).map(|mut t| {
                    t.index = kept.len();
                    t
                });
                shrink_content_field(&mut item, remaining);
            }
            kept.push(item);
            break;
        }
        used += s;
        kept.push(item);
    }
    (kept, nested)
}

/// Sample dropped nested identities so their disclosure cannot undo the budget savings.
const NESTED_DROPPED_SAMPLE: usize = 10;

/// Nested protected arrays may contain bare paths rather than records.
fn nested_element_identifier(entry: &serde_json::Value) -> Option<String> {
    match entry {
        serde_json::Value::String(s) => Some(s.clone()),
        other => element_identifier(other),
    }
}

/// Trim one nested array, preferring unprotected fields and disclosing protected drops.
fn shrink_largest_nested_array(
    item: &mut serde_json::Value,
    budget_chars: usize,
) -> Option<NestedTrim> {
    let map = item.as_object_mut()?;
    let (key, key_len, protected) = largest_trimmable_array(map, &[])?;
    let item_len = serde_json::to_string(&*map).map(|s| s.len()).unwrap_or(0);
    let remaining = budget_chars.saturating_sub(item_len.saturating_sub(key_len));
    let serde_json::Value::Array(entries) = map.remove(&key)? else {
        return None;
    };
    let total = entries.len();
    let identifiers: Vec<Option<String>> = if protected {
        entries.iter().map(nested_element_identifier).collect()
    } else {
        Vec::new()
    };
    let mut inner = Vec::new();
    // `remaining` was measured against the array including its brackets, which
    // the per-entry lengths below do not carry.
    let mut used = 2;
    for entry in entries {
        // +1 for the separating comma, which the entry lengths also omit.
        let s = serde_json::to_string(&entry).map(|s| s.len()).unwrap_or(0) + 1;
        if used + s > remaining {
            break;
        }
        used += s;
        inner.push(entry);
    }
    let count = inner.len();
    map.insert(key.clone(), serde_json::Value::Array(inner));
    if count == total {
        return None;
    }
    let dropped = identifiers.get(count..).unwrap_or(&[]);
    Some(NestedTrim {
        index: 0,
        field: key,
        kept: count,
        total,
        dropped_named: dropped
            .iter()
            .flatten()
            .take(NESTED_DROPPED_SAMPLE)
            .cloned()
            .collect(),
        dropped_total: dropped.len(),
    })
}

/// Record a protected array's dropped-entry identities on a `_meta` object,
/// merging with whatever the top-level pass already put under the same keys.
/// `dropped_count` counts every dropped entry once any of them lacked an
/// identifier, matching the top-level protected path.
fn merge_dropped_identities(meta: &mut serde_json::Value, named: &[String], dropped_total: usize) {
    if dropped_total == 0 {
        return;
    }
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    if !named.is_empty() {
        let slot = obj
            .entry("dropped_files")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(list) = slot {
            list.extend(named.iter().map(|s| serde_json::Value::String(s.clone())));
        }
    }
    if named.len() < dropped_total {
        let prev = obj
            .get("dropped_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        obj.insert(
            "dropped_count".to_string(),
            serde_json::json!(prev + dropped_total as u64),
        );
    }
}

const TRUNCATION_MARKER: &str = "\n…[truncated by MCP budget]";

/// Serialized cost of one char inside a JSON string, matching serde_json's
/// escaping: two-char escapes for the named controls, `\u00xx` for the rest
/// below 0x20, raw UTF-8 otherwise.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

fn escaped_len(s: &str) -> usize {
    s.chars().map(escaped_char_len).sum()
}

/// Shrink content with a visible marker. If sibling fields already exceed the
/// budget, leave content intact for a later pass after nested-array trimming.
fn shrink_content_field(item: &mut serde_json::Value, budget_chars: usize) {
    let serde_json::Value::Object(map) = item else {
        return;
    };
    let content = match map.get_mut("content") {
        Some(serde_json::Value::String(s)) => std::mem::take(s),
        _ => return,
    };
    // Measure sibling overhead; a fixed reserve could needlessly truncate nested entries.
    let overhead = serde_json::to_string(&*map).map(|s| s.len()).unwrap_or(0);
    let marker = escaped_len(TRUNCATION_MARKER);
    if budget_chars <= overhead + marker || overhead + escaped_len(&content) <= budget_chars {
        map.insert("content".to_string(), serde_json::Value::String(content));
        return;
    }
    let room = budget_chars - overhead - marker;
    let mut used = 0;
    let mut cut = content.len();
    for (i, c) in content.char_indices() {
        let w = escaped_char_len(c);
        if used + w > room {
            cut = i;
            break;
        }
        used += w;
    }
    let shrunk = if cut == content.len() {
        content
    } else {
        let mut s = content[..cut].to_string();
        s.push_str(TRUNCATION_MARKER);
        s
    };
    map.insert("content".to_string(), serde_json::Value::String(shrunk));
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

    fn truncate_array(items: Vec<Value>, budget_chars: usize) -> Vec<Value> {
        truncate_array_reporting(items, budget_chars).0
    }

    #[test]
    fn cap_passes_through_when_under_budget() {
        let v = json!([{"name": "a"}, {"name": "b"}]);
        let out = cap_to_budget_with(v.clone(), "test", MCP_BUDGET_CHARS);
        assert_eq!(out, v);
    }

    #[test]
    fn bundle_token_budget_is_monotonic_by_repo_size() {
        // Avoid process-global environment writes in parallel tests.
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
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let out = cap_to_budget_with(Value::Array(items), "find_symbol", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("wrapped as object");
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["kind"], json!("find_symbol"));
        assert_eq!(meta["total_results"], json!(50));
        let returned = meta["returned"].as_u64().unwrap() as usize;
        assert!(returned > 0 && returned < 50, "got {returned}");
        assert_eq!(obj["results"].as_array().unwrap().len(), returned);
        let hint = meta["hint"].as_str().unwrap();
        assert!(!hint.contains("offset"), "{hint}");
    }

    #[test]
    fn cap_search_envelope_hint_keeps_pagination_advice() {
        let results: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let v = json!({
            "results": results,
            "confidence": "low",
            "margin_pct": 3,
            "cliff_at": 50,
        });
        let out = cap_to_budget_with(v, "search", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["kind"], json!("search"));
        assert_eq!(meta["field"], json!("results"));
        assert_eq!(meta["total_results"], json!(50));
        let hint = meta["hint"].as_str().unwrap();
        assert!(hint.contains("offset"), "{hint}");
        assert_eq!(obj["confidence"], json!("low"), "scalar fields survive");
    }

    #[test]
    fn cap_trims_largest_array_field_in_object() {
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
    fn cap_trims_every_oversized_array_not_just_the_largest() {
        let big = |n: usize, prefix: &str| -> Vec<Value> {
            (0..n)
                .map(|i| json!(format!("{prefix}/{i}/{}", fat_string(200))))
                .collect()
        };
        let v = json!({
            "session_id": "s",
            "new_files": big(300, "src/new"),
            "removed_files": big(300, "src/old"),
            "pass": true,
        });
        let out = cap_to_budget_with(v, "session_end", MCP_BUDGET_CHARS);
        let serialized = serde_json::to_string(&out).unwrap();
        // Allow fixed JSON envelope overhead, not overshoot that scales with untrimmed arrays.
        assert!(
            serialized.len() < MCP_BUDGET_CHARS + 1024,
            "capped response must land at the budget, got {} chars",
            serialized.len()
        );
        let obj = out.as_object().expect("still an object");
        assert!(obj["new_files"].as_array().unwrap().len() < 300);
        assert!(obj["removed_files"].as_array().unwrap().len() < 300);
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        let also: Vec<&str> = meta["also_truncated_fields"]
            .as_array()
            .expect("also_truncated_fields")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(also.len(), 1, "got {also:?}");
        let headline = meta["field"].as_str().unwrap();
        let other = if headline == "new_files" {
            "removed_files"
        } else {
            "new_files"
        };
        assert!(
            also[0].starts_with(other) && also[0].ends_with("/300)"),
            "expected `{other} (kept/300)`, got {also:?}"
        );
    }

    #[test]
    fn cap_leaves_protected_files_alone_after_another_array_absorbed_a_trim() {
        let files: Vec<Value> = (0..40)
            .map(|i| json!({"file": format!("src/f{i}.rs"), "blob": fat_string(1000)}))
            .collect();
        let notes: Vec<Value> = (0..20).map(|i| json!(format!("note {i}"))).collect();
        let v = json!({ "files": files, "summary_notes": notes, "max_score": 0.9 });
        let out = cap_to_budget_with(v, "assess_risk_diff", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        assert_eq!(obj["files"].as_array().unwrap().len(), 40);
        assert!(
            obj["_meta"].get("also_truncated_fields").is_none(),
            "no second field should be trimmed: {:?}",
            obj["_meta"]
        );
    }

    #[test]
    fn cap_single_oversized_array_reports_no_also_truncated_fields() {
        let related: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let v = json!({ "target_description": "t", "related": related });
        let out = cap_to_budget_with(v, "export_context", MCP_BUDGET_CHARS);
        let meta = &out.as_object().unwrap()["_meta"];
        assert_eq!(meta["field"], json!("related"));
        assert!(meta.get("also_truncated_fields").is_none());
    }

    #[test]
    fn cap_trims_nested_array_inside_the_surviving_element() {
        let cycle: Vec<Value> = (0..4200)
            .map(|i| json!(format!("src/module{i}/handler.rs")))
            .collect();
        let v = json!({
            "files": [{
                "file_path": "src/hot.rs",
                "score": 0.87,
                "cycle_files": cycle,
            }],
            "_legend": { "T": "no direct test file" },
        });
        let out = cap_to_budget_with(v, "assess_risk_batch", MCP_BUDGET_CHARS);
        let obj = out.as_object().expect("still an object");
        let files = obj["files"].as_array().expect("files array");
        assert_eq!(files.len(), 1, "the single risk entry must survive");
        let kept = files[0]["cycle_files"]
            .as_array()
            .expect("cycle_files array")
            .len();
        assert!(kept > 0 && kept < 4200, "nested array not trimmed: {kept}");
        assert!(
            serde_json::to_string(&obj["files"]).unwrap().len() <= MCP_BUDGET_CHARS,
            "trimmed payload must fit the budget"
        );
        // `_meta` is appended after the payload budget check.
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() < MCP_BUDGET_CHARS + 1024,
            "capped response must land at the budget, got {} chars",
            serialized.len()
        );
        let also: Vec<&str> = obj["_meta"]["also_truncated_fields"]
            .as_array()
            .expect("also_truncated_fields")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(also, vec![format!("files[0].cycle_files ({kept}/4200)")]);
        let mut paths = Vec::new();
        collect_referenced_paths(&out, &mut paths);
        assert!(paths.contains(&"src/hot.rs".to_string()));
        assert_eq!(
            paths.iter().filter(|p| p.starts_with("src/module")).count(),
            kept
        );
    }

    #[test]
    fn nested_trim_prefers_an_unprotected_array_over_a_protected_one() {
        let mut item = json!({
            "severity": "high",
            "files": (0..200).map(|i| json!(format!("src/patch{i}.rs"))).collect::<Vec<_>>(),
            "cycle_files": (0..400).map(|i| json!(format!("src/cycle{i}.rs"))).collect::<Vec<_>>(),
        });
        let trim = shrink_largest_nested_array(&mut item, 3_000).expect("nested trim");
        assert_eq!(trim.field, "cycle_files");
        assert_eq!(
            item["files"].as_array().unwrap().len(),
            200,
            "protected nested array must survive while an unprotected one exists"
        );
        assert!(
            trim.dropped_named.is_empty(),
            "unprotected: no drop identity"
        );
        assert_eq!(trim.dropped_total, 0);
    }

    #[test]
    fn nested_trim_names_dropped_entries_of_a_protected_array() {
        let objection = json!({
            "severity": "high",
            "files": (0..900).map(|i| json!(format!("src/area{i}/handler.rs"))).collect::<Vec<_>>(),
        });
        let v = json!({ "objections": [objection], "pass": false });
        let out = cap_to_budget_with(v, "review_rehearsal", 4_000);
        let obj = out.as_object().expect("still an object");
        let kept = obj["objections"][0]["files"]
            .as_array()
            .expect("files array")
            .len();
        assert!(kept > 0 && kept < 900, "nested protected array not trimmed");
        let meta = &obj["_meta"];
        let also: Vec<&str> = meta["also_truncated_fields"]
            .as_array()
            .expect("also_truncated_fields")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(also, vec![format!("objections[0].files ({kept}/900)")]);
        let dropped = meta["dropped_files"].as_array().expect("dropped_files");
        assert_eq!(
            dropped.len(),
            NESTED_DROPPED_SAMPLE,
            "identity list sampled"
        );
        assert_eq!(dropped[0], json!(format!("src/area{kept}/handler.rs")));
        assert_eq!(
            meta["dropped_count"],
            json!(900 - kept),
            "every dropped entry counted, not just the named sample"
        );
    }

    #[test]
    fn content_shrink_absorbs_the_cut_before_any_nested_trim() {
        let item = json!({
            "file_path": "src/big.rs",
            "content": fat_string(5_000),
            "cycle_files": (0..10).map(|i| json!(format!("src/{}{i}.rs", fat_string(90)))).collect::<Vec<_>>(),
        });
        let (kept, nested) = truncate_array_reporting(vec![item], 4_000);
        assert!(nested.is_none(), "nested trim fired unnecessarily");
        assert_eq!(
            kept[0]["cycle_files"].as_array().unwrap().len(),
            10,
            "all nested entries must survive a content-absorbable cut"
        );
        let len = serde_json::to_string(&kept[0]).unwrap().len();
        assert!(len <= 4_000, "shrunk element still over budget: {len}");
        assert!(
            kept[0]["content"]
                .as_str()
                .unwrap()
                .contains("[truncated by MCP budget]")
        );
    }

    #[test]
    fn nested_trim_leaves_a_small_content_field_intact() {
        let item = json!({
            "file_path": "src/a.rs",
            "content": "fn main() {}",
            "cycle_files": (0..4000).map(|i| json!(format!("src/mod{i}/lib.rs"))).collect::<Vec<_>>(),
        });
        let (kept, nested) = truncate_array_reporting(vec![item], 8_000);
        assert_eq!(nested.expect("nested trim").field, "cycle_files");
        assert_eq!(
            kept[0]["content"],
            json!("fn main() {}"),
            "a content field that fits must survive untouched"
        );
        let len = serde_json::to_string(&kept[0]).unwrap().len();
        assert!(len <= 8_000, "element still over budget: {len}");
    }

    #[test]
    fn nested_trim_charges_the_array_brackets() {
        let entries: Vec<Value> = ["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd", "eeeeeeee"]
            .iter()
            .map(|s| json!(s))
            .collect();
        for budget in 30..=120 {
            let mut item = json!({ "id": "x", "list": entries });
            shrink_largest_nested_array(&mut item, budget);
            let len = serde_json::to_string(&item).unwrap().len();
            assert!(len <= budget, "budget {budget}: element is {len} chars");
        }
    }

    #[test]
    fn cap_leaves_under_budget_nested_arrays_byte_identical() {
        let v = json!({
            "files": [{
                "file_path": "src/a.rs",
                "cycle_files": ["src/b.rs", "src/c.rs"],
                "notes": ["in import cycle of 3 files"],
            }],
            "_legend": {},
        });
        let out = cap_to_budget_with(v.clone(), "assess_risk_batch", MCP_BUDGET_CHARS);
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            serde_json::to_string(&v).unwrap()
        );
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
        let kept = truncate_array(items, 600);
        assert!(
            (4..=6).contains(&kept.len()),
            "expected 4-6, got {}",
            kept.len()
        );
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
        assert!(!obj.contains_key("_meta"));
    }

    #[test]
    fn render_passes_object_through_unchanged() {
        let r: Result<Value> = Ok(json!({"file_path": "a.rs", "imports": ["b.rs"]}));
        let result = render_with_kind(r, "list_dependencies");
        let value = result.structured_content.expect("structured content");
        let obj = value.as_object().expect("object preserved");
        assert_eq!(obj["file_path"], json!("a.rs"));
        assert!(!obj.contains_key("results"));
    }

    #[test]
    fn render_wraps_empty_array() {
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
        let items: Vec<Value> = (0..50)
            .map(|i| json!({"i": i, "blob": fat_string(1000)}))
            .collect();
        let r: Result<Vec<Value>> = Ok(items);
        let result = render_with_kind(r, "find_similar");
        let value = result.structured_content.expect("structured content");
        let obj = value
            .as_object()
            .expect("structuredContent must be an object");
        assert!(obj.contains_key("results"));
        let meta = &obj["_meta"];
        assert_eq!(meta["truncated"], json!(true));
        assert_eq!(meta["kind"], json!("find_similar"));
        assert_eq!(meta["total_results"], json!(50));
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
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]);
    }

    #[test]
    fn collect_referenced_paths_covers_review_rehearsal_file_lists() {
        let v = json!({
            "files": ["src/a.rs", "src/b.rs"],
            "objections": [
                { "category": "test-gap", "files": ["src/c.rs"] },
                { "category": "risk", "files": ["src/d.rs"] }
            ]
        });
        let mut paths = Vec::new();
        collect_referenced_paths(&v, &mut paths);
        paths.sort();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]);
    }

    #[test]
    fn collect_referenced_paths_reaches_nested_cycle_arrays() {
        let v = json!({
            "new_cycles": [["a.rs", "b.rs"], ["c.rs"]],
            "resolved_cycles": [["d.rs"]],
            "imported_by": ["e.rs"],
            "primary": ["f_test.rs"]
        });
        let mut paths = Vec::new();
        collect_referenced_paths(&v, &mut paths);
        paths.sort();
        assert_eq!(
            paths,
            vec!["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f_test.rs"]
        );
    }

    #[test]
    fn collect_referenced_paths_does_not_scoop_object_strings_under_a_path_key() {
        let v = json!({
            "files": [
                { "file": "a.rs", "notes": ["test gap: no test found"], "category": "hotspot" }
            ]
        });
        let mut paths = Vec::new();
        collect_referenced_paths(&v, &mut paths);
        assert_eq!(paths, vec!["a.rs"]);
    }

    #[test]
    fn collect_referenced_paths_files_key_tolerates_object_arrays() {
        let v = json!({
            "files": [
                { "file": "src/a.rs", "score": 0.5 },
                { "path": "src/b.rs", "role": "owned" }
            ]
        });
        let mut paths = Vec::new();
        collect_referenced_paths(&v, &mut paths);
        paths.sort();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn merge_stale_meta_preserves_existing_meta() {
        let mut v = json!({ "results": [], "_meta": { "truncated": true } });
        merge_stale_meta(&mut v, &["src/a.rs".to_string()]);
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

        write("src/same.rs", b"fn a() {}");
        index("src/same.rs", b"fn a() {}");
        write("src/changed.rs", b"fn b() {} // edited");
        index("src/changed.rs", b"fn b() {}");
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
    fn coverage_annotates_only_empty_results_of_the_listed_tools() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        let db = Database::open(&codesage_dir.join("index.db")).unwrap();
        for (rel, lang) in [
            ("src/a.rs", Language::Rust),
            ("src/b.rs", Language::Rust),
            ("app/c.php", Language::Php),
        ] {
            db.upsert_file(&codesage_protocol::FileInfo {
                path: rel.to_string(),
                language: lang,
                content_hash: codesage_parser::discover::content_hash(b"x"),
            })
            .unwrap();
        }
        drop(db);

        let server = CodeSageServer::with_state(Arc::new(CodeSageServerState::new()));
        let project = root.to_str().unwrap();

        let coverage_of = |kind: &str, payload: serde_json::Value| {
            let rendered = render_with_kind(Ok(payload), kind);
            let annotated = server.annotate_coverage(project, kind, rendered);
            annotated
                .structured_content
                .and_then(|s| s.get("_meta").and_then(|m| m.get("coverage")).cloned())
        };

        let cov = coverage_of("search", json!({ "results": [] }))
            .expect("empty search should carry a coverage hint");
        assert_eq!(cov["indexed_files"], json!(3));
        assert_eq!(cov["indexed_by_language"]["rust"], json!(2));
        assert_eq!(cov["indexed_by_language"]["php"], json!(1));

        assert!(
            coverage_of("find_symbol", json!({ "results": [] })).is_some(),
            "find_symbol is in the annotated set"
        );
        assert!(
            coverage_of("find_references", json!({ "results": [] })).is_some(),
            "find_references is in the annotated set"
        );
        assert!(
            coverage_of("find_similar", json!({ "results": [] })).is_some(),
            "find_similar is in the annotated set"
        );
        assert!(
            coverage_of(
                "search",
                json!({ "results": [{ "file_path": "src/a.rs" }] })
            )
            .is_none(),
            "a non-empty result must not be annotated"
        );
        assert!(
            coverage_of("impact_analysis", json!({ "results": [] })).is_none(),
            "impact_analysis is deliberately outside the annotated set"
        );
        assert!(
            coverage_of("search", json!({ "found": false })).is_none(),
            "a payload without `results` must not be treated as empty"
        );

        let sem = coverage_of("search", json!({ "results": [] })).unwrap();
        assert_eq!(sem["semantically_indexed_files"], json!(0));
        let structural = coverage_of("find_symbol", json!({ "results": [] })).unwrap();
        assert!(structural.get("semantically_indexed_files").is_none());
    }

    #[test]
    fn coverage_speaks_up_loudest_when_nothing_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        drop(Database::open(&codesage_dir.join("index.db")).unwrap());

        let server = CodeSageServer::with_state(Arc::new(CodeSageServerState::new()));
        let project = root.to_str().unwrap();
        let rendered = render_with_kind(Ok(json!({ "results": [] })), "find_symbol");
        let annotated = server.annotate_coverage(project, "find_symbol", rendered);

        let cov = annotated
            .structured_content
            .as_ref()
            .and_then(|s| s.get("_meta").and_then(|m| m.get("coverage")))
            .expect("an empty index must still be annotated");
        assert_eq!(cov["indexed_files"], json!(0));
        let banner = annotated.content.first().and_then(|c| c.as_text()).unwrap();
        assert!(
            banner.text.contains("nothing is indexed"),
            "got {:?}",
            banner.text
        );
    }

    #[test]
    fn clamps_annotate_requested_vs_applied_and_spare_the_common_case() {
        let plain = render_with_kind(Ok(json!({ "results": [] })), "search");
        let untouched = annotate_clamps(plain, &[]);
        assert!(
            untouched
                .structured_content
                .as_ref()
                .unwrap()
                .get("_meta")
                .is_none(),
            "an unclamped response must not grow a `_meta` envelope"
        );

        let capped = render_with_kind(Ok(json!({ "results": [] })), "search");
        let annotated = annotate_clamps(
            capped,
            &[ClampNote {
                param: "limit",
                requested: json!(10_000),
                applied: json!(100),
            }],
        );
        let clamps = annotated.structured_content.as_ref().unwrap()["_meta"]["clamps"].clone();
        assert_eq!(
            clamps,
            json!([{ "param": "limit", "requested": 10_000, "applied": 100 }])
        );

        let failed: CallToolResult =
            render_with_kind::<serde_json::Value>(Err(anyhow::anyhow!("boom")), "search");
        assert_eq!(failed.is_error, Some(true));
        let still_failed = annotate_clamps(
            failed,
            &[ClampNote {
                param: "limit",
                requested: json!(10_000),
                applied: json!(100),
            }],
        );
        assert_eq!(still_failed.is_error, Some(true));
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

    #[test]
    fn test_override_marks_success_and_spares_the_common_case() {
        let marked =
            annotate_test_override(CallToolResult::structured(json!({"results": []})), true);
        assert_eq!(
            marked.structured_content.as_ref().unwrap()["_meta"]["test_override"],
            json!(true)
        );

        let merged = annotate_test_override(
            CallToolResult::structured(json!({"results": [], "_meta": {"truncated": true}})),
            true,
        );
        let meta = &merged.structured_content.as_ref().unwrap()["_meta"];
        assert_eq!(meta["test_override"], json!(true));
        assert_eq!(meta["truncated"], json!(true));

        let unmarked =
            annotate_test_override(CallToolResult::structured(json!({"results": []})), false);
        assert!(
            unmarked
                .structured_content
                .as_ref()
                .unwrap()
                .get("_meta")
                .is_none(),
            "inert override must leave the response byte-identical"
        );

        let still_failed = annotate_test_override(
            CallToolResult::error(vec![ContentBlock::text("boom")]),
            true,
        );
        assert_eq!(still_failed.is_error, Some(true));
    }
}
