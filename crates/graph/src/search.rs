use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use anyhow::{Context, Result};
use codesage_parser::detect::detect_language;
use codesage_parser::discover::{TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set};
use codesage_protocol::{
    Language, SearchConfidence, SearchRequest, SearchResult, SearchResults, Symbol, SymbolSummary,
};
use codesage_storage::{Database, RawSearchRow, SemanticValidityToken, embedding_to_bytes};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;

/// Preserve rows with unknown language tags (version skew or corruption).
/// Warn once and use a placeholder; a handler panic can leave MCP clients waiting.
pub(crate) fn parse_db_language(s: &str) -> Language {
    Language::parse(s).unwrap_or_else(|| {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                language = %s,
                "unknown language string in index (version skew or corruption); \
                 results kept with a placeholder language label — reindex to fix"
            );
        });
        Language::Rust
    })
}

fn l2_to_score(distance: f32) -> f32 {
    // Negative similarity would turn multiplicative penalties into promotions.
    (1.0 - distance * distance / 2.0).max(0.0)
}

const RERANK_OVERFETCH: usize = 5;

/// Bound deep-pagination retrieval/reranking; large explicit limits may exceed
/// this cap. Deeper pages can return fewer or no results.
const MAX_SEMANTIC_FETCH: usize = 500;

/// Extra KNN candidates to fetch before applying path globs. Path filters are
/// applied after bounded sqlite-vec retrieval, so recall is approximate when a
/// glob excludes many of the nearest neighbors, but query cost stays bounded
/// independently of total chunk count.
const PATH_FILTER_KNN_OVERFETCH: usize = 10;
const MAX_PATH_FILTER_KNN_FETCH: usize = 5_000;

/// RRF constant. Standard value from the original paper; larger values
/// damp the influence of absolute rank position, smaller values amplify it.
const RRF_K: f64 = 60.0;

/// Use a synthetic score span below this threshold so flat +0.1 boosts do not
/// overwhelm fusion when semantic scores are tied or nearly tied.
const MIN_FUSED_RESCALE_SPAN: f32 = 0.05;

/// A present token below 1% document frequency qualifies as rare.
const RARE_TOKEN_DF_THRESHOLD: f64 = 0.01;

/// Minimum token length for the length-based rare check. Short tokens
/// (`fd`, `pt`, `if`) match too broadly regardless of doc frequency.
const RARE_TOKEN_MIN_LEN: usize = 8;

/// Gate BM25 on backticks, extension globs, ::, dotted identifiers, or code-shaped
/// tokens of at least 8 bytes present in fewer than 1% of indexed chunks.
/// CODESAGE_QUALIFIED_GROUPS=1 also admits backslash-qualified names.
pub(crate) fn query_has_rare_literal(db: &Database, query: &str) -> Result<bool> {
    query_has_rare_literal_with_groups(db, query, qualified_groups_enabled())
}

fn qualified_groups_enabled() -> bool {
    std::env::var("CODESAGE_QUALIFIED_GROUPS").is_ok_and(|value| value == "1")
}

fn query_has_rare_literal_with_groups(db: &Database, query: &str, groups: bool) -> Result<bool> {
    if query.contains("::") || query.contains('`') || query.contains("*.") {
        return Ok(true);
    }
    if if groups {
        !extract_qualified_name_groups(query).is_empty()
    } else {
        !extract_dotted_identifier_tokens(query).is_empty()
    } {
        return Ok(true);
    }
    for tok in query
        .split(|c: char| c == '|' || c.is_whitespace() || c == ',' || c == ';')
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
    {
        if tok.len() < RARE_TOKEN_MIN_LEN {
            continue;
        }
        if !token_looks_code_shaped(tok) {
            // Corpus rarity alone does not make an English word an identifier.
            continue;
        }
        let (doc, total) = db.token_doc_frequency(tok)?;
        if total == 0 {
            continue;
        }
        let df = doc as f64 / total as f64;
        // An absent token cannot contribute a BM25 match.
        if doc > 0 && df < RARE_TOKEN_DF_THRESHOLD {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Exclude ordinary English words from the rare-token branch.
fn token_looks_code_shaped(tok: &str) -> bool {
    tok.contains('_')
        || tok.chars().any(|c| c.is_ascii_uppercase())
        || tok.chars().any(|c| c.is_ascii_digit())
}

/// Extract every `identifier.identifier` pair from the query where both
/// sides are ASCII identifiers of length ≥3. Returns tokens flat, not
/// pairs — the caller feeds them into the FTS MATCH disjunction. Skips
/// sentence-punctuation patterns like `e.g.` (1-char left side) and
/// `i.e.` (1-char right side).
fn extract_dotted_identifier_tokens(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let mut out = Vec::new();
    let is_id_start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let is_id_body = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        if !is_id_start(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_id_body(bytes[i]) {
            i += 1;
        }
        let first = &query[start..i];
        if first.len() < 3 || i >= bytes.len() || bytes[i] != b'.' {
            continue;
        }
        let after_dot = i + 1;
        if after_dot >= bytes.len() || !is_id_start(bytes[after_dot]) {
            continue;
        }
        let second_start = after_dot;
        i = after_dot;
        while i < bytes.len() && is_id_body(bytes[i]) {
            i += 1;
        }
        let second = &query[second_start..i];
        if second.len() < 3 {
            continue;
        }
        out.push(first);
        out.push(second);
    }
    out
}

/// Split :: and backslash namespaces. Keep dotted receiver/member pairs separate:
/// their OR terms improved the nest benchmark without broad namespace prefixes.
fn extract_qualified_name_groups_legacy(query: &str) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    for raw in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        if !raw.contains("::") && !raw.contains('\\') {
            continue;
        }
        let parts: Vec<String> = raw
            .split([':', '\\'])
            .filter(|p| p.len() >= 2 && p.chars().all(|c| c.is_alphanumeric() || c == '_'))
            .map(str::to_string)
            .collect();
        if parts.len() >= 2 {
            groups.push(parts);
        }
    }
    groups
}

/// Build an FTS5 MATCH expression from a user query. Emits a disjunction
/// of quoted terms so code tokens like `doc_cfg` and `ModuleRef::create`
/// survive FTS5's reserved-character parsing without raising syntax errors
/// at query time. Empty when no usable tokens are extracted.
fn build_fts_match_query_legacy(query: &str) -> String {
    use std::collections::HashSet;
    // Match unicode61 token boundaries and exclude English glue from the OR query.
    // Dotted-pair context admits lowercase components as code identifiers.
    let is_sep = |c: char| !c.is_alphanumeric() && c != '_';
    let mut seen: HashSet<String> = HashSet::new();
    let mut tokens: Vec<String> = Vec::new();

    // Namespace prefixes swamp selective tails. Full-name phrases regressed
    // semble C++ NDCG@10 by 0.006, so only selective tails replace prefixes.
    let mut suppressed: HashSet<String> = HashSet::new();
    for parts in extract_qualified_name_groups_legacy(query) {
        // Lowercase tails such as create must still pass the code-shape filter.
        let tail_is_selective = parts.last().is_some_and(|t| token_looks_code_shaped(t));
        if tail_is_selective
            && let Some(tail) = parts.last()
            && seen.insert(tail.to_lowercase())
        {
            tokens.push(format!("\"{tail}\""));
        }
        // Keep prefixes when the tail is not selective, or MATCH could become empty.
        if tail_is_selective {
            for prefix in &parts[..parts.len() - 1] {
                suppressed.insert(prefix.to_lowercase());
            }
        }
    }

    for tok in extract_dotted_identifier_tokens(query) {
        let key = tok.to_lowercase();
        if seen.insert(key) {
            tokens.push(format!("\"{tok}\""));
        }
    }
    for raw in query.split(is_sep) {
        if raw.len() < 2 {
            continue;
        }
        if !token_looks_code_shaped(raw) {
            continue;
        }
        // unicode61 matches case-insensitively.
        let key = raw.to_lowercase();
        if suppressed.contains(&key) {
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        tokens.push(format!("\"{raw}\""));
    }
    tokens.join(" OR ")
}

struct QualifiedName<'a> {
    start: usize,
    end: usize,
    parts: Vec<&'a str>,
}

fn identifier_end(query: &str, start: usize) -> usize {
    let mut chars = query[start..].char_indices();
    if !chars
        .next()
        .is_some_and(|(_, c)| c.is_alphabetic() || c == '_')
    {
        return start;
    }
    chars
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map_or(query.len(), |(offset, _)| start + offset)
}

fn extract_qualified_name_groups(query: &str) -> Vec<QualifiedName<'_>> {
    let mut groups = Vec::new();
    let mut cursor = 0;
    while cursor < query.len() {
        let start = cursor;
        let mut end = identifier_end(query, start);
        if end == start {
            cursor += query[cursor..].chars().next().unwrap().len_utf8();
            continue;
        }
        let mut parts = vec![&query[start..end]];
        loop {
            let suffix = &query[end..];
            let separator_len = if suffix.starts_with("::") {
                2
            } else if suffix.starts_with(['\\', '.']) {
                1
            } else {
                break;
            };
            let next_start = end + separator_len;
            let next_end = identifier_end(query, next_start);
            if next_end == next_start {
                break;
            }
            let next = &query[next_start..next_end];
            // Keep sentence abbreviations out of the dotted-identifier gate.
            if suffix.starts_with('.')
                && (parts.last().unwrap().chars().count() < 3 || next.chars().count() < 3)
            {
                break;
            }
            parts.push(next);
            end = next_end;
        }
        if parts.len() >= 2 {
            groups.push(QualifiedName { start, end, parts });
        }
        cursor = end;
    }
    groups
}

/// Build an FTS5 MATCH expression from a user query. Emits a disjunction
/// of quoted terms, or experimental qualified-name conjunctions when opted in,
/// so `doc_cfg` and `ModuleRef::create`
/// survive FTS5's reserved-character parsing without raising syntax errors
/// at query time. Empty when no usable tokens are extracted.
fn build_fts_match_query(query: &str) -> String {
    if qualified_groups_enabled() {
        build_fts_match_query_mode(query, false)
    } else {
        build_fts_match_query_legacy(query)
    }
}

fn build_fts_match_query_mode(query: &str, fallback: bool) -> String {
    // Apply the legacy token/code-shape rules. Qualified components stay in
    // conjunctions, so lowercase members do not broaden the OR expression.
    let is_sep = |c: char| !c.is_alphanumeric() && c != '_';
    let mut seen: HashSet<String> = HashSet::new();
    let mut tokens: Vec<String> = Vec::new();

    let groups = extract_qualified_name_groups(query);
    let mut outside_groups = Vec::new();
    let mut cursor = 0;
    for group in groups {
        if fallback {
            let dotted = query[group.start..group.end].contains('.');
            let parts = if !dotted
                && group
                    .parts
                    .last()
                    .is_some_and(|part| token_looks_code_shaped(part))
            {
                &group.parts[group.parts.len() - 1..]
            } else {
                &group.parts[..]
            };
            for part in parts
                .iter()
                .filter(|part| dotted || token_looks_code_shaped(part))
            {
                if seen.insert(part.to_lowercase()) {
                    tokens.push(format!("\"{part}\""));
                }
            }
        } else {
            let terms = group.parts.iter().map(|part| format!("\"{part}\""));
            let expression = format!("({})", terms.collect::<Vec<_>>().join(" AND "));
            if seen.insert(expression.to_lowercase()) {
                tokens.push(expression);
            }
        }
        outside_groups.push(&query[cursor..group.start]);
        cursor = group.end;
    }
    outside_groups.push(&query[cursor..]);
    for raw in outside_groups
        .iter()
        .flat_map(|fragment| fragment.split(is_sep))
    {
        if raw.len() < 2 {
            continue;
        }
        if !token_looks_code_shaped(raw) {
            continue;
        }
        let key = raw.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        tokens.push(format!("\"{raw}\""));
    }
    tokens.join(" OR ")
}

/// Weight applied to BM25 contributions in the gated hybrid RRF merge.
/// Give selective lexical hits enough weight to beat semantic-only candidates.
const BM25_WEIGHT: f64 = 2.0;

/// Reciprocal Rank Fusion over two ranked lists. Each list contributes
/// `weight / (k + rank)` to the merged score for each document. The
/// BM25 list gets `BM25_WEIGHT`; semantic gets 1.0. De-duplicates by
/// (file_path, start_line, end_line) since chunk ids differ between the
/// vec0 and FTS5 rankings but the underlying text region does not.
fn rrf_merge(
    semantic: Vec<RawSearchRow>,
    bm25: Vec<RawSearchRow>,
    limit: usize,
) -> Vec<RawSearchRow> {
    use std::collections::HashMap;
    // Keep fused scores on the scale downstream boosts were tuned for.
    let (sem_min, sem_max) = semantic
        .iter()
        .map(|r| l2_to_score(r.distance))
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(s), hi.max(s))
        });
    let (sem_min, sem_max) = if sem_min.is_finite() && sem_max.is_finite() {
        (sem_min, sem_max)
    } else {
        (0.0, 1.0)
    };
    // Near-tied scores need a synthetic span; an epsilon-only guard would still
    // let flat +0.1 boosts overwhelm the entire fused ranking.
    let (sem_min, sem_max) = if sem_max - sem_min < MIN_FUSED_RESCALE_SPAN {
        ((sem_max - 0.2).clamp(0.0, 1.0), sem_max.clamp(0.0, 1.0))
    } else {
        (sem_min, sem_max)
    };
    let mut scores: HashMap<(String, u32, u32), (f64, RawSearchRow)> = HashMap::new();
    for (rank, row) in semantic.into_iter().enumerate() {
        let contrib = 1.0 / (RRF_K + rank as f64 + 1.0);
        let key = (row.file_path.clone(), row.start_line, row.end_line);
        scores
            .entry(key)
            .and_modify(|(s, _)| *s += contrib)
            .or_insert((contrib, row));
    }
    for (rank, row) in bm25.into_iter().enumerate() {
        let contrib = BM25_WEIGHT / (RRF_K + rank as f64 + 1.0);
        let key = (row.file_path.clone(), row.start_line, row.end_line);
        scores
            .entry(key)
            .and_modify(|(s, _)| *s += contrib)
            .or_insert((contrib, row));
    }
    let mut ranked: Vec<(f64, RawSearchRow)> = scores.into_values().collect();
    // Break reachable exact f64 ties before truncation; HashMap order is random.
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.file_path.cmp(&b.1.file_path))
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
    });
    ranked.truncate(limit);
    // Raw RRF spans are too narrow for additive boosts. Rescale onto the semantic
    // span, then invert l2_to_score so its downstream read preserves that scale.
    let fused_hi = ranked.first().map(|(s, _)| *s).unwrap_or(0.0);
    let fused_lo = ranked.last().map(|(s, _)| *s).unwrap_or(0.0);
    let fused_range = fused_hi - fused_lo;
    ranked
        .into_iter()
        .map(|(score, mut row)| {
            let rescaled = if fused_range > f64::EPSILON {
                sem_min + ((score - fused_lo) / fused_range) as f32 * (sem_max - sem_min)
            } else {
                sem_max
            };
            row.distance = (2.0 * (1.0 - rescaled.clamp(0.0, 1.0))).sqrt();
            row
        })
        .collect()
}

/// Smallest adjacent relative score drop that counts as a relevance cliff.
pub const MIN_CLIFF_DROP: f32 = 0.20;

/// Where a ranked page's scores fall off, and how sharply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliffCut {
    /// Rows to keep: the index just past the largest drop when `confidence`
    /// is `High`, the full length otherwise.
    pub cut: usize,
    /// Largest adjacent relative drop, rounded to whole percent (0 when the
    /// page has fewer than two comparable rows).
    pub drop_pct: u8,
    pub confidence: SearchConfidence,
}

/// Find the largest adjacent relative drop in descending non-negative scores.
/// Skip non-finite pairs and non-positive leaders; a drop to zero is 100%.
/// Equal scores never split. Confidence uses the rounded disclosed percentage.
pub fn relevance_cliff(scores: &[f32]) -> CliffCut {
    let mut best_drop = 0.0f32;
    let mut best_cut = scores.len();
    for (i, pair) in scores.windows(2).enumerate() {
        let (hi, lo) = (pair[0], pair[1]);
        if !hi.is_finite() || !lo.is_finite() || hi <= 0.0 {
            continue;
        }
        let drop = ((hi - lo) / hi).min(1.0);
        if drop > best_drop {
            best_drop = drop;
            best_cut = i + 1;
        }
    }
    // best_drop is within [0, 1], so the rounded percentage fits in a u8.
    let drop_pct = (best_drop * 100.0).round() as u8;
    // Gate on the rounded figure the caller sees, so a disclosed
    // `margin_pct` of 20 is never paired with `confidence: low`.
    let min_drop_pct = (MIN_CLIFF_DROP * 100.0).round() as u8;
    if drop_pct >= min_drop_pct {
        CliffCut {
            cut: best_cut,
            drop_pct,
            confidence: SearchConfidence::High,
        }
    } else {
        CliffCut {
            cut: scores.len(),
            drop_pct,
            confidence: SearchConfidence::Low,
        }
    }
}

fn apply_offset_and_limit<T>(rows: &mut Vec<T>, offset: usize, limit: usize) {
    if offset >= rows.len() {
        rows.clear();
    } else if offset > 0 {
        rows.drain(..offset);
    }
    rows.truncate(limit);
}

fn bm25_search_candidates(
    db: &Database,
    match_expr: &str,
    fetch_limit: usize,
    languages: Option<&[&str]>,
    paths: Option<&[&str]>,
) -> Result<Vec<RawSearchRow>> {
    db.search_bm25(match_expr, fetch_limit, languages, paths)
}

fn bm25_candidates_with_fallback(
    db: &Database,
    match_expr: &str,
    query: &str,
    fetch_limit: usize,
    languages: Option<&[&str]>,
    paths: Option<&[&str]>,
) -> Result<Vec<RawSearchRow>> {
    let rows = bm25_search_candidates(db, match_expr, fetch_limit, languages, paths)?;
    if !rows.is_empty() {
        return Ok(rows);
    }
    // Older chunks may carry a method without its owner context.
    // Keep the previous selective lookup when the grouped query has no hits.
    let fallback = build_fts_match_query_mode(query, true);
    if fallback.is_empty() || fallback == match_expr {
        return Ok(rows);
    }
    bm25_search_candidates(db, &fallback, fetch_limit, languages, paths)
}

/// Return one cross-encoder score per candidate. The callback owns locking so
/// callers can keep SQL retrieval outside the reranker lock.
pub type RerankFn<'a> = Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>> + 'a>;

fn semantic_knn_candidates(
    db: &Database,
    embedding_bytes: &[u8],
    fetch: usize,
    languages: Option<&[Language]>,
) -> Result<Vec<RawSearchRow>> {
    if fetch == 0 {
        return Ok(Vec::new());
    }
    match languages {
        None => db.search_knn(embedding_bytes, fetch, None),
        Some(langs) if langs.len() == 1 => {
            db.search_knn(embedding_bytes, fetch, Some(langs[0].as_str()))
        }
        Some(langs) => {
            // sqlite-vec partition keys require one query per language.
            let mut merged: Vec<RawSearchRow> = Vec::new();
            for lang in langs {
                let lang_rows = db.search_knn(embedding_bytes, fetch, Some(lang.as_str()))?;
                merged.extend(lang_rows);
            }
            merged.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(Ordering::Equal)
            });
            merged.truncate(fetch);
            Ok(merged)
        }
    }
}

fn path_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .with_context(|| format!("invalid search path glob {pattern:?}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .context("failed to build search path globset")
}

fn path_filtered_knn_candidates(
    db: &Database,
    embedding_bytes: &[u8],
    semantic_fetch: usize,
    languages: Option<&[Language]>,
    path_patterns: &[String],
) -> Result<Vec<RawSearchRow>> {
    if semantic_fetch == 0 {
        return Ok(Vec::new());
    }
    let globset = path_globset(path_patterns)?;
    let fetch = semantic_fetch
        .saturating_mul(PATH_FILTER_KNN_OVERFETCH)
        .min(MAX_PATH_FILTER_KNN_FETCH);
    let mut rows = semantic_knn_candidates(db, embedding_bytes, fetch, languages)?;
    rows.retain(|row| globset.is_match(row.file_path.as_str()));
    rows.truncate(semantic_fetch);
    Ok(rows)
}

/// [`search_page`] without the envelope: the ranked rows only. Honors
/// `req.adaptive_limit` the same way; callers that need the cliff disclosure
/// use [`search_page`].
pub fn search(
    db: &Database,
    query_embedding: &[f32],
    rerank: Option<RerankFn<'_>>,
    req: &SearchRequest,
) -> Result<Vec<SearchResult>> {
    search_page(db, query_embedding, rerank, req).map(|page| page.results)
}

/// Run the search pipeline and return the page together with its
/// relevance-cliff disclosure (`confidence`, `margin_pct`, `cliff_at`). When
/// `req.adaptive_limit` is set and the cliff rates `High`, the page is cut at
/// the cliff instead of filling `limit` rows.
pub fn search_page(
    db: &Database,
    query_embedding: &[f32],
    rerank: Option<RerankFn<'_>>,
    req: &SearchRequest,
) -> Result<SearchResults> {
    let limit = req.limit.unwrap_or(10);
    let offset = req.offset.unwrap_or(0);

    let known_symbols = extract_known_symbols(db, &req.query)?;
    let has_symbols = !known_symbols.is_empty();
    let has_reranker = rerank.is_some();
    let overfetch = if has_reranker {
        RERANK_OVERFETCH
    } else if has_symbols {
        3
    } else {
        1
    };

    let embedding_bytes = embedding_to_bytes(query_embedding);

    let page_window = limit.saturating_add(offset);
    // Cap the candidate pool so a deep `offset` can't balloon the cross-encoder
    // workload, while still honoring a large explicit `limit`.
    let semantic_fetch = page_window
        .saturating_mul(overfetch)
        .min(limit.max(MAX_SEMANTIC_FETCH));

    let hybrid_gate = match hybrid_mode() {
        HybridMode::Always => true,
        HybridMode::Never => false,
        HybridMode::Gated => query_has_rare_literal(db, &req.query).unwrap_or(false),
    };

    let rows = if let Some(path_patterns) = &req.paths {
        path_filtered_knn_candidates(
            db,
            &embedding_bytes,
            semantic_fetch,
            req.languages.as_deref(),
            path_patterns,
        )?
    } else {
        semantic_knn_candidates(
            db,
            &embedding_bytes,
            semantic_fetch,
            req.languages.as_deref(),
        )?
    };

    // A triggered gate can still yield no BM25 hits; reranking depends on actual fusion.
    let mut fused = false;
    let rows = if hybrid_gate {
        let match_expr = build_fts_match_query(&req.query);
        if match_expr.is_empty() {
            rows
        } else {
            // Filter before fusion or excluded languages can displace valid candidates.
            let bm25_languages: Option<Vec<&str>> = req
                .languages
                .as_ref()
                .map(|ls| ls.iter().map(|l| l.as_str()).collect());
            let bm25_paths: Option<Vec<&str>> = req
                .paths
                .as_ref()
                .map(|p| p.iter().map(|s| s.as_str()).collect());
            let bm25_rows = if qualified_groups_enabled() {
                bm25_candidates_with_fallback(
                    db,
                    &match_expr,
                    &req.query,
                    semantic_fetch,
                    bm25_languages.as_deref(),
                    bm25_paths.as_deref(),
                )
            } else {
                bm25_search_candidates(
                    db,
                    &match_expr,
                    semantic_fetch,
                    bm25_languages.as_deref(),
                    bm25_paths.as_deref(),
                )
            };
            match bm25_rows {
                Ok(bm25_rows) if !bm25_rows.is_empty() => {
                    fused = true;
                    rrf_merge(rows, bm25_rows, semantic_fetch)
                }
                _ => rows,
            }
        }
    } else {
        rows
    };

    let mut rows = rows;
    if let Some(languages) = &req.languages {
        let allowed: HashSet<&str> = languages.iter().map(|lang| lang.as_str()).collect();
        rows.retain(|row| allowed.contains(row.language.as_str()));
    }

    let semantic_results: Vec<SearchResult> = rows
        .into_iter()
        .map(|r| SearchResult {
            file_path: r.file_path,
            language: parse_db_language(&r.language),
            content: r.content,
            start_line: r.start_line,
            end_line: r.end_line,
            score: l2_to_score(r.distance),
            symbols: Vec::new(),
        })
        .collect();

    let mut results = semantic_results;

    if has_symbols {
        apply_symbol_boost(&mut results, &known_symbols);
    }

    if stem_scan_enabled() {
        apply_non_candidate_stem_scan(db, &mut results, &req.query)?;
    }

    annotate_with_symbols(db, &mut results)?;

    if qualified_name_boost_enabled() && has_symbols {
        apply_qualified_name_boost(&mut results, &known_symbols);
    }

    if definition_boost_enabled() {
        apply_definition_boost(&mut results, &req.query);
    }

    // Fused reranking is opt-in; its reduced weight preserves more of BM25's signal.
    // A gated query with no BM25 hits still follows the ordinary reranker path.
    if let Some(mut rerank) = rerank
        && (!fused || fused_rerank_enabled())
    {
        let weight_override = fused.then_some(RERANK_WEIGHT_SHORT_ID);
        apply_reranking(&mut rerank, &req.query, &mut results, weight_override);
    }

    // Apply penalties after blending; before it, their strength shrinks by 1 - w.
    if path_penalty_enabled() {
        apply_path_penalties(&mut results, &req.query);
    }

    if version_demote_enabled() {
        apply_version_demote(&mut results, &req.query);
    }

    // The cross-encoder cannot see filenames, so do not dilute the stem boost in its blend.
    if stem_match_boost_enabled() {
        apply_stem_match_boost(&mut results, &req.query);
    }

    if file_saturation_enabled() {
        apply_file_saturation(&mut results);
    }

    if dir_saturation_enabled() {
        apply_directory_saturation(&mut results);
    }

    // Anchor after saturation so decay cannot undo the lift; first page only.
    apply_mention_anchor(
        &mut results,
        &req.query,
        limit.saturating_mul(overfetch),
        offset,
        mention_anchor_enabled(),
    );

    apply_offset_and_limit(&mut results, offset, limit);

    let scores: Vec<f32> = results.iter().map(|r| r.score).collect();
    let cliff = relevance_cliff(&scores);
    if req.adaptive_limit && cliff.confidence == SearchConfidence::High {
        results.truncate(cliff.cut);
    }
    Ok(SearchResults {
        results,
        confidence: Some(cliff.confidence),
        margin_pct: Some(cliff.drop_pct),
        cliff_at: Some(cliff.cut),
    })
}

fn extract_known_symbols(db: &Database, query: &str) -> Result<Vec<String>> {
    let mut known = Vec::new();
    for token in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if token.len() < 3 || !looks_like_identifier(token) {
            continue;
        }
        if db.symbol_exists(token)? {
            known.push(token.to_lowercase());
        }
    }
    Ok(known)
}

fn looks_like_identifier(s: &str) -> bool {
    let first = match s.chars().next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_alphabetic() && first != '_' {
        return false;
    }
    s.contains('_')
        || s.chars().any(|c| c.is_uppercase())
        || s.chars().all(|c| c.is_alphanumeric() || c == '_') && s.len() >= 4
}

/// Unlike looks_like_identifier, this has no symbol-table check. Require explicit
/// identifier syntax so long lowercase English words do not get SHORT_ID weight.
fn looks_like_short_identifier(s: &str) -> bool {
    let first = match s.chars().next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_alphabetic() && first != '_' {
        return false;
    }
    s.contains('_')
        || s.contains('-')
        || s.contains("::")
        || s.chars().any(|c| c.is_uppercase())
        || s.chars().any(|c| c.is_ascii_digit())
}

fn apply_symbol_boost(results: &mut [SearchResult], known_symbols: &[String]) {
    for result in results.iter_mut() {
        let content_lower = result.content.to_lowercase();
        let mut boost = 0.0f32;
        for sym in known_symbols {
            // Whole tokens prevent test matching latest or log matching catalog.
            if contains_token(&content_lower, sym) {
                boost += 0.1;
            }
        }
        result.score += boost;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

/// Identifier character for token-boundary checks: letters, digits, `_`.
/// A query token only matches when neither neighbor is one of these.
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `needle` occurs in `haystack` with a token boundary on both
/// sides (string edges count as boundaries).
fn contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(idx, _)| {
        let left_ok = haystack[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_token_char(c));
        let right_ok = haystack[idx + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_token_char(c));
        left_ok && right_ok
    })
}

// Boost annotated qualified-name matches once per chunk. Common method names
// require a non-leaf match to avoid treating prose such as default as Mode::default.
// Default-off via CODESAGE_QUALIFIED_NAME_BOOST; annotation must run first.
const QUALIFIED_NAME_BOOST_FACTOR: f32 = 2.0;

// These common words boost only type/module segments, never a leaf method name.
const ANTI_TRIGGER_TOKENS: &[&str] = &[
    "default",
    "new",
    "clone",
    "drop",
    "from",
    "into",
    "as",
    "eq",
    "cmp",
    "hash",
    "partial_eq",
    "partial_cmp",
    "get",
    "set",
    "has",
    "is",
    "len",
    "size",
    "init",
    "build",
    "run",
    "start",
    "stop",
    "open",
    "close",
    "load",
    "save",
    "parse",
    "format",
    "print",
    "read",
    "write",
    "clear",
    "reset",
    "update",
    "delete",
    "remove",
    "next",
    "iter",
    "test",
    "config",
];

fn is_anti_trigger(token: &str) -> bool {
    ANTI_TRIGGER_TOKENS.contains(&token)
}

fn qualified_name_segments(qn: &str) -> Vec<&str> {
    qn.split(['.', ':', '\\'])
        .filter(|s| !s.is_empty())
        .collect()
}

fn qualified_name_matches(token: &str, qn: &str, name: &str) -> bool {
    let segments = qualified_name_segments(qn);
    if segments.is_empty() {
        return !is_anti_trigger(token) && name == token;
    }
    if is_anti_trigger(token) {
        if segments.len() < 2 {
            return false;
        }
        segments[..segments.len() - 1].contains(&token)
    } else {
        segments.contains(&token)
    }
}

fn apply_qualified_name_boost(results: &mut [SearchResult], known_symbols: &[String]) {
    if known_symbols.is_empty() {
        return;
    }
    for result in results.iter_mut() {
        let hit = result.symbols.iter().any(|s| {
            let qn = s.qualified_name.to_lowercase();
            let name = s.name.to_lowercase();
            known_symbols
                .iter()
                .any(|k| qualified_name_matches(k, &qn, &name))
        });
        if hit {
            result.score *= QUALIFIED_NAME_BOOST_FACTOR;
        }
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

fn qualified_name_boost_enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        matches!(
            std::env::var(tuning::QUALIFIED_NAME_BOOST).as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("True") | Ok("yes")
        )
    })
}

// Definition matches receive 3 * max_score, with a 1.5x matching-stem bonus.
// Run before path penalties so test definitions retain their demotion.
const DEFINITION_KEYWORDS: &[&str] = &[
    // Order matters for regex alternation: longest-first so `abstract class`
    // matches before `class`. Same trick for `data class`.
    "abstract class",
    "data class",
    "defmodule", // Elixir
    "function",
    "interface",
    "namespace",
    "package",
    "protocol", // Swift
    // typedef intentionally omitted: C/C++ typedef has the type between the
    // keyword and the symbol name (`typedef unsigned long size_t`), which the
    // namespace-prefix regex can't represent. find_symbol covers it.
    "module",
    "object",
    "record", // C# 9+, Java 16+
    "struct",
    "trait",
    "class",
    "enum",
    "func",
    "type",
    "def",
    "fun", // Kotlin
    "fn",
];

const DEFINITION_BOOST_MULTIPLIER: f32 = 3.0;
const DEFINITION_FILE_STEM_BONUS: f32 = 1.5;
// Embedded symbols get half strength: prose queries may want surrounding context.
const EMBEDDED_SYMBOL_BOOST_SCALE: f32 = 0.5;

static SYMBOL_QUERY_RE: OnceLock<Regex> = OnceLock::new();

fn symbol_query_re() -> &'static Regex {
    SYMBOL_QUERY_RE.get_or_init(|| {
        // Qualification, underscores, or capitals distinguish symbols from prose.
        Regex::new(
            r"^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$",
        )
        .expect("symbol query regex compile")
    })
}

fn is_symbol_query(query: &str) -> bool {
    let q = query.trim();
    !q.is_empty() && symbol_query_re().is_match(q)
}

static EMBEDDED_SYMBOL_RE: OnceLock<Regex> = OnceLock::new();

fn embedded_symbol_re() -> &'static Regex {
    EMBEDDED_SYMBOL_RE.get_or_init(|| {
        // Require an internal capital and a lowercase letter; exclude plain words and acronyms.
        Regex::new(
            r"\b(?:[A-Z][a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]*|[a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]+)\b",
        )
        .expect("embedded symbol regex compile")
    })
}

fn extract_embedded_symbols(query: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for m in embedded_symbol_re().find_iter(query) {
        seen.insert(m.as_str().to_string());
    }
    seen.into_iter().collect()
}

// Match ModuleRef against both module_ref.py and module-ref.ts.
fn normalize_stem(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect()
}

fn extract_symbol_name(query: &str) -> String {
    let q = query.trim();
    for sep in ["::", "\\", "->", "."] {
        if let Some(idx) = q.rfind(sep) {
            return q[idx + sep.len()..].to_string();
        }
    }
    q.to_string()
}

/// Bound compiled patterns under streams of distinct symbol queries.
const DEFINITION_PATTERN_CACHE_CAP: usize = 64;

static DEFINITION_PATTERN_CACHE: LazyLock<Mutex<HashMap<String, Regex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn build_definition_pattern(symbol_name: &str) -> Option<Regex> {
    if symbol_name.is_empty() {
        return None;
    }
    // Regex clones share the automaton; recover poisoned locks to avoid handler panics.
    if let Some(cached) = DEFINITION_PATTERN_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(symbol_name)
    {
        return Some(cached.clone());
    }
    let compiled = compile_definition_pattern(symbol_name)?;
    let mut cache = DEFINITION_PATTERN_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Clear at capacity; bursty queries do not justify LRU bookkeeping.
    if cache.len() >= DEFINITION_PATTERN_CACHE_CAP {
        cache.clear();
    }
    cache.insert(symbol_name.to_string(), compiled.clone());
    Some(compiled)
}

fn compile_definition_pattern(symbol_name: &str) -> Option<Regex> {
    let escaped = regex::escape(symbol_name);
    static KW_ALTS: OnceLock<String> = OnceLock::new();
    let kw_alts = KW_ALTS.get_or_init(|| {
        DEFINITION_KEYWORDS
            .iter()
            .map(|k| regex::escape(k))
            .collect::<Vec<_>>()
            .join("|")
    });
    // Allow namespace prefixes, but require a declaration keyword and name boundary.
    let pattern = format!(
        r"(?m)(?:^|\s)(?:{kw_alts})\s+(?:[A-Za-z_]\w*(?:\.|::))*{escaped}(?:\s|[<({{:\[;]|$)"
    );
    Regex::new(&pattern).ok()
}

fn apply_definition_boost(results: &mut [SearchResult], query: &str) {
    if results.is_empty() {
        return;
    }
    let max_score = results
        .iter()
        .map(|r| r.score)
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_score.is_finite() || max_score <= 0.0 {
        return;
    }

    let symbols: Vec<String> = if is_symbol_query(query) {
        let name = extract_symbol_name(query);
        if name.len() < 2 {
            Vec::new()
        } else {
            vec![name]
        }
    } else {
        extract_embedded_symbols(query)
    };
    if symbols.is_empty() {
        return;
    }

    let scale = if is_symbol_query(query) {
        1.0
    } else {
        EMBEDDED_SYMBOL_BOOST_SCALE
    };
    let boost_unit = max_score * DEFINITION_BOOST_MULTIPLIER * scale;

    for symbol_name in &symbols {
        let Some(pattern) = build_definition_pattern(symbol_name) else {
            continue;
        };
        let symbol_lower = symbol_name.to_lowercase();
        for r in results.iter_mut() {
            if !pattern.is_match(&r.content) {
                continue;
            }
            let mut boost = boost_unit;
            if let Some(stem) = std::path::Path::new(&r.file_path)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                let stem_lower = stem.to_lowercase();
                let stem_norm = normalize_stem(stem);
                if stem_lower == symbol_lower || stem_norm == symbol_lower {
                    boost *= DEFINITION_FILE_STEM_BONUS;
                }
            }
            r.score += boost;
        }
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

/// Search tuning names. Stage gates and numeric overrides are cached on first read.
mod tuning {
    pub(super) const QUALIFIED_NAME_BOOST: &str = "CODESAGE_QUALIFIED_NAME_BOOST";
    pub(super) const DEFINITION_BOOST: &str = "CODESAGE_DEFINITION_BOOST";
    pub(super) const STEM_SCAN: &str = "CODESAGE_STEM_SCAN";
    pub(super) const TEST_QUERY_AWARE: &str = "CODESAGE_TEST_QUERY_AWARE";
    pub(super) const PATH_PENALTY: &str = "CODESAGE_PATH_PENALTY";
    pub(super) const FILE_SATURATION: &str = "CODESAGE_FILE_SATURATION";
    pub(super) const DIR_SATURATION: &str = "CODESAGE_DIR_SATURATION";
    pub(super) const DIR_SATURATION_THRESHOLD: &str = "CODESAGE_DIR_SATURATION_THRESHOLD";
    pub(super) const DIR_SATURATION_DECAY: &str = "CODESAGE_DIR_SATURATION_DECAY";
    pub(super) const ADAPTIVE_RERANK: &str = "CODESAGE_ADAPTIVE_RERANK";
    pub(super) const VERSION_DEMOTE: &str = "CODESAGE_VERSION_DEMOTE";
    pub(super) const PLATFORM_DEMOTE: &str = "CODESAGE_PLATFORM_DEMOTE";
    pub(super) const PHP_DECLARATION_DEMOTE: &str = "CODESAGE_PHP_DECLARATION_DEMOTE";
    pub(super) const FUSED_RERANK: &str = "CODESAGE_FUSED_RERANK";
    pub(super) const STEM_MATCH_BOOST: &str = "CODESAGE_STEM_MATCH_BOOST";
    pub(super) const HYBRID: &str = "CODESAGE_HYBRID";
    pub(super) const MENTION_ANCHOR: &str = "CODESAGE_MENTION_ANCHOR";
}

/// True unless explicitly set to 0 or false.
pub(crate) fn env_default_on(var: &str) -> bool {
    !matches!(std::env::var(var).as_deref(), Ok("0") | Ok("false"))
}

/// True only when explicitly set to 1 or true.
pub(crate) fn env_default_off(var: &str) -> bool {
    matches!(std::env::var(var).as_deref(), Ok("1") | Ok("true"))
}

static VERSION_DEMOTE_ENABLED: OnceLock<bool> = OnceLock::new();

fn version_demote_enabled() -> bool {
    *VERSION_DEMOTE_ENABLED.get_or_init(|| env_default_on(tuning::VERSION_DEMOTE))
}

static PLATFORM_DEMOTE_ENABLED: OnceLock<bool> = OnceLock::new();

fn platform_demote_enabled() -> bool {
    *PLATFORM_DEMOTE_ENABLED.get_or_init(|| env_default_off(tuning::PLATFORM_DEMOTE))
}

static PHP_DECLARATION_DEMOTE_ENABLED: OnceLock<bool> = OnceLock::new();

fn php_declaration_demote_enabled() -> bool {
    *PHP_DECLARATION_DEMOTE_ENABLED.get_or_init(|| env_default_off(tuning::PHP_DECLARATION_DEMOTE))
}

/// Hybrid fusion mode; Always/Never support ablation of the default literal gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HybridMode {
    Gated,
    Always,
    Never,
}

static HYBRID_MODE: OnceLock<HybridMode> = OnceLock::new();

fn hybrid_mode() -> HybridMode {
    *HYBRID_MODE.get_or_init(|| match std::env::var(tuning::HYBRID).as_deref() {
        Ok("always") => HybridMode::Always,
        Ok("never") => HybridMode::Never,
        _ => HybridMode::Gated,
    })
}

static STEM_MATCH_BOOST_ENABLED: OnceLock<bool> = OnceLock::new();

// Default-off: semble pooled +0.001 was noise (Rust +0.008, C++ -0.004).
// The nlohmann ADL query regressed 1.000 to 0.500; retain as a Rust-project opt-in.
fn stem_match_boost_enabled() -> bool {
    *STEM_MATCH_BOOST_ENABLED.get_or_init(|| env_default_off(tuning::STEM_MATCH_BOOST))
}

static FUSED_RERANK_ENABLED: OnceLock<bool> = OnceLock::new();

// Default-off: reduced-weight fused reranking measured -0.0012 pooled on semble;
// C++ improved 0.004, but C lost 0.008 and TypeScript lost 0.002.
fn fused_rerank_enabled() -> bool {
    *FUSED_RERANK_ENABLED.get_or_init(|| env_default_off(tuning::FUSED_RERANK))
}

static DEFINITION_BOOST_ENABLED: OnceLock<bool> = OnceLock::new();

fn definition_boost_enabled() -> bool {
    *DEFINITION_BOOST_ENABLED.get_or_init(|| env_default_on(tuning::DEFINITION_BOOST))
}

/// Cached lowercase and separator-normalized stems avoid a full path scan per query.
struct StemIndex {
    token: SemanticValidityToken,
    by_lower: HashMap<String, Vec<String>>,
    by_norm: HashMap<String, Vec<String>>,
}

impl StemIndex {
    fn build(db: &Database, token: SemanticValidityToken) -> Result<Self> {
        let mut by_lower: HashMap<String, Vec<String>> = HashMap::new();
        let mut by_norm: HashMap<String, Vec<String>> = HashMap::new();
        for file_path in db.all_chunk_file_paths()? {
            let Some(stem) = std::path::Path::new(&file_path)
                .file_stem()
                .and_then(|s| s.to_str())
            else {
                continue;
            };
            by_lower
                .entry(stem.to_lowercase())
                .or_default()
                .push(file_path.clone());
            by_norm
                .entry(normalize_stem(stem))
                .or_default()
                .push(file_path);
        }
        Ok(Self {
            token,
            by_lower,
            by_norm,
        })
    }

    /// File paths whose stem equals `symbol_lower` (lowercase) or
    /// `symbol_norm` (separator-stripped), deduped, in stable path order.
    fn matching_paths(&self, symbol_lower: &str, symbol_norm: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for hit in [
            self.by_lower.get(symbol_lower),
            self.by_norm.get(symbol_norm),
        ]
        .into_iter()
        .flatten()
        {
            out.extend(hit.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }
}

/// (db file path, chunk table) — see `Database::semantic_cache_key`.
type StemCacheKey = (String, String);
type StemCacheMap = HashMap<StemCacheKey, Arc<StemIndex>>;

/// Cache stems/paths per database and chunk table. The aggregate validity token
/// is a heuristic invalidator; see SemanticValidityToken for collision limits.
static STEM_CACHE: LazyLock<Mutex<StemCacheMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn stem_index_from_cache(
    cache: &Mutex<StemCacheMap>,
    key: StemCacheKey,
    db: &Database,
) -> Result<Arc<StemIndex>> {
    // Build outside the global lock so one project's rebuild cannot block others.
    let token = db.semantic_files_validity_token()?;
    {
        let cache = cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hit) = cache.get(&key)
            && hit.token == token
        {
            return Ok(Arc::clone(hit));
        }
    }
    let built = Arc::new(StemIndex::build(db, token)?);
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    // Reuse a racing builder's Arc when it has the same validity token.
    if let Some(hit) = cache.get(&key)
        && hit.token == token
    {
        return Ok(Arc::clone(hit));
    }
    cache.insert(key, Arc::clone(&built));
    Ok(built)
}

fn stem_index_for(db: &Database) -> Result<Arc<StemIndex>> {
    match db.semantic_cache_key() {
        Some(key) => stem_index_from_cache(&STEM_CACHE, key, db),
        // In-memory handles have no stable process-cache identity.
        None => {
            let token = db.semantic_files_validity_token()?;
            Ok(Arc::new(StemIndex::build(db, token)?))
        }
    }
}

// Recover embedding misses by scanning matching file stems for a definition.
fn apply_non_candidate_stem_scan(
    db: &Database,
    results: &mut Vec<SearchResult>,
    query: &str,
) -> Result<()> {
    if results.is_empty() || !is_symbol_query(query) {
        return Ok(());
    }
    let symbol_name = extract_symbol_name(query);
    // Min length 3 keeps short identifiers from matching every stem in the
    // repo and triggering N file scans.
    if symbol_name.len() < 3 {
        return Ok(());
    }
    let Some(pattern) = build_definition_pattern(&symbol_name) else {
        return Ok(());
    };
    let symbol_lower = symbol_name.to_lowercase();
    let symbol_norm = normalize_stem(&symbol_name);

    let candidate_set: HashSet<String> = results.iter().map(|r| r.file_path.clone()).collect();

    let stem_index = stem_index_for(db)?;
    let mut injected: Vec<SearchResult> = Vec::new();
    for file_path in stem_index.matching_paths(&symbol_lower, &symbol_norm) {
        if candidate_set.contains(&file_path) {
            continue;
        }
        let chunks = db.chunks_for_file(&file_path)?;
        for chunk in chunks {
            if pattern.is_match(&chunk.content) {
                // Definition boosting supplies the initial score after injection.
                injected.push(SearchResult {
                    file_path: chunk.file_path,
                    language: parse_db_language(&chunk.language),
                    content: chunk.content,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score: 0.0,
                    symbols: Vec::new(),
                });
                break;
            }
        }
    }

    results.extend(injected);
    Ok(())
}

static STEM_SCAN_ENABLED: OnceLock<bool> = OnceLock::new();

fn stem_scan_enabled() -> bool {
    *STEM_SCAN_ENABLED.get_or_init(|| env_default_on(tuning::STEM_SCAN))
}

// Search-only demotions: structural indexing and reference queries keep these files.
const SOFT_PENALTY_STRONG: f32 = 0.3; // tests, benches, compat, legacy, examples
const SOFT_PENALTY_MODERATE: f32 = 0.5; // re-export barrels (__init__.py, package-info.java)
const SOFT_PENALTY_MILD: f32 = 0.7; // .d.ts type declaration stubs

const COMPAT_DIR_NAMES: &[&str] = &["compat", "_compat", "legacy", "_legacy"];
// Only plural forms — "example" (singular) collides with the `com.example.*`
// Java/Kotlin package namespace, which is production code, not sample code.
const EXAMPLES_DIR_NAMES: &[&str] = &["examples", "_examples"];
const REEXPORT_BASENAMES: &[&str] = &["__init__.py", "package-info.java"];

static TEST_LIKE_GLOBSET: OnceLock<GlobSet> = OnceLock::new();

fn test_like_globset() -> &'static GlobSet {
    TEST_LIKE_GLOBSET.get_or_init(|| {
        let patterns: Vec<String> = TEST_LIKE_EXCLUDE_PATTERNS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        build_exclude_set(&patterns).expect("TEST_LIKE_EXCLUDE_PATTERNS compile")
    })
}

fn has_dir_segment(path: &str, names: &[&str]) -> bool {
    path.split('/').any(|seg| names.contains(&seg))
}

// Test intent lifts only the test-path demotion; other path penalties still compose.
pub(crate) fn path_penalty_for_query(path: &str, query_is_test_shaped: bool) -> f32 {
    let normalized = if path.contains('\\') {
        path.replace('\\', "/")
    } else {
        path.to_string()
    };
    let mut penalty = 1.0f32;

    if !query_is_test_shaped && test_like_globset().is_match(&normalized) {
        penalty *= SOFT_PENALTY_STRONG * EXTRA_TEST_DEMOTE_NON_TEST_QUERY;
    }
    if has_dir_segment(&normalized, COMPAT_DIR_NAMES) {
        penalty *= SOFT_PENALTY_STRONG;
    }
    if has_dir_segment(&normalized, EXAMPLES_DIR_NAMES) {
        penalty *= SOFT_PENALTY_STRONG;
    }

    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    if REEXPORT_BASENAMES.contains(&basename) {
        penalty *= SOFT_PENALTY_MODERATE;
    }
    if normalized.ends_with(".d.ts") {
        penalty *= SOFT_PENALTY_MILD;
    }

    penalty
}

// Prefer C implementations over declaration headers. C++ headers often contain
// implementations, so gate by detected language; exempt -inl.h and _inl.h.
fn declaration_header_penalty(path: &str, language: Language) -> f32 {
    if language != Language::C {
        return 1.0;
    }
    let normalized = path.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    if !basename.ends_with(".h") {
        return 1.0;
    }
    if basename.ends_with("-inl.h") || basename.ends_with("_inl.h") {
        return 1.0;
    }
    SOFT_PENALTY_MILD
}

// Default-off: host-platform preference is measured only on libuv and may encode
// its benchmark's host assumptions. Validate other C repositories before enabling.
const WINDOWS_PLATFORM_DIR_NAMES: &[&str] = &["win", "win32", "windows"];
const UNIX_PLATFORM_DIR_NAMES: &[&str] = &["unix", "posix", "linux", "darwin", "macos", "bsd"];

fn foreign_platform_penalty(path: &str, windows_host: bool) -> f32 {
    let normalized = path.replace('\\', "/");
    let names = if windows_host {
        UNIX_PLATFORM_DIR_NAMES
    } else {
        WINDOWS_PLATFORM_DIR_NAMES
    };
    if has_dir_segment(&normalized, names) {
        SOFT_PENALTY_MILD
    } else {
        1.0
    }
}

// Whole-token match so "windowsize" or "rewind" can't trip the guard.
const WINDOWS_INTENT_KEYWORDS: &[&str] = &[
    "windows", "win32", "win64", "iocp", "msvc", "mingw", "winapi",
];
const UNIX_INTENT_KEYWORDS: &[&str] = &[
    "unix", "posix", "linux", "darwin", "macos", "bsd", "epoll", "kqueue", "inotify", "pthread",
    "pthreads",
];

fn query_names_foreign_platform(query: &str, windows_host: bool) -> bool {
    let keywords = if windows_host {
        UNIX_INTENT_KEYWORDS
    } else {
        WINDOWS_INTENT_KEYWORDS
    };
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            let lower = t.to_ascii_lowercase();
            keywords.contains(&lower.as_str())
                || matches!(
                    lower.as_str(),
                    "platform" | "platforms" | "portable" | "portability"
                )
        })
}

fn apply_foreign_platform_penalties(results: &mut [SearchResult], query: &str, windows_host: bool) {
    if query_names_foreign_platform(query, windows_host)
        || !results
            .iter()
            .any(|r| foreign_platform_penalty(&r.file_path, !windows_host) < 1.0)
    {
        return;
    }
    for result in results {
        result.score *= foreign_platform_penalty(&result.file_path, windows_host);
    }
}

fn php_declaration_penalty(result: &SearchResult, query: &str) -> f32 {
    if result.language != Language::Php {
        return 1.0;
    }
    let normalized = result.file_path.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    let Some(stem) = basename.strip_suffix(".php") else {
        return 1.0;
    };
    if !stem.ends_with("Interface") && !has_dir_segment(&normalized, &["Contracts", "Facades"]) {
        return 1.0;
    }
    let explicit = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "interface" | "interfaces" | "contract" | "contracts" | "facade" | "facades"
            ) || token.eq_ignore_ascii_case(stem)
                || result
                    .symbols
                    .iter()
                    .any(|s| token.eq_ignore_ascii_case(&s.name))
        });
    if explicit { 1.0 } else { SOFT_PENALTY_MODERATE }
}

// Combined test demotion is 0.3 * 0.5 = 0.15; 0.3 alone left axios tests dominant.
const EXTRA_TEST_DEMOTE_NON_TEST_QUERY: f32 = 0.5;

// Whole alphanumeric tokens, case-insensitive: Login.test.js qualifies, testimony does not.
const TEST_INTENT_KEYWORDS: &[&str] = &[
    "test", "tests", "testing", "spec", "specs", "fixture", "fixtures",
    "phpt", // PHP testing convention
];

fn query_is_test_shaped(query: &str) -> bool {
    if !test_query_aware_enabled() {
        return false;
    }
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .any(|tok| {
            let lowered = tok.to_ascii_lowercase();
            TEST_INTENT_KEYWORDS.contains(&lowered.as_str())
        })
}

static TEST_QUERY_AWARE_ENABLED: OnceLock<bool> = OnceLock::new();

fn test_query_aware_enabled() -> bool {
    *TEST_QUERY_AWARE_ENABLED.get_or_init(|| env_default_on(tuning::TEST_QUERY_AWARE))
}

fn apply_path_penalties(results: &mut [SearchResult], query: &str) {
    let is_test_query = query_is_test_shaped(query);
    let demote_php_declaration = php_declaration_demote_enabled();
    // Require a competing .c implementation; otherwise exempt inline headers get
    // a free relative boost in header-only projects detected as C.
    let has_c_implementation = results
        .iter()
        .any(|r| r.language == Language::C && r.file_path.ends_with(".c"));
    for result in results.iter_mut() {
        let mut penalty = path_penalty_for_query(&result.file_path, is_test_query);
        if has_c_implementation {
            penalty *= declaration_header_penalty(&result.file_path, result.language);
        }
        if demote_php_declaration {
            penalty *= php_declaration_penalty(result, query);
        }
        result.score *= penalty;
    }
    if cfg!(any(unix, windows)) && platform_demote_enabled() {
        apply_foreign_platform_penalties(results, query, cfg!(windows));
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

// Bound score ratios, not rank movement: tightly clustered rows can pass several
// neighbors. Stronger promotion regressed nlohmann's ADL conversion query.
const STEM_MATCH_BOOST: f32 = 1.2;
// Counted in characters, not bytes — see `stem_match_tokens`.
const STEM_MATCH_MIN_TOKEN_LEN: usize = 4;

/// Require an underscore, digit, or mixed case; bare acronyms such as JSON
/// would boost ubiquitous filenames across unrelated queries. ABSL_LOG still qualifies.
fn stem_match_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        // chars(), not len(): `len()` is bytes, so a 3-character token like
        // `Äbc` measures 4 and would slip past the minimum.
        if token.chars().count() < STEM_MATCH_MIN_TOKEN_LEN {
            continue;
        }
        let has_underscore = token.contains('_');
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let has_upper = token.chars().any(char::is_uppercase);
        let has_lower = token.chars().any(char::is_lowercase);
        if has_underscore || has_digit || (has_upper && has_lower) {
            out.push(normalize_stem(token));
        }
    }
    out.sort();
    out.dedup();
    out
}

// Reach C++ free functions and macro-attributed declarations the definition regex misses.
fn apply_stem_match_boost(results: &mut [SearchResult], query: &str) {
    let tokens = stem_match_tokens(query);
    if tokens.is_empty() {
        return;
    }
    let mut boosted = false;
    for result in results.iter_mut() {
        let Some(stem) = std::path::Path::new(&result.file_path)
            .file_stem()
            .and_then(|s| s.to_str())
        else {
            continue;
        };
        let stem_norm = normalize_stem(stem);
        if tokens.contains(&stem_norm) {
            result.score *= STEM_MATCH_BOOST;
            boosted = true;
        }
    }
    if boosted {
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    }
}

/// Numeric version directory (`v3/`, `v4/`) carried by a path, if any.
fn version_dir_of(path: &str) -> Option<u32> {
    path.replace('\\', "/")
        .split('/')
        .filter_map(|seg| seg.strip_prefix('v').or_else(|| seg.strip_prefix('V')))
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|rest| rest.parse::<u32>().ok())
        .max()
}

/// True when the query itself names a version, e.g. "v3 compatibility error
/// types". Such a query is asking for the old line on purpose and must be left
/// alone.
fn query_names_version(query: &str) -> bool {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            let lower = t.to_ascii_lowercase();
            matches!(lower.as_str(), "legacy" | "compat" | "deprecated")
                || lower.strip_prefix('v').is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                })
        })
}

// Prefer the highest version among candidates, unless the query names an old line.
// Candidate-local comparison leaves single-version pages unchanged.
fn apply_version_demote(results: &mut [SearchResult], query: &str) {
    if query_names_version(query) {
        return;
    }
    let Some(max_version) = results
        .iter()
        .filter_map(|r| version_dir_of(&r.file_path))
        .max()
    else {
        return;
    };
    let mut demoted = false;
    for result in results.iter_mut() {
        if version_dir_of(&result.file_path).is_some_and(|v| v < max_version) {
            result.score *= SOFT_PENALTY_MILD;
            demoted = true;
        }
    }
    if demoted {
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    }
}

static PATH_PENALTY_ENABLED: OnceLock<bool> = OnceLock::new();

fn path_penalty_enabled() -> bool {
    *PATH_PENALTY_ENABLED.get_or_init(|| env_default_on(tuning::PATH_PENALTY))
}

// Decay repeated files by 0.5^(N-1) to prevent one file monopolizing the page.
const FILE_SATURATION_THRESHOLD: usize = 1;
const FILE_SATURATION_DECAY: f32 = 0.5;

fn apply_file_saturation(results: &mut [SearchResult]) {
    if results.is_empty() {
        return;
    }
    // Count repeated files in score order regardless of the caller's ordering.
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

    let mut per_file: HashMap<String, usize> = HashMap::new();
    for result in results.iter_mut() {
        let already = per_file.get(&result.file_path).copied().unwrap_or(0);
        if already >= FILE_SATURATION_THRESHOLD {
            let excess = (already - FILE_SATURATION_THRESHOLD + 1) as i32;
            result.score *= FILE_SATURATION_DECAY.powi(excess);
        }
        *per_file.entry(result.file_path.clone()).or_insert(0) += 1;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

// (2, 0.75) improved or tied Laravel, Flask, and Redux in the saturation A/B.
const DIR_SATURATION_THRESHOLD_DEFAULT: usize = 2;
const DIR_SATURATION_DECAY_DEFAULT: f32 = 0.75;

// Read tuning once per process; decay 1.0 disables directory saturation.
static DIR_SATURATION_THRESHOLD: OnceLock<usize> = OnceLock::new();
static DIR_SATURATION_DECAY: OnceLock<f32> = OnceLock::new();

fn dir_saturation_threshold() -> usize {
    *DIR_SATURATION_THRESHOLD.get_or_init(|| {
        std::env::var(tuning::DIR_SATURATION_THRESHOLD)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(DIR_SATURATION_THRESHOLD_DEFAULT)
    })
}

fn dir_saturation_decay() -> f32 {
    *DIR_SATURATION_DECAY.get_or_init(|| {
        std::env::var(tuning::DIR_SATURATION_DECAY)
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|&v| v > 0.0 && v <= 1.0)
            .unwrap_or(DIR_SATURATION_DECAY_DEFAULT)
    })
}

/// Directory saturation catches sibling files missed by per-file saturation;
/// the two penalties intentionally stack.
fn apply_directory_saturation(results: &mut [SearchResult]) {
    if results.is_empty() {
        return;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

    let threshold = dir_saturation_threshold();
    let decay = dir_saturation_decay();
    let mut per_dir: HashMap<String, usize> = HashMap::new();
    for result in results.iter_mut() {
        let dir = parent_dir_for_saturation(&result.file_path);
        let already = per_dir.get(&dir).copied().unwrap_or(0);
        if already >= threshold {
            let excess = (already - threshold + 1) as i32;
            result.score *= decay.powi(excess);
        }
        *per_dir.entry(dir).or_insert(0) += 1;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

/// Return the parent-directory key used for saturation grouping. Repo-
/// root files (no `/`) bucket together under `""`; everything else uses
/// the dirname.
fn parent_dir_for_saturation(file_path: &str) -> String {
    match file_path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

static DIR_SATURATION_ENABLED: OnceLock<bool> = OnceLock::new();

fn dir_saturation_enabled() -> bool {
    *DIR_SATURATION_ENABLED.get_or_init(|| env_default_on(tuning::DIR_SATURATION))
}

static FILE_SATURATION_ENABLED: OnceLock<bool> = OnceLock::new();

fn file_saturation_enabled() -> bool {
    *FILE_SATURATION_ENABLED.get_or_init(|| env_default_on(tuning::FILE_SATURATION))
}

// Lift named candidates to at least top * (1 - gap)^(slot + 1), in original-rank
// order. Never demote or fetch; absent candidates stay absent, and bare identifiers
// do not qualify. Match only within scan_limit and on page one: deeper candidate
// pools and score scales differ, so later-page lifts can make rows unreachable.
const MENTION_TOP_GAP_FRAC: f32 = 0.05;
const MENTION_MAX_FILES: usize = 4;
const MENTION_MAX_CHUNKS_PER_FILE: usize = 3;
// The total cap binds before 4 files * 3 chunks can fill the page.
const MENTION_MAX_ANCHORED: usize = 5;
// `name.ext` tokens with one of these are files, never `Type.method`.
const MENTION_NON_CODE_EXTENSIONS: &[&str] = &[
    "toml", "md", "json", "yaml", "yml", "lock", "txt", "graphql", "proto", "sql", "csv", "xml",
    "html", "css",
];
// An all-lowercase `.member` this short is indistinguishable from a file
// extension (`parser.cc`, `Foo.kt`), so the dotted form needs a longer or
// otherwise qualified member (uppercase, digit, underscore).
const MENTION_DOTTED_MEMBER_AMBIGUOUS_LEN: usize = 4;
const MENTION_TRIM_LEADING: &[char] = &['`', '\'', '"', '(', '[', '{', '<', '*'];
// `(` is trailing-trimmed too, so `Database::open()` reduces to the identifier.
const MENTION_TRIM_TRAILING: &[char] = &[
    '`', '\'', '"', '(', ')', ']', '}', '>', ',', ';', ':', '.', '!', '?', '*',
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryMention {
    /// A path-like token, line/column suffix already stripped.
    Path(String),
    /// A two-segment identifier: `owner::member`, `owner\member`, or
    /// `owner.member`. `stem_fallback` is set for the first two: they are
    /// unambiguously scoped, so a file stem equal to `owner` may stand in
    /// for a missing qualified name. The dotted form is also prose
    /// (`request.body`), so it must match a qualified name.
    Symbol {
        owner: String,
        member: String,
        stem_fallback: bool,
    },
}

/// Strip up to two trailing `:digits` groups (`foo.rs:120`, `foo.rs:120:7`).
/// `Type::method` is untouched: its tail after the last `:` is not numeric.
fn strip_line_suffix(token: &str) -> &str {
    let mut out = token;
    for _ in 0..2 {
        match out.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
                out = head;
            }
            _ => break,
        }
    }
    out
}

/// Require / plus a parser-recognized extension to exclude slash prose and bare
/// basenames. Reject backslashes because indexed paths use forward slashes.
fn path_mention(token: &str) -> Option<String> {
    let t = strip_line_suffix(token);
    let t = t.strip_prefix("./").unwrap_or(t);
    if t.is_empty() || t.ends_with('/') || t.contains('\\') || t.contains("://") || t.contains("::")
    {
        return None;
    }
    let has_slash = t.contains('/');
    let has_source_ext = detect_language(std::path::Path::new(t)).is_some();
    (has_slash && has_source_ext).then(|| t.to_string())
}

fn is_mention_segment(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.chars().count() >= 2
        && (first.is_alphabetic() || first == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn symbol_mention(token: &str) -> Option<QueryMention> {
    let (owner, member, stem_fallback) = if let Some((o, m)) = token.rsplit_once("::") {
        (o, m, true)
    } else if let Some((o, m)) = token.rsplit_once('\\') {
        (o, m, true)
    } else {
        let (o, m) = token.rsplit_once('.')?;
        // `Cargo.toml`, `README.md`: a file, not a member access.
        if MENTION_NON_CODE_EXTENSIONS.contains(&m) {
            return None;
        }
        let ambiguous_with_extension = m.chars().count() <= MENTION_DOTTED_MEMBER_AMBIGUOUS_LEN
            && m.chars().all(|c| c.is_ascii_lowercase());
        if ambiguous_with_extension {
            return None;
        }
        (o, m, false)
    };
    // A deeper scope (`a::b::Type::method`) keeps only the innermost owner.
    let owner = owner.rsplit(['.', ':', '\\']).next().unwrap_or(owner);
    (is_mention_segment(owner) && is_mention_segment(member)).then(|| QueryMention::Symbol {
        owner: owner.to_string(),
        member: member.to_string(),
        stem_fallback,
    })
}

/// Deduplicated query-order mentions; path shape takes precedence over symbol shape.
fn extract_query_mentions(query: &str) -> Vec<QueryMention> {
    let mut out = Vec::new();
    for raw in query.split_whitespace() {
        let token = raw
            .trim_start_matches(MENTION_TRIM_LEADING)
            .trim_end_matches(MENTION_TRIM_TRAILING);
        if token.is_empty() {
            continue;
        }
        let mention = if let Some(path) = path_mention(token) {
            QueryMention::Path(path)
        } else if let Some(symbol) = symbol_mention(token) {
            symbol
        } else {
            continue;
        };
        if !out.contains(&mention) {
            out.push(mention);
        }
    }
    out
}

/// Case-sensitive whole-component suffix in either direction, with at least two
/// components on both sides. Supports absolute pastes but does not prove uniqueness;
/// callers must reject matches spanning multiple candidate files.
fn path_matches_mention(file_path: &str, mention: &str) -> bool {
    let candidate: Vec<&str> = file_path.rsplit('/').collect();
    let wanted: Vec<&str> = mention
        .rsplit('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if wanted.len() < 2 || candidate.len() < 2 {
        return false;
    }
    if wanted.len() <= candidate.len() {
        wanted.iter().zip(&candidate).all(|(w, c)| w == c)
    } else {
        candidate.iter().zip(&wanted).all(|(c, w)| c == w)
    }
}

/// The chunk carries `member` with a qualified name ending in `owner::member`
/// (any separator), or, when `stem_fallback` is set, a file stem equal to
/// `owner`. The stem rule is what reaches Rust free functions, whose
/// qualified name is bare (`search::apply_symbol_boost` has qualified name
/// `apply_symbol_boost`).
fn symbols_match_mention(
    result: &SearchResult,
    owner: &str,
    member: &str,
    stem_fallback: bool,
) -> bool {
    let members: Vec<&SymbolSummary> = result.symbols.iter().filter(|s| s.name == member).collect();
    if members.is_empty() {
        return false;
    }
    let qualified = members.iter().any(|s| {
        let segments = qualified_name_segments(&s.qualified_name);
        segments.len() >= 2
            && segments[segments.len() - 1] == member
            && segments[segments.len() - 2] == owner
    });
    qualified
        || (stem_fallback
            && std::path::Path::new(&result.file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                == Some(owner))
}

/// Share caps across path and symbol mentions so combining them cannot bypass limits.
struct AnchorLedger<'a> {
    anchored: Vec<usize>,
    per_file: HashMap<&'a str, usize>,
}

impl<'a> AnchorLedger<'a> {
    fn try_anchor(&mut self, idx: usize, file_path: &'a str) {
        if self.anchored.len() >= MENTION_MAX_ANCHORED || self.anchored.contains(&idx) {
            return;
        }
        let count = self.per_file.get(file_path).copied().unwrap_or(0);
        if count == 0 && self.per_file.len() >= MENTION_MAX_FILES {
            return;
        }
        if count >= MENTION_MAX_CHUNKS_PER_FILE {
            return;
        }
        self.per_file.insert(file_path, count + 1);
        self.anchored.push(idx);
    }
}

fn apply_mention_anchor(
    results: &mut [SearchResult],
    query: &str,
    scan_limit: usize,
    offset: usize,
    enabled: bool,
) {
    if !enabled || offset != 0 || results.is_empty() {
        return;
    }
    let mentions = extract_query_mentions(query);
    if mentions.is_empty() {
        return;
    }
    let window = &results[..scan_limit.min(results.len())];
    if window.is_empty() {
        return;
    }

    let mut ledger = AnchorLedger {
        anchored: Vec::new(),
        per_file: HashMap::new(),
    };
    for mention in &mentions {
        match mention {
            QueryMention::Path(path) => {
                let hits: Vec<usize> = window
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| path_matches_mention(&r.file_path, path))
                    .map(|(idx, _)| idx)
                    .collect();
                if hits.is_empty() {
                    tracing::debug!(
                        mention = %path,
                        "query names a file absent from the candidate set; not fetched"
                    );
                    continue;
                }
                // Reject multi-file suffixes within the window. Ambiguity outside
                // retrieved candidates is invisible; several chunks of one file are fine.
                let distinct: HashSet<&str> =
                    hits.iter().map(|&i| window[i].file_path.as_str()).collect();
                if distinct.len() > 1 {
                    tracing::debug!(
                        mention = %path,
                        files = distinct.len(),
                        "path mention matches several files; skipped as non-specific"
                    );
                    continue;
                }
                for idx in hits {
                    ledger.try_anchor(idx, window[idx].file_path.as_str());
                }
            }
            QueryMention::Symbol {
                owner,
                member,
                stem_fallback,
            } => {
                for (idx, result) in window.iter().enumerate() {
                    if ledger.anchored.len() >= MENTION_MAX_ANCHORED {
                        break;
                    }
                    if symbols_match_mention(result, owner, member, *stem_fallback) {
                        ledger.try_anchor(idx, result.file_path.as_str());
                    }
                }
            }
        }
    }
    let mut anchored = ledger.anchored;
    if anchored.is_empty() {
        return;
    }

    // Preserve original rank; use the window maximum defensively for unsorted input.
    anchored.sort_unstable();
    let top = window.iter().map(|r| r.score).fold(f32::MIN, f32::max);
    let mut lifted = false;
    for (slot, &idx) in anchored.iter().enumerate() {
        let target = top * (1.0 - MENTION_TOP_GAP_FRAC).powi(slot as i32 + 1);
        if results[idx].score < target {
            results[idx].score = target;
            lifted = true;
        }
    }
    if lifted {
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    }
}

static MENTION_ANCHOR_ENABLED: OnceLock<bool> = OnceLock::new();

fn mention_anchor_enabled() -> bool {
    *MENTION_ANCHOR_ENABLED.get_or_init(|| env_default_on(tuning::MENTION_ANCHOR))
}

const RERANK_WEIGHT_DEFAULT: f32 = 0.5;
const RERANK_WEIGHT_SHORT_ID: f32 = 0.35;
const RERANK_WEIGHT_NATLANG: f32 = 0.6;

/// Use 0.35 for a single identifier, 0.6 for at least three alphabetic words,
/// and 0.5 otherwise. Opt-in fused reranking overrides this with 0.35.
fn adaptive_rerank_weight(query: &str) -> f32 {
    if !adaptive_rerank_weight_enabled() {
        return RERANK_WEIGHT_DEFAULT;
    }
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return RERANK_WEIGHT_DEFAULT;
    }
    let single_token = !trimmed.chars().any(char::is_whitespace);
    if single_token && looks_like_short_identifier(trimmed) {
        return RERANK_WEIGHT_SHORT_ID;
    }
    let alpha_words: Vec<&str> = trimmed
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.' || c == ';')
        .filter(|s| s.chars().all(|c| c.is_alphabetic()) && s.len() >= 2)
        .collect();
    if alpha_words.len() >= 3 {
        return RERANK_WEIGHT_NATLANG;
    }
    RERANK_WEIGHT_DEFAULT
}

static ADAPTIVE_RERANK_ENABLED: OnceLock<bool> = OnceLock::new();

fn adaptive_rerank_weight_enabled() -> bool {
    *ADAPTIVE_RERANK_ENABLED.get_or_init(|| env_default_on(tuning::ADAPTIVE_RERANK))
}

/// Return whether reranking succeeded; failures retain scores/order and warn.
/// weight_override bypasses query-shape weighting for opt-in fused reranking.
fn apply_reranking(
    rerank: &mut RerankFn<'_>,
    query: &str,
    results: &mut [SearchResult],
    weight_override: Option<f32>,
) -> bool {
    if results.is_empty() {
        return false;
    }

    let docs: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
    let ce_scores = match rerank(query, &docs) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                candidates = results.len(),
                "cross-encoder rerank failed; keeping pre-rerank order"
            );
            return false;
        }
    };

    let ce_min = ce_scores.iter().cloned().fold(f32::INFINITY, f32::min);
    let ce_max = ce_scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let ce_range = ce_max - ce_min;

    let weight = weight_override.unwrap_or_else(|| adaptive_rerank_weight(query));
    for (result, &ce_raw) in results.iter_mut().zip(ce_scores.iter()) {
        let ce_norm = if ce_range > 1e-6 {
            (ce_raw - ce_min) / ce_range
        } else {
            0.5
        };
        result.score = (1.0 - weight) * result.score + weight * ce_norm;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    true
}

pub(crate) fn annotate_with_symbols(db: &Database, results: &mut [SearchResult]) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }

    let distinct_files: Vec<String> = {
        let set: HashSet<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
        set.into_iter().map(|s| s.to_string()).collect()
    };
    let by_file = db.symbols_for_files(&distinct_files)?;

    for result in results.iter_mut() {
        let symbols: &[Symbol] = by_file
            .get(&result.file_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let overlapping: Vec<SymbolSummary> = symbols
            .iter()
            .filter(|s| s.line_start <= result.end_line && s.line_end >= result.start_line)
            .map(|s| SymbolSummary {
                name: s.name.clone(),
                qualified_name: s.qualified_name.clone(),
                kind: s.kind,
            })
            .collect();

        result.symbols = overlapping;
    }
    Ok(())
}

#[cfg(test)]
mod hybrid_tests {
    use super::*;

    fn mk_embedding(v: f32) -> Vec<f32> {
        let mut e = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        for slot in e.iter_mut().take(10) {
            *slot = v;
        }
        e
    }

    fn seed_chunks(db: &Database) {
        // Four chunks: three generic, one with a distinctive literal the
        // gate should trigger on.
        db.insert_chunks(
            "src/lib.rs",
            "rust",
            &[(
                "fn auth() { println!(\"authentication logic\"); }",
                1,
                10,
                mk_embedding(0.1).as_slice(),
            )],
        )
        .unwrap();
        db.insert_chunks(
            "src/db.rs",
            "rust",
            &[(
                "fn connect() { println!(\"database pool\"); }",
                1,
                10,
                mk_embedding(0.2).as_slice(),
            )],
        )
        .unwrap();
        db.insert_chunks(
            "src/reg.rs",
            "rust",
            &[(
                "// registers ColdFusion and BoxLang file types",
                1,
                5,
                mk_embedding(0.3).as_slice(),
            )],
        )
        .unwrap();
        db.insert_chunks(
            "src/misc.rs",
            "rust",
            &[("fn handler() { }", 1, 5, mk_embedding(0.4).as_slice())],
        )
        .unwrap();
    }

    #[test]
    fn gate_triggers_on_backtick() {
        let db = Database::open_in_memory().unwrap();
        assert!(query_has_rare_literal(&db, "use `doc_cfg` here").unwrap());
    }

    #[test]
    fn gate_triggers_on_scope_resolution() {
        let db = Database::open_in_memory().unwrap();
        assert!(query_has_rare_literal(&db, "call ModuleRef::create").unwrap());
    }

    #[test]
    fn gate_triggers_on_glob_extension() {
        let db = Database::open_in_memory().unwrap();
        assert!(query_has_rare_literal(&db, "add *.svelte.ts to globs").unwrap());
    }

    #[test]
    fn gate_rejects_plain_english_query() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        assert!(!query_has_rare_literal(&db, "where is authentication handled").unwrap());
    }

    #[test]
    fn gate_triggers_on_rare_long_identifier() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        // ColdFusion occurs in 1/4 chunks: 25% exceeds the 1% rarity threshold.
        assert!(!query_has_rare_literal(&db, "ColdFusion support").unwrap());
    }

    #[test]
    fn build_fts_match_query_quotes_identifiers() {
        let q = build_fts_match_query("printer: use `doc_cfg` instead of `doc_auto_cfg`");
        assert!(q.contains("\"doc_cfg\""));
        assert!(q.contains("\"doc_auto_cfg\""));
        assert!(q.contains(" OR "));
    }

    #[test]
    fn build_fts_match_query_handles_scoped() {
        let q = build_fts_match_query("call ModuleRef::create");
        assert_eq!(q, "\"ModuleRef\"");
    }

    #[test]
    fn build_fts_match_query_drops_plain_english() {
        assert_eq!(build_fts_match_query("use this instead of that"), "");
    }

    #[test]
    fn gate_triggers_on_dotted_identifier_pair() {
        let db = Database::open_in_memory().unwrap();
        assert!(query_has_rare_literal(&db, "fix moduleref.create edge").unwrap());
    }

    #[test]
    fn gate_does_not_trigger_on_sentence_punctuation() {
        let db = Database::open_in_memory().unwrap();
        assert!(!query_has_rare_literal(&db, "fix e.g. the handler").unwrap());
        assert!(!query_has_rare_literal(&db, "fix i.e. the handler").unwrap());
    }

    #[test]
    fn dotted_tokens_survive_code_shape_filter() {
        let q = build_fts_match_query("edge case with moduleref.create");
        assert!(q.contains("\"moduleref\""));
        assert!(q.contains("\"create\""));
    }

    #[test]
    fn build_fts_match_query_keeps_mixed_code_tokens() {
        let q = build_fts_match_query("printer: use `doc_cfg` instead of `doc_auto_cfg`");
        assert!(q.contains("\"doc_cfg\""));
        assert!(q.contains("\"doc_auto_cfg\""));
        assert!(!q.contains("\"printer\""));
        assert!(!q.contains("\"use\""));
        assert!(!q.contains("\"instead\""));
    }

    #[test]
    fn rrf_merge_prioritizes_rows_that_appear_in_both_lists() {
        let a = RawSearchRow {
            file_path: "a.rs".into(),
            language: "rust".into(),
            content: "a".into(),
            start_line: 1,
            end_line: 1,
            distance: 0.0,
        };
        let b = RawSearchRow {
            file_path: "b.rs".into(),
            language: "rust".into(),
            content: "b".into(),
            start_line: 1,
            end_line: 1,
            distance: 0.0,
        };
        let c = RawSearchRow {
            file_path: "c.rs".into(),
            language: "rust".into(),
            content: "c".into(),
            start_line: 1,
            end_line: 1,
            distance: 0.0,
        };
        // c receives semantic and BM25 contributions; a and b receive one.
        let semantic = vec![a.clone(), b.clone(), c.clone()];
        let bm25 = vec![c.clone()];
        let out = rrf_merge(semantic, bm25, 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].file_path, "c.rs");
    }

    #[test]
    fn apply_offset_and_limit_clears_rows_when_offset_reaches_end() {
        let mut rows = vec![1, 2, 3];
        apply_offset_and_limit(&mut rows, 3, 10);
        assert!(rows.is_empty());
    }

    #[test]
    fn apply_offset_and_limit_clears_rows_when_offset_exceeds_end() {
        let mut rows = vec![1, 2, 3];
        apply_offset_and_limit(&mut rows, 9, 10);
        assert!(rows.is_empty());
    }

    #[test]
    fn apply_offset_and_limit_keeps_requested_page() {
        let mut rows = vec![1, 2, 3, 4, 5];
        apply_offset_and_limit(&mut rows, 2, 2);
        assert_eq!(rows, vec![3, 4]);
    }

    #[test]
    fn relevance_cliff_cuts_at_largest_relative_drop() {
        let cut = relevance_cliff(&[1.0, 0.95, 0.5, 0.48]);
        assert_eq!(cut.confidence, SearchConfidence::High);
        assert_eq!(cut.cut, 2);
        assert_eq!(cut.drop_pct, 47);
    }

    #[test]
    fn relevance_cliff_flat_page_is_low_and_keeps_everything() {
        let cut = relevance_cliff(&[1.0, 0.97, 0.95]);
        assert_eq!(cut.confidence, SearchConfidence::Low);
        assert_eq!(cut.cut, 3);
        assert_eq!(cut.drop_pct, 3);
    }

    #[test]
    fn relevance_cliff_gates_on_the_rounded_percentage() {
        // 19.54% rounds to the disclosed 20; the verdict must agree with it.
        let rounds_up = relevance_cliff(&[1.0, 0.8046]);
        assert_eq!(rounds_up.confidence, SearchConfidence::High);
        assert_eq!(rounds_up.cut, 1);
        assert_eq!(rounds_up.drop_pct, 20);

        let just_under = relevance_cliff(&[1.0, 0.81]);
        assert_eq!(just_under.confidence, SearchConfidence::Low);
        assert_eq!(just_under.cut, 2);
        assert_eq!(just_under.drop_pct, 19);
    }

    #[test]
    fn relevance_cliff_never_splits_ties() {
        let all_tied = relevance_cliff(&[0.7, 0.7, 0.7]);
        assert_eq!(all_tied.confidence, SearchConfidence::Low);
        assert_eq!(all_tied.cut, 3);
        assert_eq!(all_tied.drop_pct, 0);

        let two_bands = relevance_cliff(&[1.0, 1.0, 0.5, 0.5]);
        assert_eq!(two_bands.confidence, SearchConfidence::High);
        assert_eq!(two_bands.cut, 2);
        assert_eq!(two_bands.drop_pct, 50);

        let zero_tail = relevance_cliff(&[0.5, 0.5, 0.0, 0.0]);
        assert_eq!(zero_tail.confidence, SearchConfidence::High);
        assert_eq!(zero_tail.cut, 2);
        assert_eq!(zero_tail.drop_pct, 100);
    }

    #[test]
    fn relevance_cliff_handles_empty_single_and_non_finite() {
        let empty = relevance_cliff(&[]);
        assert_eq!(
            (empty.confidence, empty.cut, empty.drop_pct),
            (SearchConfidence::Low, 0, 0)
        );
        let single = relevance_cliff(&[0.9]);
        assert_eq!(
            (single.confidence, single.cut, single.drop_pct),
            (SearchConfidence::Low, 1, 0)
        );
        let nan_mid = relevance_cliff(&[1.0, f32::NAN, 0.9]);
        assert_eq!(nan_mid.confidence, SearchConfidence::Low);
        assert_eq!(nan_mid.cut, 3);
        assert_eq!(nan_mid.drop_pct, 0);
        let inf_lead = relevance_cliff(&[f32::INFINITY, 0.1, 0.05]);
        assert_eq!(inf_lead.confidence, SearchConfidence::High);
        assert_eq!(inf_lead.cut, 2);
        assert_eq!(inf_lead.drop_pct, 50);
    }

    #[test]
    fn search_page_discloses_cliff_and_keeps_full_page_by_default() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let emb = mk_embedding(0.1);
        let rerank: RerankFn = Box::new(|_q, docs| {
            Ok(docs
                .iter()
                .map(|d| {
                    if d.contains("authentication") {
                        10.0
                    } else {
                        0.0
                    }
                })
                .collect())
        });
        let mut req = search_req("authentication logic");
        req.limit = Some(4);

        let page = search_page(&db, &emb, Some(rerank), &req).unwrap();
        assert_eq!(page.results.len(), 4, "default path fills `limit` rows");
        assert_eq!(page.confidence, Some(SearchConfidence::High));
        let cliff_at = page.cliff_at.expect("cliff_at disclosed");
        assert!(
            (1..4).contains(&cliff_at),
            "cliff must sit strictly inside the page, got {cliff_at}"
        );
        assert!(page.margin_pct.unwrap() >= 20);

        let rerank: RerankFn = Box::new(|_q, docs| {
            Ok(docs
                .iter()
                .map(|d| {
                    if d.contains("authentication") {
                        10.0
                    } else {
                        0.0
                    }
                })
                .collect())
        });
        req.adaptive_limit = true;
        let cut_page = search_page(&db, &emb, Some(rerank), &req).unwrap();
        assert_eq!(cut_page.results.len(), cliff_at);
        assert_eq!(cut_page.cliff_at, Some(cliff_at));
        assert_eq!(cut_page.confidence, Some(SearchConfidence::High));
        assert_eq!(cut_page.results[0].file_path, "src/lib.rs");
    }

    #[test]
    fn adaptive_limit_does_not_cut_a_flat_page() {
        // Separate directories avoid saturation; identical chunks yield equal scores.
        let db = Database::open_in_memory().unwrap();
        for path in ["a.rs", "b/x.rs", "c/y.rs", "d/z.rs"] {
            db.insert_chunks(
                path,
                "rust",
                &[("fn handler() { }", 1, 5, mk_embedding(0.1).as_slice())],
            )
            .unwrap();
        }
        let mut req = search_req("handler");
        req.limit = Some(4);
        req.adaptive_limit = true;

        let page = search_page(&db, &mk_embedding(0.1), None, &req).unwrap();
        assert_eq!(page.confidence, Some(SearchConfidence::Low));
        assert_eq!(page.results.len(), 4);
        assert_eq!(page.cliff_at, Some(4));
        assert_eq!(page.margin_pct, Some(0));
    }

    #[test]
    fn search_bm25_returns_chunks_containing_rare_literal() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let rows = db.search_bm25("\"ColdFusion\"", 10, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");
    }

    #[test]
    fn search_bm25_filters_by_language_set() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        db.insert_chunks(
            "src/legacy.php",
            "php",
            &[(
                "// ColdFusion PHP bridge",
                1,
                2,
                mk_embedding(0.5).as_slice(),
            )],
        )
        .unwrap();

        let rows = db
            .search_bm25("\"ColdFusion\"", 10, Some(&["rust"]), None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");

        let mut rows = db
            .search_bm25("\"ColdFusion\"", 10, Some(&["rust", "php"]), None)
            .unwrap();
        rows.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        let paths: Vec<&str> = rows.iter().map(|r| r.file_path.as_str()).collect();
        assert_eq!(paths, ["src/legacy.php", "src/reg.rs"]);

        let rows = db
            .search_bm25("\"ColdFusion\"", 10, Some(&[]), None)
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn search_bm25_respects_path_filters() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);

        let rows = db
            .search_bm25("\"ColdFusion\"", 10, None, Some(&["src/lib.rs"]))
            .unwrap();
        assert!(rows.is_empty());

        let rows = db
            .search_bm25("\"ColdFusion\"", 10, None, Some(&["src/reg.rs"]))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");
    }

    #[test]
    fn bm25_candidate_fetch_keeps_rows_for_final_offset_page() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);

        let rows =
            bm25_search_candidates(&db, "\"ColdFusion\"", 10, None, Some(&["src/reg.rs"])).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");
    }

    #[test]
    fn path_filtered_knn_candidates_apply_glob_after_bounded_knn() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        db.insert_chunks(
            "vendor/close.rs",
            "rust",
            &[("fn vendor_close() {}", 1, 5, mk_embedding(0.05).as_slice())],
        )
        .unwrap();
        let query_bytes = embedding_to_bytes(&mk_embedding(0.05));
        let rows = path_filtered_knn_candidates(
            &db,
            &query_bytes,
            10,
            Some(&[Language::Rust]),
            &["src/*".to_string()],
        )
        .unwrap();

        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|r| r.file_path.starts_with("src/")),
            "path-filtered KNN must not leak nonmatching files: {rows:?}"
        );
    }

    #[test]
    fn hybrid_search_respects_multi_language_filters_after_bm25_fusion() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        db.insert_chunks(
            "src/legacy.php",
            "php",
            &[(
                "// ColdFusion::register legacy PHP integration",
                1,
                3,
                mk_embedding(0.05).as_slice(),
            )],
        )
        .unwrap();
        let mut req = search_req("ColdFusion::register");
        req.languages = Some(vec![Language::Rust, Language::TypeScript]);

        let results = search(&db, &mk_embedding(0.05), None, &req).unwrap();

        assert!(
            results.iter().all(|r| r.language != Language::Php),
            "BM25-fused results must not leak excluded languages: {results:?}"
        );
    }

    #[test]
    fn bm25_leg_filters_to_requested_language_set_so_matches_are_not_displaced() {
        let db = Database::open_in_memory().unwrap();
        // Rust decoys sitting right on the query embedding, no rare token:
        // they fill the semantic candidate pool.
        db.insert_chunks(
            "src/a.rs",
            "rust",
            &[("fn decoy_a() {}", 1, 5, mk_embedding(0.1).as_slice())],
        )
        .unwrap();
        db.insert_chunks(
            "src/b.rs",
            "rust",
            &[("fn decoy_b() {}", 1, 5, mk_embedding(0.1).as_slice())],
        )
        .unwrap();
        db.insert_chunks(
            "src/c.rs",
            "rust",
            &[("fn decoy_c() {}", 1, 5, mk_embedding(0.1).as_slice())],
        )
        .unwrap();
        // The target: matching-language chunk with the rare literal, far from
        // the query embedding so only the BM25 leg can surface it. Longer
        // content keeps its BM25 rank below the dense foreign rows.
        db.insert_chunks(
            "src/target.rs",
            "rust",
            &[(
                "// ColdFusion registration entry point with a long body of prose",
                1,
                5,
                mk_embedding(0.9).as_slice(),
            )],
        )
        .unwrap();
        // Foreign-language rows stuffed with the token: an unfiltered BM25
        // leg (k = 3) returns only these.
        for path in ["php/a.php", "php/b.php", "php/c.php", "php/d.php"] {
            db.insert_chunks(
                path,
                "php",
                &[("ColdFusion ColdFusion", 1, 2, mk_embedding(0.5).as_slice())],
            )
            .unwrap();
        }
        db.insert_chunks(
            "pkg/x.go",
            "go",
            &[("ColdFusion go side", 1, 2, mk_embedding(0.5).as_slice())],
        )
        .unwrap();

        let mut req = search_req("ColdFusion::register");
        req.languages = Some(vec![Language::Rust, Language::TypeScript]);
        req.limit = Some(3);

        let results = search(&db, &mk_embedding(0.1), None, &req).unwrap();

        assert!(
            results.iter().any(|r| r.file_path == "src/target.rs"),
            "matching-language BM25 hit must not be displaced by \
             foreign-language rows occupying the fused pool: {results:?}"
        );
        assert!(
            results.iter().all(|r| r.language == Language::Rust),
            "no foreign-language rows may survive: {results:?}"
        );
    }

    fn search_req(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            limit: Some(10),
            offset: Some(0),
            languages: None,
            paths: None,
            adaptive_limit: false,
        }
    }

    #[test]
    fn gated_query_with_empty_bm25_still_applies_reranking() {
        // :: triggers the gate but absent Zzz yields no BM25 hits.
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let emb = mk_embedding(0.1);
        let mut called = false;
        let rerank: RerankFn = Box::new(|_q, docs| {
            called = true;
            Ok(vec![0.0; docs.len()])
        });
        let results = search(&db, &emb, Some(rerank), &search_req("Zzz::qqq")).unwrap();
        assert!(
            called,
            "reranking must still run when BM25 fusion produced nothing"
        );
        assert!(!results.is_empty());
    }

    #[test]
    fn fused_query_skips_reranking_by_default() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let emb = mk_embedding(0.1);
        let mut called = false;
        let rerank: RerankFn = Box::new(|_q, docs| {
            called = true;
            Ok(vec![0.0; docs.len()])
        });
        search(&db, &emb, Some(rerank), &search_req("ColdFusion::register")).unwrap();
        assert!(!called, "reranking must be skipped when RRF fusion ran");
    }

    #[test]
    fn reduced_fused_weight_protects_a_win_that_the_natlang_weight_would_lose() {
        // A hostile cross-encoder requires a lead > w/(1-w): ~0.54 at 0.35,
        // 1.5 at 0.6. This 0.60 lead survives only the reduced weight;
        // narrower fused wins can still flip.
        use super::{RERANK_WEIGHT_NATLANG, RERANK_WEIGHT_SHORT_ID, apply_reranking};

        let mk = |file: &str, score: f32| SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: file.to_string(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        };
        // Cross-encoder ranks the fused winner last.
        let ce = |_q: &str, docs: &[&str]| {
            Ok(docs
                .iter()
                .map(|d| if d.contains("reg.rs") { 0.0 } else { 1.0 })
                .collect())
        };

        let mut kept = vec![mk("src/reg.rs", 0.95), mk("src/lib.rs", 0.35)];
        let mut rerank: RerankFn = Box::new(ce);
        assert!(apply_reranking(
            &mut rerank,
            "ColdFusion::register",
            &mut kept,
            Some(RERANK_WEIGHT_SHORT_ID),
        ));
        assert_eq!(
            kept[0].file_path, "src/reg.rs",
            "fused winner should survive at the SHORT_ID weight"
        );

        let mut lost = vec![mk("src/reg.rs", 0.95), mk("src/lib.rs", 0.35)];
        let mut rerank: RerankFn = Box::new(ce);
        assert!(apply_reranking(
            &mut rerank,
            "ColdFusion::register",
            &mut lost,
            Some(RERANK_WEIGHT_NATLANG),
        ));
        assert_eq!(
            lost[0].file_path, "src/lib.rs",
            "same disagreement should flip the order at the natural-language weight"
        );
    }

    #[test]
    fn fused_scores_rescale_to_semantic_span_so_flat_boost_cannot_invert() {
        let top = RawSearchRow {
            file_path: "top.rs".into(),
            language: "rust".into(),
            content: "fn top() {}".into(),
            start_line: 1,
            end_line: 1,
            distance: 0.2, // l2_to_score = 0.98
        };
        let mid = RawSearchRow {
            file_path: "mid.rs".into(),
            language: "rust".into(),
            content: "uses known_sym here".into(),
            start_line: 1,
            end_line: 1,
            distance: 1.2, // l2_to_score = 0.28
        };
        // top is rank 1 in both lists; mid appears in semantic only.
        let fused = rrf_merge(vec![top.clone(), mid], vec![top], 10);
        let mut results: Vec<SearchResult> = fused
            .into_iter()
            .map(|r| SearchResult {
                file_path: r.file_path,
                language: codesage_protocol::Language::Rust,
                content: r.content,
                start_line: r.start_line,
                end_line: r.end_line,
                score: l2_to_score(r.distance),
                symbols: Vec::new(),
            })
            .collect();
        assert_eq!(results[0].file_path, "top.rs");
        assert!(
            (results[0].score - 0.98).abs() < 1e-3,
            "top: {}",
            results[0].score
        );
        assert!(
            (results[1].score - 0.28).abs() < 1e-3,
            "mid: {}",
            results[1].score
        );

        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }

    #[test]
    fn fused_rescale_spreads_rows_when_all_semantic_scores_are_equal() {
        let mk_row = |path: &str, content: &str| RawSearchRow {
            file_path: path.into(),
            language: "rust".into(),
            content: content.into(),
            start_line: 1,
            end_line: 1,
            distance: 0.2, // identical for every row => l2_to_score = 0.98
        };
        let semantic = vec![
            mk_row("top.rs", "fn top() {}"),
            mk_row("second.rs", "fn second() {}"),
            mk_row("mid.rs", "uses known_sym here"),
            mk_row("low.rs", "fn low() {}"),
        ];
        let fused = rrf_merge(semantic, Vec::new(), 10);
        let mut results: Vec<SearchResult> = fused
            .into_iter()
            .map(|r| SearchResult {
                file_path: r.file_path,
                language: codesage_protocol::Language::Rust,
                content: r.content,
                start_line: r.start_line,
                end_line: r.end_line,
                score: l2_to_score(r.distance),
                symbols: Vec::new(),
            })
            .collect();

        assert_eq!(results[0].file_path, "top.rs");
        assert!(
            results[0].score > results.last().unwrap().score,
            "equal semantic scores must still yield a spread fused ranking: {:?}",
            results.iter().map(|r| r.score).collect::<Vec<_>>()
        );

        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }

    #[test]
    fn fused_rescale_uses_synthetic_span_on_near_tied_semantic_scores() {
        let mk_row = |path: &str, content: &str, distance: f32| RawSearchRow {
            file_path: path.into(),
            language: "rust".into(),
            content: content.into(),
            start_line: 1,
            end_line: 1,
            distance,
        };
        // l2_to_score spans ~6e-7 across these four distances — wider than
        // f32::EPSILON, far narrower than MIN_FUSED_RESCALE_SPAN.
        let semantic = vec![
            mk_row("top.rs", "fn top() {}", 0.2),
            mk_row("second.rs", "fn second() {}", 0.2000005),
            mk_row("mid.rs", "uses known_sym here", 0.200001),
            mk_row("low.rs", "fn low() {}", 0.2000015),
        ];
        let fused = rrf_merge(semantic, Vec::new(), 10);
        let mut results: Vec<SearchResult> = fused
            .into_iter()
            .map(|r| SearchResult {
                file_path: r.file_path,
                language: codesage_protocol::Language::Rust,
                content: r.content,
                start_line: r.start_line,
                end_line: r.end_line,
                score: l2_to_score(r.distance),
                symbols: Vec::new(),
            })
            .collect();

        assert_eq!(results[0].file_path, "top.rs");
        assert!(
            results[0].score - results.last().unwrap().score > 0.1,
            "near-tied semantic scores must still yield a usable fused spread: {:?}",
            results.iter().map(|r| r.score).collect::<Vec<_>>()
        );

        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }
}

#[cfg(test)]
mod path_penalty_tests {
    use super::path_penalty_for_query;

    fn penalty(path: &str) -> f32 {
        path_penalty_for_query(path, false)
    }

    fn assert_penalty(path: &str, expected: f32) {
        let p = penalty(path);
        assert!((p - expected).abs() < 1e-6, "{path}: got {p}");
    }

    #[test]
    fn production_code_keeps_full_score() {
        assert_penalty("src/auth/login.rs", 1.0);
        assert_penalty("crates/graph/src/query.rs", 1.0);
        assert_penalty("packages/common/pipes/parse-date.pipe.ts", 1.0);
    }

    #[test]
    fn test_files_get_strong_penalty() {
        assert_penalty("tests/integration.rs", 0.15);
        assert_penalty("crates/graph/tests/risk.rs", 0.15);
        assert_penalty("packages/core/test/auth.spec.ts", 0.15);
        assert_penalty("src/__tests__/login.test.ts", 0.15);
        assert_penalty("ext/standard/tests/string/foo.phpt", 0.15);
        assert_penalty("tests/test_login.py", 0.15);
        assert_penalty("foo/bar/something_test.go", 0.15);
        assert_penalty("src/Login/LoginTest.php", 0.15);
    }

    #[test]
    fn bench_files_get_strong_penalty() {
        assert_penalty("benches/throughput.rs", 0.15);
        assert_penalty("benchmarks/end_to_end.py", 0.15);
    }

    #[test]
    fn compat_legacy_dirs_get_strong_penalty() {
        assert_penalty("src/compat/php7.php", 0.3);
        assert_penalty("src/_compat/legacy_api.rs", 0.3);
        assert_penalty("packages/legacy/v1/foo.ts", 0.3);
        let p = path_penalty_for_query("src/compat/php7.php", true);
        assert!((p - 0.3).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn examples_dirs_get_strong_penalty() {
        assert_penalty("examples/quickstart.rs", 0.3);
        assert_penalty("packages/sdk/examples/main.go", 0.3);
        assert_penalty("src/_examples/demo.py", 0.3);
    }

    #[test]
    fn reexport_barrels_get_moderate_penalty() {
        assert_penalty("src/auth/__init__.py", 0.5);
        assert_penalty("com/example/foo/package-info.java", 0.5);
    }

    #[test]
    fn type_declarations_get_mild_penalty() {
        assert_penalty("types/express.d.ts", 0.7);
    }

    #[test]
    fn penalties_compose_multiplicatively() {
        // Test in compat/ — the test-like demote (0.3 * 0.5) AND the strong
        // compat penalty stack: 0.15 * 0.3 = 0.045.
        assert_penalty("compat/tests/old_api_test.go", 0.045);
        // A test-shaped query lifts only the test-like part; compat remains.
        let p = path_penalty_for_query("compat/tests/old_api_test.go", true);
        assert!((p - 0.3).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn windows_separators_normalize() {
        assert_penalty(r"tests\integration.rs", 0.15);
    }

    #[test]
    fn substring_match_does_not_trigger_dir_penalty() {
        assert_penalty("src/compatibility/check.rs", 1.0);
        assert_penalty("src/examplesite/index.ts", 1.0);
        assert_penalty("src/utilities.rs", 1.0);
    }
}

#[cfg(test)]
mod test_query_aware_penalty_tests {
    use super::{SearchResult, apply_path_penalties, path_penalty_for_query, query_is_test_shaped};

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::JavaScript,
            content: String::new(),
            start_line: 0,
            end_line: 0,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn classifier_detects_test_intent() {
        assert!(query_is_test_shaped("test for InterceptorManager"));
        assert!(query_is_test_shaped("InterceptorManager test"));
        assert!(query_is_test_shaped("Authentication spec"));
        assert!(query_is_test_shaped("login fixtures"));
        assert!(query_is_test_shaped("phpt for string"));
        assert!(query_is_test_shaped("UPPERCASE TEST query")); // case-insensitive
    }

    #[test]
    fn classifier_skips_production_intent() {
        assert!(!query_is_test_shaped("request and response interceptors"));
        assert!(!query_is_test_shaped("queue connection resolution"));
        assert!(!query_is_test_shaped("authentication handler"));
        assert!(!query_is_test_shaped("testimony"));
        assert!(!query_is_test_shaped("contest results"));
    }

    #[test]
    fn non_test_query_demotes_tests_harder() {
        let p = path_penalty_for_query("tests/integration.rs", false);
        assert!((p - 0.15).abs() < 1e-6, "got {}", p);
        let p = path_penalty_for_query("src/__tests__/login.test.ts", false);
        assert!((p - 0.15).abs() < 1e-6, "got {}", p);
    }

    #[test]
    fn test_query_lifts_test_penalty() {
        assert_eq!(path_penalty_for_query("tests/integration.rs", true), 1.0);
        assert_eq!(
            path_penalty_for_query("src/__tests__/login.test.ts", true),
            1.0
        );
        assert!(
            (path_penalty_for_query("src/compat/php7.php", true) - 0.3).abs() < 1e-6,
            "compat dir should still demote on test queries"
        );
    }

    #[test]
    fn axios_interceptor_failure_mode_repro() {
        // Synthetic axios candidates reproduce tests crowding out the implementation.
        let mut results = vec![
            mk("tests/browser/interceptors.browser.test.js", 0.85),
            mk("tests/smoke/esm/tests/interceptors.smoke.test.js", 0.84),
            mk("tests/smoke/cjs/tests/interceptors.smoke.test.cjs", 0.83),
            mk("lib/core/InterceptorManager.js", 0.65),
            mk("tests/unit/regression.test.js", 0.60),
        ];

        apply_path_penalties(&mut results, "request and response interceptors");

        assert_eq!(results[0].file_path, "lib/core/InterceptorManager.js");
    }

    #[test]
    fn test_query_does_not_regress_test_for_x_case() {
        let mut results = vec![
            mk("tests/browser/interceptors.browser.test.js", 0.85),
            mk("lib/core/InterceptorManager.js", 0.80),
            mk("tests/unit/regression.test.js", 0.60),
        ];

        apply_path_penalties(&mut results, "test for InterceptorManager");

        assert_eq!(
            results[0].file_path,
            "tests/browser/interceptors.browser.test.js"
        );
        assert_eq!(results[1].file_path, "lib/core/InterceptorManager.js");
    }

    #[test]
    fn l2_to_score_clamps_negative_similarity_to_zero() {
        use super::l2_to_score;
        assert_eq!(l2_to_score(2.0), 0.0); // 1 - 4/2 = -1 → 0
        assert_eq!(l2_to_score(1.6), 0.0); // 1 - 2.56/2 = -0.28 → 0
        assert!(l2_to_score(1.41) >= 0.0);
        assert!((l2_to_score(0.0) - 1.0).abs() < 1e-6);
        assert!((l2_to_score(1.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn negative_similarity_does_not_invert_path_penalty_ranking() {
        use super::l2_to_score;
        // Without clamping, a penalty raises a negative score and promotes the worse match.
        let prod_score = l2_to_score(1.5); // better match
        let test_score = l2_to_score(1.6); // worse match
        let mut results = vec![
            mk("src/auth/session.rs", prod_score),
            mk("tests/auth/session_test.rs", test_score),
        ];
        apply_path_penalties(&mut results, "validate session token");
        let prod_idx = results
            .iter()
            .position(|r| r.file_path == "src/auth/session.rs")
            .unwrap();
        let test_idx = results
            .iter()
            .position(|r| r.file_path == "tests/auth/session_test.rs")
            .unwrap();
        assert!(
            prod_idx < test_idx,
            "better-matching production file must not rank below a penalized \
             test file: {results:?}"
        );
    }
}

#[cfg(test)]
mod file_saturation_tests {
    use super::{SearchResult, apply_file_saturation};

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: String::new(),
            start_line: 0,
            end_line: 0,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn single_chunk_per_file_unchanged() {
        let mut rs = vec![mk("a.rs", 1.0), mk("b.rs", 0.9), mk("c.rs", 0.8)];
        apply_file_saturation(&mut rs);
        assert_eq!(rs[0].file_path, "a.rs");
        assert_eq!(rs[0].score, 1.0);
        assert_eq!(rs[1].file_path, "b.rs");
        assert_eq!(rs[1].score, 0.9);
        assert_eq!(rs[2].file_path, "c.rs");
        assert_eq!(rs[2].score, 0.8);
    }

    #[test]
    fn second_chunk_from_same_file_decays_50pct() {
        let mut rs = vec![mk("a.rs", 1.0), mk("a.rs", 0.9), mk("b.rs", 0.7)];
        apply_file_saturation(&mut rs);
        assert_eq!(rs[0].file_path, "a.rs");
        assert_eq!(rs[0].score, 1.0);
        assert_eq!(rs[1].file_path, "b.rs");
        assert_eq!(rs[1].score, 0.7);
        assert_eq!(rs[2].file_path, "a.rs");
        assert!((rs[2].score - 0.45).abs() < 1e-6);
    }

    #[test]
    fn third_chunk_from_same_file_decays_25pct() {
        let mut rs = vec![
            mk("a.rs", 1.0),
            mk("a.rs", 0.9), // → 0.45
            mk("a.rs", 0.8), // → 0.20
            mk("b.rs", 0.5),
        ];
        apply_file_saturation(&mut rs);
        assert_eq!(rs[0].file_path, "a.rs");
        assert_eq!(rs[0].score, 1.0);
        assert_eq!(rs[1].file_path, "b.rs");
        assert_eq!(rs[1].score, 0.5);
        assert_eq!(rs[2].file_path, "a.rs");
        assert!((rs[2].score - 0.45).abs() < 1e-6);
        assert_eq!(rs[3].file_path, "a.rs");
        assert!((rs[3].score - 0.2).abs() < 1e-6);
    }

    #[test]
    fn diversity_promotes_lower_scored_distinct_file() {
        let mut rs = vec![
            mk("a.rs", 1.0),
            mk("a.rs", 0.9),
            mk("a.rs", 0.8),
            mk("b.rs", 0.6),
        ];
        apply_file_saturation(&mut rs);
        assert_eq!(rs[0].file_path, "a.rs");
        assert_eq!(rs[1].file_path, "b.rs");
        assert_eq!(rs[2].file_path, "a.rs");
        assert_eq!(rs[3].file_path, "a.rs");
    }

    #[test]
    fn empty_input_is_noop() {
        let mut rs: Vec<SearchResult> = Vec::new();
        apply_file_saturation(&mut rs);
        assert!(rs.is_empty());
    }
}

#[cfg(test)]
mod symbol_boost_tests {
    use super::{SearchResult, apply_symbol_boost, contains_token};

    fn mk(content: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: "a.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content: content.to_string(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn token_match_requires_word_boundaries() {
        assert!(contains_token("let test = 1;", "test"));
        assert!(contains_token("test", "test"));
        assert!(contains_token("(test)", "test"));
        assert!(!contains_token("latest news", "test"));
        assert!(!contains_token("catalog of logs", "log"));
        assert!(!contains_token("attested", "test"));
        assert!(!contains_token("testing", "test"));
    }

    #[test]
    fn symbol_boost_ignores_substring_inside_longer_identifier() {
        let syms = vec!["test".to_string()];
        let mut hit = vec![mk("fn test() {}", 0.5)];
        apply_symbol_boost(&mut hit, &syms);
        assert!((hit[0].score - 0.6).abs() < 1e-6);

        let mut miss = vec![mk("latest updates", 0.5)];
        apply_symbol_boost(&mut miss, &syms);
        assert!((miss[0].score - 0.5).abs() < 1e-6);
    }
}

#[cfg(test)]
mod definition_boost_tests {
    use super::{
        SearchResult, apply_definition_boost, build_definition_pattern, extract_symbol_name,
        is_symbol_query,
    };

    fn mk(file: &str, content: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: content.to_string(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn is_symbol_query_accepts_camelcase_snake_namespace() {
        assert!(is_symbol_query("FooBar"));
        assert!(is_symbol_query("fooBar"));
        assert!(is_symbol_query("foo_bar"));
        assert!(is_symbol_query("FOO_CONSTANT"));
        assert!(is_symbol_query("Foo::Bar"));
        assert!(is_symbol_query("namespace.method"));
        assert!(is_symbol_query("Sinatra::Base"));
        assert!(is_symbol_query("_private"));
        assert!(is_symbol_query("Foo"));
    }

    #[test]
    fn is_symbol_query_rejects_plain_words_and_phrases() {
        assert!(!is_symbol_query("session"));
        assert!(!is_symbol_query("login"));
        assert!(!is_symbol_query("how does auth work"));
        assert!(!is_symbol_query(""));
        assert!(!is_symbol_query("   "));
        assert!(!is_symbol_query("fix(common): accept zero timestamp"));
    }

    #[test]
    fn extract_symbol_name_strips_namespace_prefix() {
        assert_eq!(extract_symbol_name("Foo::Bar"), "Bar");
        assert_eq!(extract_symbol_name("a.b.c"), "c");
        assert_eq!(extract_symbol_name("Foo\\Bar"), "Bar");
        assert_eq!(extract_symbol_name("ptr->method"), "method");
        assert_eq!(extract_symbol_name("FooBar"), "FooBar");
    }

    #[test]
    fn definition_pattern_matches_rust_struct() {
        let pat = build_definition_pattern("FooBar").unwrap();
        assert!(pat.is_match("pub struct FooBar { x: i32 }"));
        assert!(pat.is_match("    struct FooBar;"));
    }

    #[test]
    fn definition_pattern_matches_python_class_and_def() {
        let pat = build_definition_pattern("FooBar").unwrap();
        assert!(pat.is_match("class FooBar:\n    pass"));
        assert!(pat.is_match("def FooBar(x):\n    return x"));
    }

    #[test]
    fn definition_pattern_matches_namespace_qualified() {
        let pat = build_definition_pattern("Router").unwrap();
        assert!(pat.is_match("defmodule Phoenix.Router do\nend"));
        assert!(pat.is_match("class Foo::Router; end"));
    }

    #[test]
    fn definition_pattern_does_not_match_references() {
        let pat = build_definition_pattern("FooBar").unwrap();
        assert!(!pat.is_match("let x = FooBar::new();"));
        assert!(!pat.is_match("call_something(FooBar)"));
        assert!(!pat.is_match("FooBar.method()"));
        assert!(!pat.is_match("// FooBar is a struct"));
    }

    #[test]
    fn boost_promotes_definition_chunk_over_reference_chunk() {
        let mut rs = vec![
            mk("src/uses.rs", "let x = FooBar::new();", 1.0),
            mk("src/foo_bar.rs", "pub struct FooBar { x: i32 }", 0.5),
        ];
        apply_definition_boost(&mut rs, "FooBar");
        assert_eq!(rs[0].file_path, "src/foo_bar.rs");
        assert!(rs[0].score > rs[1].score);
    }

    #[test]
    fn nl_query_without_embedded_symbol_does_not_trigger_boost() {
        let original_score = 0.5;
        let mut rs = vec![mk(
            "src/foo.rs",
            "pub struct FooBar { x: i32 }",
            original_score,
        )];
        apply_definition_boost(&mut rs, "how does foo work");
        assert_eq!(rs[0].score, original_score);
    }

    #[test]
    fn nl_query_with_embedded_camelcase_triggers_half_strength_boost() {
        let mut rs = vec![
            mk("src/uses.rs", "let x = StateManager::new();", 1.0),
            mk("src/state.rs", "pub struct StateManager { v: u32 }", 0.5),
        ];
        apply_definition_boost(&mut rs, "how does StateManager initialize?");
        assert_eq!(rs[0].file_path, "src/state.rs");
        assert!(rs[0].score > rs[1].score);
    }

    #[test]
    fn embedded_symbol_extraction_skips_acronyms_and_words() {
        use super::extract_embedded_symbols;
        let syms = extract_embedded_symbols("HTTP request and XML parser handle login");
        assert!(syms.is_empty(), "got: {syms:?}");
        let syms = extract_embedded_symbols("how does XmlParser work for HTTP requests");
        assert_eq!(syms, vec!["XmlParser"]);
        let syms = extract_embedded_symbols("call getCurrentUser before isLoggedIn");
        assert_eq!(syms, vec!["getCurrentUser", "isLoggedIn"]);
    }

    #[test]
    fn embedded_path_promotes_definition_files_over_unrelated_chunk() {
        let mut rs = vec![
            mk("src/state.rs", "pub struct StateManager { v: u32 }", 0.4),
            mk(
                "src/login.rs",
                "pub struct LoginController { u: User }",
                0.4,
            ),
            mk("src/other.rs", "fn unrelated() {}", 0.5),
        ];
        apply_definition_boost(&mut rs, "how do StateManager and LoginController interact?");
        assert_eq!(rs[2].file_path, "src/other.rs");
    }

    #[test]
    fn file_stem_bonus_applies_with_underscore_normalization() {
        let mut rs = vec![
            mk(
                "src/login_controller.rs",
                "pub struct LoginController;",
                0.5,
            ),
            mk("src/other.rs", "pub struct LoginController;", 0.5),
        ];
        apply_definition_boost(&mut rs, "LoginController");
        assert_eq!(rs[0].file_path, "src/login_controller.rs");
        assert!(rs[0].score > rs[1].score);
    }

    #[test]
    fn empty_results_is_noop() {
        let mut rs: Vec<SearchResult> = Vec::new();
        apply_definition_boost(&mut rs, "FooBar");
        assert!(rs.is_empty());
    }

    #[test]
    fn elixir_defmodule_keyword_is_recognised() {
        let pat = build_definition_pattern("Router").unwrap();
        assert!(pat.is_match("defmodule Phoenix.Router do"));
    }

    #[test]
    fn kotlin_fun_keyword_is_recognised() {
        let pat = build_definition_pattern("doStuff").unwrap();
        assert!(pat.is_match("    fun doStuff(): Unit { }"));
    }
}

#[cfg(test)]
mod stem_scan_tests {
    use super::apply_non_candidate_stem_scan;
    use codesage_protocol::SearchResult;
    use codesage_storage::Database;

    fn mk(file: &str, content: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: content.to_string(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    fn seed(db: &Database) {
        let zero = vec![0.0f32; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "src/foo_bar.rs",
            "rust",
            &[("pub struct FooBar { x: i32 }", 1, 10, zero.as_slice())],
        )
        .unwrap();
        db.insert_chunks(
            "src/login.rs",
            "rust",
            &[(
                "pub struct LoginController { u: User }",
                1,
                10,
                zero.as_slice(),
            )],
        )
        .unwrap();
        db.insert_chunks(
            "src/uses_foo.rs",
            "rust",
            &[("let x = FooBar::new();", 1, 10, zero.as_slice())],
        )
        .unwrap();
    }

    #[test]
    fn injects_stem_matched_definition_when_not_in_candidates() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        let mut results = vec![mk("src/uses_foo.rs", "let x = FooBar::new();", 0.6)];
        apply_non_candidate_stem_scan(&db, &mut results, "FooBar").unwrap();
        let injected = results
            .iter()
            .find(|r| r.file_path == "src/foo_bar.rs")
            .expect("stem-matched definition should be injected");
        assert!(injected.content.contains("struct FooBar"));
        assert_eq!(injected.score, 0.0);
    }

    #[test]
    fn does_not_inject_when_definition_already_in_candidates() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        let mut results = vec![mk("src/foo_bar.rs", "pub struct FooBar { x: i32 }", 0.7)];
        let before = results.len();
        apply_non_candidate_stem_scan(&db, &mut results, "FooBar").unwrap();
        assert_eq!(results.len(), before);
    }

    #[test]
    fn skips_stem_match_without_definition_keyword() {
        let db = Database::open_in_memory().unwrap();
        let zero = vec![0.0f32; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "src/foo_bar.rs",
            "rust",
            &[("// FooBar is documented elsewhere", 1, 5, zero.as_slice())],
        )
        .unwrap();
        let mut results = vec![mk("src/other.rs", "let x = FooBar::new();", 0.6)];
        apply_non_candidate_stem_scan(&db, &mut results, "FooBar").unwrap();
        assert!(!results.iter().any(|r| r.file_path == "src/foo_bar.rs"));
    }

    #[test]
    fn nl_query_does_not_trigger_scan() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        let mut results = vec![mk("src/uses_foo.rs", "let x = FooBar::new();", 0.6)];
        let before = results.len();
        apply_non_candidate_stem_scan(&db, &mut results, "how does foo work").unwrap();
        assert_eq!(results.len(), before);
    }

    #[test]
    fn short_symbol_does_not_trigger_scan() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        let mut results = vec![mk("src/uses_foo.rs", "use Fb;", 0.6)];
        let before = results.len();
        apply_non_candidate_stem_scan(&db, &mut results, "Fb").unwrap();
        assert_eq!(results.len(), before);
    }

    #[test]
    fn stem_cache_reuses_entry_while_validity_token_is_unchanged() {
        use super::stem_index_from_cache;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let db = Database::open_in_memory().unwrap();
        seed(&db);
        db.upsert_semantic_file_hash("src/foo_bar.rs", "h1")
            .unwrap();

        let cache = Mutex::new(HashMap::new());
        let key = ("/tmp/test.db".to_string(), "chunks_test".to_string());
        let first = stem_index_from_cache(&cache, key.clone(), &db).unwrap();
        let again = stem_index_from_cache(&cache, key, &db).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "unchanged validity token must reuse the cached index"
        );
    }

    #[test]
    fn stem_cache_rebuilds_when_validity_token_changes() {
        use super::stem_index_from_cache;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let db = Database::open_in_memory().unwrap();
        seed(&db);
        db.upsert_semantic_file_hash("src/foo_bar.rs", "h1")
            .unwrap();

        let cache = Mutex::new(HashMap::new());
        let key = ("/tmp/test.db".to_string(), "chunks_test".to_string());
        let first = stem_index_from_cache(&cache, key.clone(), &db).unwrap();
        assert!(!first.by_lower.contains_key("new_thing"));

        let zero = vec![0.0f32; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "src/new_thing.rs",
            "rust",
            &[("pub struct NewThing;", 1, 5, zero.as_slice())],
        )
        .unwrap();
        db.upsert_semantic_file_hash("src/new_thing.rs", "h2")
            .unwrap();

        let rebuilt = stem_index_from_cache(&cache, key, &db).unwrap();
        assert!(
            !Arc::ptr_eq(&first, &rebuilt),
            "a changed validity token must rebuild the index"
        );
        assert!(rebuilt.by_lower.contains_key("new_thing"));
        assert!(rebuilt.by_norm.contains_key("newthing"));
    }
}

#[cfg(test)]
mod adaptive_rerank_tests {
    use super::{
        RERANK_WEIGHT_DEFAULT, RERANK_WEIGHT_NATLANG, RERANK_WEIGHT_SHORT_ID,
        adaptive_rerank_weight,
    };

    #[test]
    fn short_identifier_leans_semantic() {
        assert_eq!(adaptive_rerank_weight("FooBar"), RERANK_WEIGHT_SHORT_ID);
        assert_eq!(
            adaptive_rerank_weight("parse_config"),
            RERANK_WEIGHT_SHORT_ID
        );
        assert_eq!(adaptive_rerank_weight("Middleware"), RERANK_WEIGHT_SHORT_ID);
    }

    #[test]
    fn natural_language_leans_reranker() {
        assert_eq!(
            adaptive_rerank_weight("queue connection resolution and connectors"),
            RERANK_WEIGHT_NATLANG
        );
        assert_eq!(
            adaptive_rerank_weight("where does authentication happen"),
            RERANK_WEIGHT_NATLANG
        );
    }

    #[test]
    fn mixed_short_queries_use_default() {
        assert_eq!(adaptive_rerank_weight("http server"), RERANK_WEIGHT_DEFAULT);
        assert_eq!(
            adaptive_rerank_weight("FooBar BarBaz"),
            RERANK_WEIGHT_DEFAULT
        );
    }

    #[test]
    fn empty_query_uses_default() {
        assert_eq!(adaptive_rerank_weight(""), RERANK_WEIGHT_DEFAULT);
        assert_eq!(adaptive_rerank_weight("   "), RERANK_WEIGHT_DEFAULT);
    }

    #[test]
    fn plain_english_word_is_not_a_short_identifier() {
        assert_eq!(
            adaptive_rerank_weight("authentication"),
            RERANK_WEIGHT_DEFAULT
        );
        assert_eq!(adaptive_rerank_weight("middleware"), RERANK_WEIGHT_DEFAULT);
    }

    #[test]
    fn kebab_snake_and_camel_identifiers_lean_semantic() {
        assert_eq!(adaptive_rerank_weight("foo-bar"), RERANK_WEIGHT_SHORT_ID);
        assert_eq!(
            adaptive_rerank_weight("getUserById"),
            RERANK_WEIGHT_SHORT_ID
        );
        assert_eq!(adaptive_rerank_weight("user_id"), RERANK_WEIGHT_SHORT_ID);
    }
}

#[cfg(test)]
mod rerank_blend_tests {
    use super::{RERANK_WEIGHT_NATLANG, RerankFn, SearchResult, apply_reranking};

    fn mk(file: &str, content: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: content.to_string(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    // Natural-language query (≥3 alphabetic words) → adaptive weight 0.6.
    const QUERY: &str = "where does authentication happen";

    #[test]
    fn blended_ordering_matches_expected_arithmetic() {
        // CE raw scores 0.0 / 10.0 min-max normalize to 0.0 / 1.0. Blend:
        //   a: 0.4 * 0.9 + 0.6 * 0.0 = 0.36
        //   b: 0.4 * 0.5 + 0.6 * 1.0 = 0.80
        // so the cross-encoder flips the semantic order.
        let mut results = vec![mk("a.rs", "doc a", 0.9), mk("b.rs", "doc b", 0.5)];
        let mut rerank: RerankFn = Box::new(|_q, docs| {
            Ok(docs
                .iter()
                .map(|d| if *d == "doc b" { 10.0 } else { 0.0 })
                .collect())
        });
        assert!(apply_reranking(&mut rerank, QUERY, &mut results, None));

        let w = RERANK_WEIGHT_NATLANG;
        assert_eq!(results[0].file_path, "b.rs");
        assert!(
            (results[0].score - ((1.0 - w) * 0.5 + w)).abs() < 1e-6,
            "b: {}",
            results[0].score
        );
        assert_eq!(results[1].file_path, "a.rs");
        assert!(
            (results[1].score - (1.0 - w) * 0.9).abs() < 1e-6,
            "a: {}",
            results[1].score
        );
    }

    #[test]
    fn equal_ce_scores_fall_back_to_half_and_preserve_semantic_order() {
        // Degenerate CE range (all equal) → every ce_norm = 0.5. The blend
        // is then monotone in the semantic score, so ordering is unchanged.
        let mut results = vec![
            mk("a.rs", "doc a", 0.9),
            mk("b.rs", "doc b", 0.5),
            mk("c.rs", "doc c", 0.1),
        ];
        let mut rerank: RerankFn = Box::new(|_q, docs| Ok(vec![3.25; docs.len()]));
        assert!(apply_reranking(&mut rerank, QUERY, &mut results, None));

        let order: Vec<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
        assert_eq!(order, ["a.rs", "b.rs", "c.rs"]);
        let w = RERANK_WEIGHT_NATLANG;
        assert!(
            (results[0].score - ((1.0 - w) * 0.9 + w * 0.5)).abs() < 1e-6,
            "a: {}",
            results[0].score
        );
        assert!(
            (results[2].score - ((1.0 - w) * 0.1 + w * 0.5)).abs() < 1e-6,
            "c: {}",
            results[2].score
        );
    }

    #[test]
    fn rerank_error_leaves_results_untouched() {
        let mut results = vec![mk("a.rs", "doc a", 0.9), mk("b.rs", "doc b", 0.5)];
        let mut rerank: RerankFn = Box::new(|_q, _docs| anyhow::bail!("ORT unavailable"));
        assert!(!apply_reranking(&mut rerank, QUERY, &mut results, None));
        assert_eq!(results[0].file_path, "a.rs");
        assert!((results[0].score - 0.9).abs() < 1e-6);
        assert!((results[1].score - 0.5).abs() < 1e-6);
    }
}

#[cfg(test)]
mod dir_saturation_tests {
    use super::{apply_directory_saturation, apply_qualified_name_boost, qualified_name_matches};
    use codesage_protocol::SearchResult;

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn penalizes_chunks_past_threshold_from_same_directory() {
        let mut results = vec![
            mk("Queue/Connectors/AConnector.php", 0.95),
            mk("Queue/Connectors/BConnector.php", 0.94),
            mk("Queue/Connectors/CConnector.php", 0.93),
            mk("Queue/Connectors/DConnector.php", 0.92),
            mk("Queue/Connectors/EConnector.php", 0.91),
            mk("Queue/QueueManager.php", 0.80),
        ];
        apply_directory_saturation(&mut results);

        let by_path: std::collections::HashMap<_, _> = results
            .iter()
            .map(|r| (r.file_path.clone(), r.score))
            .collect();
        assert!((by_path["Queue/Connectors/AConnector.php"] - 0.95).abs() < 1e-6);
        assert!((by_path["Queue/Connectors/BConnector.php"] - 0.94).abs() < 1e-6);
        assert!(by_path["Queue/Connectors/CConnector.php"] < 0.93);
        assert!(by_path["Queue/Connectors/DConnector.php"] < 0.92);
        assert!(by_path["Queue/Connectors/EConnector.php"] < 0.91);
        assert!((by_path["Queue/QueueManager.php"] - 0.80).abs() < 1e-6);
    }

    #[test]
    fn no_penalty_below_threshold() {
        let mut results = vec![mk("src/a.rs", 0.9), mk("src/b.rs", 0.8)];
        let before: Vec<f32> = results.iter().map(|r| r.score).collect();
        apply_directory_saturation(&mut results);
        let by_path: std::collections::HashMap<_, _> = results
            .iter()
            .map(|r| (r.file_path.clone(), r.score))
            .collect();
        for (i, p) in ["src/a.rs", "src/b.rs"].iter().enumerate() {
            assert!((by_path[*p] - before[i]).abs() < 1e-6);
        }
    }

    #[test]
    fn repo_root_files_bucket_together() {
        let mut results = vec![
            mk("README.md", 0.9),
            mk("Cargo.toml", 0.8),
            mk("Makefile", 0.7),
        ];
        apply_directory_saturation(&mut results);
        assert_eq!(results.len(), 3); // sanity
    }

    fn mk_with_symbols(file: &str, score: f32, symbols: Vec<(&str, &str)>) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: codesage_protocol::Language::Rust,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: symbols
                .into_iter()
                .map(|(name, qn)| codesage_protocol::SymbolSummary {
                    name: name.to_string(),
                    qualified_name: qn.to_string(),
                    kind: codesage_protocol::SymbolKind::Function,
                })
                .collect(),
        }
    }

    #[test]
    fn anti_trigger_leaf_only_match_does_not_boost() {
        assert!(!qualified_name_matches(
            "default",
            "mode::default",
            "default"
        ));
        assert!(!qualified_name_matches("default", "default", "default"));
    }

    #[test]
    fn anti_trigger_root_match_does_boost() {
        assert!(qualified_name_matches("config", "config::load", "load"));
        assert!(qualified_name_matches("default", "default::clone", "clone"));
    }

    #[test]
    fn non_anti_trigger_any_segment_match_boosts() {
        assert!(qualified_name_matches(
            "login",
            "authservice::login",
            "login"
        ));
        assert!(qualified_name_matches(
            "ignore",
            "ignore::add_child_path",
            "add_child_path"
        ));
    }

    #[test]
    fn qualified_name_boost_lifts_matching_chunk() {
        let mut results = vec![
            mk_with_symbols("src/auth.rs", 0.5, vec![("login", "AuthService::login")]),
            mk_with_symbols("src/other.rs", 0.45, vec![("foo", "Other::foo")]),
        ];
        apply_qualified_name_boost(&mut results, &["login".to_string()]);
        assert_eq!(results[0].file_path, "src/auth.rs");
        assert!((results[0].score - 1.0).abs() < 1e-6);
        assert!((results[1].score - 0.45).abs() < 1e-6);
    }

    #[test]
    fn qualified_name_boost_anti_trigger_regression_blocked() {
        let mut results = vec![
            mk_with_symbols("src/correct.rs", 0.80, vec![]),
            mk_with_symbols("src/wrong.rs", 0.77, vec![("default", "Mode::default")]),
        ];
        apply_qualified_name_boost(&mut results, &["default".to_string()]);
        assert_eq!(results[0].file_path, "src/correct.rs");
        assert!((results[1].score - 0.77).abs() < 1e-6);
    }

    #[test]
    fn qualified_name_boost_idempotent_per_chunk() {
        let mut results = vec![mk_with_symbols(
            "src/auth.rs",
            0.5,
            vec![
                ("login", "AuthService::login"),
                ("logout", "AuthService::logout"),
            ],
        )];
        apply_qualified_name_boost(&mut results, &["login".to_string(), "logout".to_string()]);
        assert!((results[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn qualified_name_boost_no_known_symbols_no_op() {
        let mut results = vec![mk_with_symbols(
            "src/auth.rs",
            0.5,
            vec![("login", "AuthService::login")],
        )];
        let before = results[0].score;
        apply_qualified_name_boost(&mut results, &[]);
        assert!((results[0].score - before).abs() < 1e-6);
    }
}

#[cfg(test)]
mod language_and_version_penalty_tests {
    use super::{
        SOFT_PENALTY_MILD, SOFT_PENALTY_MODERATE, apply_foreign_platform_penalties,
        apply_version_demote, declaration_header_penalty, foreign_platform_penalty,
        php_declaration_penalty, query_names_foreign_platform, query_names_version, version_dir_of,
    };
    use codesage_protocol::{Language, SearchResult};

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: Language::TypeScript,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn demotes_declaration_headers_only_in_c_projects() {
        assert_eq!(
            declaration_header_penalty("lib/cfilters.h", Language::C),
            SOFT_PENALTY_MILD
        );
        assert_eq!(
            declaration_header_penalty("lib/connect.c", Language::C),
            1.0
        );
    }

    #[test]
    fn leaves_cpp_headers_alone() {
        assert_eq!(
            declaration_header_penalty("include/nlohmann/json.hpp", Language::Cpp),
            1.0
        );
        assert_eq!(
            declaration_header_penalty("absl/strings/str_split.h", Language::Cpp),
            1.0
        );
    }

    #[test]
    fn exempts_inline_definition_headers() {
        assert_eq!(
            declaration_header_penalty("src/heap-inl.h", Language::C),
            1.0
        );
        assert_eq!(
            declaration_header_penalty("src/queue_inl.h", Language::C),
            1.0
        );
    }

    #[test]
    fn reads_numeric_version_directories() {
        assert_eq!(
            version_dir_of("packages/zod/src/v4/core/schemas.ts"),
            Some(4)
        );
        assert_eq!(version_dir_of("packages/zod/src/v3/types.ts"), Some(3));
        assert_eq!(version_dir_of("src/validate.ts"), None);
        assert_eq!(version_dir_of("src/view/index.ts"), None);
    }

    #[test]
    fn demotes_older_version_trees() {
        let mut results = vec![
            mk("src/v3/types.ts", 1.0),
            mk("src/v4/core/schemas.ts", 0.9),
        ];
        apply_version_demote(&mut results, "how ZodType parses and validates input");
        assert_eq!(results[0].file_path, "src/v4/core/schemas.ts");
        assert!((results[1].score - SOFT_PENALTY_MILD).abs() < 1e-6);
    }

    #[test]
    fn keeps_old_version_when_the_query_asks_for_it() {
        assert!(query_names_version(
            "v3 compatibility error types and ZodError"
        ));
        assert!(!query_names_version(
            "how ZodType parses and validates input"
        ));

        let mut results = vec![
            mk("src/v3/errors.ts", 1.0),
            mk("src/v4/core/errors.ts", 0.9),
        ];
        apply_version_demote(&mut results, "v3 compatibility error types and ZodError");
        assert_eq!(results[0].file_path, "src/v3/errors.ts");
        assert!((results[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn version_demote_is_inert_without_competing_versions() {
        let mut results = vec![mk("src/v4/a.ts", 1.0), mk("src/v4/b.ts", 0.9)];
        apply_version_demote(&mut results, "schema parsing");
        assert!((results[0].score - 1.0).abs() < 1e-6);
        assert!((results[1].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn foreign_platform_guard_matches_whole_tokens() {
        assert!(query_names_foreign_platform(
            "windows named pipe handling",
            false
        ));
        assert!(query_names_foreign_platform("IOCP completion port", false));
        assert!(!query_names_foreign_platform(
            "tty terminal raw mode and window size",
            false
        ));
    }

    #[test]
    fn demotes_foreign_platform_directories() {
        assert_eq!(
            foreign_platform_penalty("src/win/tcp.c", false),
            SOFT_PENALTY_MILD
        );
        assert_eq!(foreign_platform_penalty("src/unix/tcp.c", false), 1.0);
        assert_eq!(foreign_platform_penalty("src/window/tcp.c", false), 1.0);
    }

    #[test]
    fn windows_host_demotes_unix_mirrors_but_preserves_explicit_intent() {
        for directory in ["unix", "posix", "linux", "darwin", "macos", "bsd"] {
            let mut results = vec![
                mk(&format!("src/{directory}/tcp.c"), 1.0),
                mk("src/win/tcp.c", 0.9),
            ];
            apply_foreign_platform_penalties(&mut results, "TCP socket connection", true);
            assert_eq!(results[0].score, SOFT_PENALTY_MILD);
            assert_eq!(results[1].score, 0.9);
            for query in [
                "POSIX socket connection",
                "Linux epoll",
                "pthread creation",
                "portable TCP implementation",
            ] {
                let mut explicit = vec![
                    mk(&format!("src/{directory}/tcp.c"), 1.0),
                    mk("src/win/tcp.c", 0.9),
                ];
                apply_foreign_platform_penalties(&mut explicit, query, true);
                assert_eq!(explicit[0].score, 1.0, "{query}");
            }
        }
        assert_eq!(
            foreign_platform_penalty("src\\unix\\tcp.c", true),
            SOFT_PENALTY_MILD
        );
        assert_eq!(foreign_platform_penalty("src/unixish/tcp.c", true), 1.0);
    }

    #[test]
    fn foreign_only_and_platform_neutral_pages_keep_scores() {
        for (windows_host, directory) in [(false, "win"), (true, "unix")] {
            let mut results = vec![
                mk(&format!("src/{directory}/tcp.c"), 1.0),
                mk("src/common.c", 0.9),
            ];
            apply_foreign_platform_penalties(&mut results, "TCP connections", windows_host);
            assert_eq!(results[0].score, 1.0);
            assert_eq!(results[1].score, 0.9);
        }
    }

    #[test]
    fn php_declarations_demote_only_for_implicit_behavior_queries() {
        for path in [
            "src/ClientInterface.php",
            "src/Contracts/Dispatcher.php",
            "src/Facades/Cache.php",
            "src\\Contracts\\Dispatcher.php",
        ] {
            let mut row = mk(path, 1.0);
            row.language = Language::Php;
            assert_eq!(
                php_declaration_penalty(&row, "how requests execute"),
                SOFT_PENALTY_MODERATE,
                "{path}"
            );
            for query in [
                "interface definition",
                "contracts for requests",
                "facade proxy handling",
                "interfaces",
                "facades",
            ] {
                assert_eq!(php_declaration_penalty(&row, query), 1.0, "{query}");
            }
        }
    }

    #[test]
    fn php_named_files_and_members_remain_legitimate_targets() {
        let mut row = mk("src/Facades/Cache.php", 1.0);
        row.language = Language::Php;
        row.symbols.push(codesage_protocol::SymbolSummary {
            name: "shouldReceive".into(),
            qualified_name: "Cache::shouldReceive".into(),
            kind: codesage_protocol::SymbolKind::Method,
        });
        for query in [
            "Support/Facades/Cache.php",
            "Cache::get",
            "how shouldReceive works",
        ] {
            assert_eq!(php_declaration_penalty(&row, query), 1.0, "{query}");
        }
        row.file_path = "src/ClientInterface.php".into();
        assert_eq!(
            php_declaration_penalty(&row, "GuzzleHttp\\ClientInterface::request"),
            1.0
        );
    }

    #[test]
    fn php_declaration_patterns_do_not_demote_other_languages_or_implementation_paths() {
        for path in ["src/Contracts/Dispatcher.php", "src/ClientInterface.php"] {
            assert_eq!(
                php_declaration_penalty(&mk(path, 1.0), "request handling"),
                1.0
            );
        }
        for path in [
            "src/ContractStore.php",
            "src/Client.php",
            "src/Interfaces/Adapter.php",
            "src/Facades/Cache.js",
        ] {
            let mut row = mk(path, 1.0);
            row.language = Language::Php;
            assert_eq!(
                php_declaration_penalty(&row, "request handling"),
                1.0,
                "{path}"
            );
        }
    }
}

#[cfg(test)]
mod header_demote_scope_tests {
    use super::apply_path_penalties;
    use codesage_protocol::{Language, SearchResult};

    fn mk(file: &str, language: Language, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn header_demote_is_inert_without_a_c_implementation_in_play() {
        // All-.h projects may detect as C; exempt inline headers must not gain a free boost.
        let mut results = vec![
            mk("include/fmt/format-inl.h", Language::C, 0.80),
            mk("include/fmt/compile.h", Language::C, 0.90),
            mk("include/fmt/base.h", Language::C, 0.85),
        ];
        apply_path_penalties(&mut results, "compile-time format string checking");
        assert_eq!(results[0].file_path, "include/fmt/compile.h");
        assert!(
            (results[0].score - 0.90).abs() < 1e-6,
            "no demote should apply"
        );
    }

    #[test]
    fn header_demote_fires_when_a_c_file_competes() {
        let mut results = vec![
            mk("lib/cfilters.h", Language::C, 0.90),
            mk("lib/connect.c", Language::C, 0.85),
        ];
        apply_path_penalties(&mut results, "connection filter chain setup");
        assert_eq!(results[0].file_path, "lib/connect.c");
    }
}

#[cfg(test)]
mod stem_match_boost_tests {
    use super::{STEM_MATCH_BOOST, apply_stem_match_boost, stem_match_tokens};
    use codesage_protocol::{Language, SearchResult};

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: Language::Rust,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn admits_identifier_shaped_tokens() {
        // Leading capitals qualify, so sentence-initial prose can pass this gate.
        assert_eq!(
            stem_match_tokens("Router path_router implementation"),
            vec!["pathrouter", "router"]
        );
        assert_eq!(
            stem_match_tokens("absl::StrSplit and StrJoin"),
            vec!["strjoin", "strsplit"]
        );
        assert_eq!(
            stem_match_tokens("logging macros ABSL_LOG"),
            vec!["absllog"]
        );
    }

    #[test]
    fn rejects_bare_acronyms_and_plain_words() {
        assert!(stem_match_tokens("JSON parser and tokenizer").is_empty());
        assert!(stem_match_tokens("HTTP client request sending").is_empty());
        assert!(stem_match_tokens("how formatters transform log records").is_empty());
        assert!(stem_match_tokens("Foo").is_empty());
    }

    #[test]
    fn boosts_the_file_the_query_names() {
        let mut r = vec![
            mk("tokio/src/sync/batch_semaphore.rs", 0.90),
            mk("tokio/src/sync/semaphore.rs", 0.85),
        ];
        apply_stem_match_boost(&mut r, "Semaphore");
        assert_eq!(r[0].file_path, "tokio/src/sync/semaphore.rs");
        assert!((r[0].score - 0.85 * STEM_MATCH_BOOST).abs() < 1e-6);
    }

    #[test]
    fn normalizes_underscores_so_strsplit_matches_str_split() {
        let mut r = vec![
            mk("absl/strings/str_join.h", 0.90),
            mk("absl/strings/str_split.h", 0.80),
        ];
        apply_stem_match_boost(&mut r, "absl::StrSplit for string splitting");
        assert_eq!(r[0].file_path, "absl/strings/str_split.h");
    }

    #[test]
    fn bounded_boost_cannot_leapfrog_a_clear_winner() {
        // The multiplier bounds score ratios, not positions moved in a clustered ranking.
        let mut r = vec![
            mk("include/nlohmann/adl_serializer.hpp", 1.00),
            mk("include/nlohmann/to_json.hpp", 0.60),
        ];
        apply_stem_match_boost(&mut r, "ADL-based to_json conversion hooks");
        assert_eq!(r[0].file_path, "include/nlohmann/adl_serializer.hpp");
    }
}

#[cfg(test)]
mod stem_match_token_edge_tests {
    use super::stem_match_tokens;

    #[test]
    fn minimum_length_counts_characters_not_bytes() {
        // Äbc has three characters but four UTF-8 bytes.
        assert!(stem_match_tokens("Äbc").is_empty());
        assert_eq!(stem_match_tokens("Äbcd"), vec!["äbcd"]);
    }
}

#[cfg(test)]
mod scoped_fts_evidence_tests {
    use super::{
        bm25_candidates_with_fallback, build_fts_match_query_legacy, build_fts_match_query_mode,
        query_has_rare_literal_with_groups,
    };
    use codesage_storage::Database;

    fn build_fts_match_query(query: &str) -> String {
        build_fts_match_query_mode(query, false)
    }

    #[test]
    fn default_builder_preserves_legacy_expressions() {
        for (query, expected) in [
            ("ModuleRef::create", "\"ModuleRef\""),
            ("fmt::format", ""),
            ("Illuminate\\Routing\\Router", "\"Router\""),
            ("moduleref.create", "\"moduleref\" OR \"create\""),
            (
                "use `doc_cfg` instead of `doc_auto_cfg`",
                "\"doc_cfg\" OR \"doc_auto_cfg\"",
            ),
        ] {
            assert_eq!(build_fts_match_query_legacy(query), expected);
        }
    }

    #[test]
    fn qualified_groups_environment_child() {
        let enabled = std::env::var("CODESAGE_QUALIFIED_GROUPS").is_ok_and(|value| value == "1");
        let query = "Illuminate\\Routing\\Router";
        assert_eq!(
            super::build_fts_match_query(query),
            if enabled {
                build_fts_match_query(query)
            } else {
                build_fts_match_query_legacy(query)
            }
        );
        let db = Database::open_in_memory().unwrap();
        assert_eq!(super::query_has_rare_literal(&db, query).unwrap(), enabled);
    }

    #[test]
    fn qualified_groups_requires_explicit_opt_in() {
        let executable = std::env::current_exe().unwrap();
        for value in [None, Some("0"), Some("true"), Some("1")] {
            let mut command = std::process::Command::new(&executable);
            command.args([
                "--exact",
                "search::scoped_fts_evidence_tests::qualified_groups_environment_child",
            ]);
            command.env_remove("CODESAGE_QUALIFIED_GROUPS");
            if let Some(value) = value {
                command.env("CODESAGE_QUALIFIED_GROUPS", value);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }

    #[test]
    fn qualified_components_remain_in_one_conjunction() {
        for (query, expected) in [
            (
                "absl::StrSplit for splitting",
                "(\"absl\" AND \"StrSplit\")",
            ),
            ("how fmt::format works", "(\"fmt\" AND \"format\")"),
            ("std::vector usage", "(\"std\" AND \"vector\")"),
            ("call ModuleRef::create", "(\"ModuleRef\" AND \"create\")"),
            ("`foo_bar::Thing`", "(\"foo_bar\" AND \"Thing\")"),
            (
                "\\Illuminate\\Routing\\Router()",
                "(\"Illuminate\" AND \"Routing\" AND \"Router\")",
            ),
            ("moduleref.create()", "(\"moduleref\" AND \"create\")"),
            ("foo.bar.baz", "(\"foo\" AND \"bar\" AND \"baz\")"),
            ("foo::bar.baz", "(\"foo\" AND \"bar\" AND \"baz\")"),
            ("Δοκιμή::μέλος", "(\"Δοκιμή\" AND \"μέλος\")"),
            ("a::b", "(\"a\" AND \"b\")"),
        ] {
            assert_eq!(build_fts_match_query(query), expected, "{query}");
        }
    }

    #[test]
    fn standalone_code_identifiers_remain_independent_alternatives() {
        let q = build_fts_match_query("absl::StrSplit and StrJoin for splitting and joining");
        assert_eq!(q, "(\"absl\" AND \"StrSplit\") OR \"StrJoin\"");
    }

    #[test]
    fn scope_queries_preserve_lowercase_components_without_english_glue() {
        for (query, expect) in [
            (
                "absl::StrCat and StrAppend for efficient string",
                "(\"absl\" AND \"StrCat\") OR \"StrAppend\"",
            ),
            (
                "absl::string_view for non-owning string references",
                "(\"absl\" AND \"string_view\")",
            ),
            (
                "absl::flat_hash_map and flat_hash_set hash tables",
                "(\"absl\" AND \"flat_hash_map\") OR \"flat_hash_set\"",
            ),
            (
                "how fmt::format and fmt::print format strings",
                "(\"fmt\" AND \"format\") OR (\"fmt\" AND \"print\")",
            ),
            (
                "std::filesystem path formatting support",
                "(\"std\" AND \"filesystem\")",
            ),
        ] {
            assert_eq!(build_fts_match_query(query), expect, "query {query:?}");
        }
    }

    #[test]
    fn backslash_qualified_names_trigger_the_hybrid_gate() {
        let db = Database::open_in_memory().unwrap();
        assert!(
            query_has_rare_literal_with_groups(&db, "Illuminate\\Routing\\Router", true).unwrap()
        );
        assert!(
            !query_has_rare_literal_with_groups(&db, "Illuminate\\Routing\\Router", false).unwrap()
        );
        assert!(!query_has_rare_literal_with_groups(&db, "e.g. the handler", true).unwrap());
    }

    #[test]
    fn an_explicit_standalone_namespace_remains_an_independent_term() {
        let q = build_fts_match_query("Illuminate\\Routing\\Router and Illuminate helpers");
        assert_eq!(
            q,
            "(\"Illuminate\" AND \"Routing\" AND \"Router\") OR \"Illuminate\""
        );
    }

    #[test]
    fn grouped_hits_exclude_partial_names_and_missing_groups_fall_back() {
        let db = Database::open_in_memory().unwrap();
        let embedding = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        for (path, content) in [
            ("exact.php", "Illuminate Routing Router dispatch"),
            ("other.php", "Unrelated Router dispatch"),
            ("namespace.php", "Illuminate Routing helper"),
        ] {
            db.insert_chunks(path, "php", &[(content, 1, 1, &embedding)])
                .unwrap();
        }
        let query = "Illuminate\\Routing\\Router";
        let rows = bm25_candidates_with_fallback(
            &db,
            &build_fts_match_query(query),
            query,
            10,
            None,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "exact.php");
        let query = "Missing\\Router";
        let rows = bm25_candidates_with_fallback(
            &db,
            &build_fts_match_query(query),
            query,
            10,
            None,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.file_path == "other.php"));
        let rows = bm25_candidates_with_fallback(
            &db,
            &build_fts_match_query(query),
            query,
            10,
            Some(&["rust"]),
            None,
        )
        .unwrap();
        assert!(rows.is_empty());
        let rows = bm25_candidates_with_fallback(
            &db,
            &build_fts_match_query(query),
            query,
            10,
            None,
            Some(&["other.php"]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "other.php");
    }

    #[test]
    fn quotes_and_boolean_words_cannot_change_fts_syntax() {
        let db = Database::open_in_memory().unwrap();
        for query in [
            "\"foo_bar::Thing\" OR \"Another",
            "foo::bar) NOT Baz",
            "foo::bar\"*",
            "",
            "💥::X",
        ] {
            let expression = build_fts_match_query(query);
            if !expression.is_empty() {
                assert!(
                    db.search_bm25(&expression, 10, None, None).is_ok(),
                    "{expression}"
                );
            }
        }
    }

    #[test]
    fn dotted_fallback_preserves_lowercase_legacy_terms() {
        let db = Database::open_in_memory().unwrap();
        let embedding = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "receiver.ts",
            "typescript",
            &[("moduleref resolver", 1, 1, &embedding)],
        )
        .unwrap();
        db.insert_chunks(
            "member.ts",
            "typescript",
            &[("create handler", 1, 1, &embedding)],
        )
        .unwrap();
        let query = "moduleref.create";
        let rows = bm25_candidates_with_fallback(
            &db,
            &build_fts_match_query(query),
            query,
            10,
            None,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
    }
}

#[cfg(test)]
mod mention_anchor_tests {
    use super::{
        MENTION_MAX_CHUNKS_PER_FILE, MENTION_MAX_FILES, MENTION_TOP_GAP_FRAC, QueryMention,
        apply_mention_anchor, apply_offset_and_limit, extract_query_mentions, path_matches_mention,
    };
    use codesage_protocol::{Language, SearchResult, SymbolKind, SymbolSummary};

    const ALL: usize = usize::MAX;
    const PAGE1: usize = 0;

    fn mk(file: &str, score: f32) -> SearchResult {
        SearchResult {
            file_path: file.to_string(),
            language: Language::Rust,
            content: String::new(),
            start_line: 1,
            end_line: 10,
            score,
            symbols: Vec::new(),
        }
    }

    fn mk_with_symbols(file: &str, score: f32, symbols: &[(&str, &str)]) -> SearchResult {
        let mut r = mk(file, score);
        r.symbols = symbols
            .iter()
            .map(|(name, qn)| SymbolSummary {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind: SymbolKind::Function,
            })
            .collect();
        r
    }

    fn rung(top: f32, slot: usize) -> f32 {
        top * (1.0 - MENTION_TOP_GAP_FRAC).powi(slot as i32 + 1)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    fn sym(owner: &str, member: &str, stem_fallback: bool) -> QueryMention {
        QueryMention::Symbol {
            owner: owner.to_string(),
            member: member.to_string(),
            stem_fallback,
        }
    }

    fn path(p: &str) -> QueryMention {
        QueryMention::Path(p.to_string())
    }

    fn ladder_fixture() -> Vec<SearchResult> {
        vec![
            mk("crates/graph/src/index.rs", 0.90),
            mk("crates/graph/src/lookups.rs", 0.80),
            mk("crates/graph/src/impact.rs", 0.70),
            mk("crates/graph/src/drift.rs", 0.60),
            mk("crates/graph/src/scc.rs", 0.50),
            mk("crates/graph/src/brief.rs", 0.40),
            mk("crates/graph/src/search.rs", 0.30),
        ]
    }

    fn snapshot(results: &[SearchResult]) -> String {
        serde_json::to_string(results).unwrap()
    }

    /// Rows at ranks 1..=n (after the untouched top) must sit exactly on
    /// rungs 0..n, in that order.
    fn assert_on_rungs(results: &[SearchResult], top: f32, files: &[&str]) {
        assert!(close(results[0].score, top), "top is untouched");
        for (slot, file) in files.iter().enumerate() {
            let r = &results[slot + 1];
            assert_eq!(r.file_path, *file, "rank {}", slot + 2);
            assert!(
                close(r.score, rung(top, slot)),
                "{file}: {} vs rung {}",
                r.score,
                rung(top, slot)
            );
        }
    }

    #[test]
    fn path_mention_lifts_rank_seven_to_rank_two_on_the_first_rung() {
        let mut r = ladder_fixture();
        apply_mention_anchor(
            &mut r,
            "thread 'main' panicked at crates/graph/src/search.rs",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(&r, 0.90, &["crates/graph/src/search.rs"]);
        assert!(close(r[1].score, 0.855));
    }

    #[test]
    fn rungs_are_relative_so_a_low_top_never_opens_a_cliff() {
        // With an absolute 0.05 step, top 0.24 would put the rung at 0.19, a
        // 20.8% drop that a downstream relevance cliff reads as a boundary.
        let mut r = vec![mk("x/a.rs", 0.24), mk("x/b.rs", 0.10), mk("x/c.rs", 0.09)];
        apply_mention_anchor(&mut r, "see x/c.rs", ALL, PAGE1, true);
        assert_on_rungs(&r, 0.24, &["x/c.rs"]);
        assert!(close(r[1].score, 0.228));
        assert!((r[0].score - r[1].score) / r[0].score < 0.20);
    }

    #[test]
    fn line_and_column_suffixes_are_stripped_from_path_mentions() {
        assert_eq!(
            extract_query_mentions("panicked at src/foo.rs:120"),
            vec![path("src/foo.rs")]
        );
        assert_eq!(
            extract_query_mentions("--> src/foo.rs:120:7"),
            vec![path("src/foo.rs")]
        );
        assert_eq!(
            extract_query_mentions("at ./src/foo.rs:120:7:"),
            vec![path("src/foo.rs")]
        );
        let mut r = vec![
            mk("a/lib.rs", 0.90),
            mk("b/other.rs", 0.80),
            mk("src/foo.rs", 0.20),
        ];
        apply_mention_anchor(&mut r, "error at src/foo.rs:120:7", ALL, PAGE1, true);
        assert_on_rungs(&r, 0.90, &["src/foo.rs"]);
    }

    #[test]
    fn absolute_paths_match_the_repo_relative_index_path() {
        let mut r = ladder_fixture();
        apply_mention_anchor(
            &mut r,
            "thread 'main' panicked at /home/x/repo/crates/graph/src/search.rs:123",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(&r, 0.90, &["crates/graph/src/search.rs"]);

        // Python traceback and Node frame shapes survive the trim.
        assert_eq!(
            extract_query_mentions("File \"/home/x/repo/bench/ablation.py\", line 40, in main"),
            vec![path("/home/x/repo/bench/ablation.py")]
        );
        assert_eq!(
            extract_query_mentions("at foo (/home/x/repo/src/app.js:12:5)"),
            vec![path("/home/x/repo/src/app.js")]
        );
    }

    #[test]
    fn type_method_mention_lifts_the_chunk_carrying_that_qualified_name() {
        let mut r = vec![
            mk("crates/graph/src/search.rs", 0.90),
            mk("crates/graph/src/lookups.rs", 0.80),
            mk_with_symbols(
                "crates/storage/src/db/structural.rs",
                0.20,
                &[("symbol_exists", "Database::symbol_exists")],
            ),
            mk_with_symbols(
                "crates/storage/src/db/mod.rs",
                0.10,
                &[("symbol_exists", "Other::symbol_exists")],
            ),
        ];
        apply_mention_anchor(
            &mut r,
            "why does Database::symbol_exists probe LIMIT 1",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(&r, 0.90, &["crates/storage/src/db/structural.rs"]);
        assert_eq!(r[3].file_path, "crates/storage/src/db/mod.rs");
        assert!(close(r[3].score, 0.10));
    }

    #[test]
    fn file_stem_stands_in_for_the_owner_of_a_scoped_free_function() {
        let mut r = vec![
            mk("crates/graph/src/index.rs", 0.90),
            mk("crates/graph/src/lookups.rs", 0.80),
            mk_with_symbols(
                "crates/graph/src/search.rs",
                0.10,
                &[("apply_symbol_boost", "apply_symbol_boost")],
            ),
        ];
        apply_mention_anchor(
            &mut r,
            "search::apply_symbol_boost double counts",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(&r, 0.90, &["crates/graph/src/search.rs"]);
    }

    #[test]
    fn dotted_form_requires_a_qualified_name_and_never_uses_the_stem() {
        // `request.headers` is as likely prose as a member access, so a
        // `request.js` that merely defines `headers` is not evidence.
        let mut r = vec![
            mk("src/app.js", 0.90),
            mk("src/router.js", 0.80),
            mk_with_symbols("src/request.js", 0.10, &[("headers", "headers")]),
        ];
        let before = snapshot(&r);
        apply_mention_anchor(&mut r, "request.headers is empty", ALL, PAGE1, true);
        assert_eq!(snapshot(&r), before);
        // body is too short to distinguish from a file extension.
        assert!(extract_query_mentions("request.body is undefined").is_empty());
        apply_mention_anchor(&mut r, "request.body is undefined", ALL, PAGE1, true);
        assert_eq!(snapshot(&r), before);
        r[2].symbols[0].qualified_name = "request.headers".to_string();
        apply_mention_anchor(&mut r, "request.headers is empty", ALL, PAGE1, true);
        assert_on_rungs(&r, 0.90, &["src/request.js"]);
    }

    #[test]
    fn at_most_four_mentioned_files_are_lifted() {
        let mut r = vec![
            mk("x/top.rs", 1.00),
            mk("x/a.rs", 0.10),
            mk("x/b.rs", 0.09),
            mk("x/c.rs", 0.08),
            mk("x/d.rs", 0.07),
            mk("x/e.rs", 0.06),
        ];
        apply_mention_anchor(
            &mut r,
            "see x/a.rs x/b.rs x/c.rs x/d.rs x/e.rs",
            ALL,
            PAGE1,
            true,
        );
        let lifted = ["x/a.rs", "x/b.rs", "x/c.rs", "x/d.rs"];
        assert_eq!(lifted.len(), MENTION_MAX_FILES);
        assert_on_rungs(&r, 1.00, &lifted);
        assert_eq!(r[5].file_path, "x/e.rs");
        assert!(close(r[5].score, 0.06), "fifth file is left where it was");
    }

    #[test]
    fn at_most_three_chunks_of_one_mentioned_file_are_lifted() {
        let mut r = vec![mk("x/top.rs", 1.00)];
        for i in 0..5 {
            let mut c = mk("x/many.rs", 0.10 - i as f32 * 0.01);
            c.start_line = 100 * (i as u32 + 1);
            r.push(c);
        }
        apply_mention_anchor(&mut r, "x/many.rs is huge", ALL, PAGE1, true);
        assert_on_rungs(&r, 1.00, &["x/many.rs"; MENTION_MAX_CHUNKS_PER_FILE]);
        assert_eq!(r[1].start_line, 100);
        assert_eq!(r[2].start_line, 200);
        assert_eq!(r[3].start_line, 300);
        assert!(close(r[3].score, 0.857375));
        assert!(close(r[4].score, 0.07), "fourth chunk is left where it was");
        assert!(close(r[5].score, 0.06));
    }

    #[test]
    fn total_anchored_rows_are_capped_so_a_page_keeps_organic_results() {
        // 12 eligible rows exceed the total cap of 5; admission follows mention order.
        let mut r = vec![mk("x/top.rs", 1.00)];
        for f in ["x/a.rs", "x/b.rs", "x/c.rs", "x/d.rs"] {
            for c in 0..3 {
                let mut row = mk(f, 0.30 - (r.len() as f32) * 0.01);
                row.start_line = 100 * (c + 1);
                r.push(row);
            }
        }
        apply_mention_anchor(
            &mut r,
            "compare x/a.rs x/b.rs x/c.rs x/d.rs",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(
            &r,
            1.00,
            &["x/a.rs", "x/a.rs", "x/a.rs", "x/b.rs", "x/b.rs"],
        );
        assert!(r[6].score < rung(1.00, 5), "sixth row is organic");
    }

    #[test]
    fn symbol_hits_share_the_per_file_chunk_cap() {
        let mut r = vec![mk("x/top.rs", 1.00)];
        for i in 0..8 {
            let mut c = mk_with_symbols(
                "src/db.rs",
                0.10 - i as f32 * 0.01,
                &[("open", "Database::open")],
            );
            c.start_line = 100 * (i as u32 + 1);
            r.push(c);
        }
        apply_mention_anchor(&mut r, "Database::open hangs", ALL, PAGE1, true);
        assert_on_rungs(&r, 1.00, &["src/db.rs"; MENTION_MAX_CHUNKS_PER_FILE]);
        assert!(close(r[4].score, 0.07), "fourth chunk is left where it was");
    }

    #[test]
    fn symbol_hits_are_bounded_by_the_total_cap_across_files() {
        // Nine symbol hits exceed the total cap of five.
        let mut r = vec![mk("x/top.rs", 1.00)];
        for f in 0..3 {
            for c in 0..3 {
                let mut row = mk_with_symbols(
                    &format!("src/f{f}.rs"),
                    0.30 - (f * 3 + c) as f32 * 0.01,
                    &[("open", "Database::open")],
                );
                row.start_line = 100 * (c as u32 + 1);
                r.push(row);
            }
        }
        apply_mention_anchor(&mut r, "Database::open hangs", ALL, PAGE1, true);
        assert_on_rungs(
            &r,
            1.00,
            &[
                "src/f0.rs",
                "src/f0.rs",
                "src/f0.rs",
                "src/f1.rs",
                "src/f1.rs",
            ],
        );
        assert!(r[6].score < rung(1.00, 5));
    }

    #[test]
    fn a_bare_basename_is_never_a_mention() {
        assert!(extract_query_mentions("mod.rs is huge").is_empty());
        assert!(extract_query_mentions("index.js and __init__.py and main.go").is_empty());
        let mut r = vec![
            mk("crates/graph/src/lib.rs", 0.90),
            mk("crates/storage/src/db/mod.rs", 0.30),
            mk("crates/cli/src/mcp/mod.rs", 0.20),
        ];
        let before = snapshot(&r);
        apply_mention_anchor(&mut r, "mod.rs is huge", ALL, PAGE1, true);
        assert_eq!(snapshot(&r), before);
        // A leading slash does not smuggle a bare basename back in.
        apply_mention_anchor(&mut r, "/mod.rs is huge", ALL, PAGE1, true);
        assert_eq!(snapshot(&r), before);
        // One directory component makes it specific.
        apply_mention_anchor(&mut r, "mcp/mod.rs is huge", ALL, PAGE1, true);
        assert_on_rungs(&r, 0.90, &["crates/cli/src/mcp/mod.rs"]);
    }

    #[test]
    fn a_suffix_shared_by_several_files_is_non_specific() {
        let mut r = vec![
            mk("crates/graph/src/index.rs", 0.90),
            mk("crates/storage/src/lib.rs", 0.30),
            mk("crates/graph/src/lib.rs", 0.20),
            mk("crates/graph/src/lib.rs", 0.10),
        ];
        let before = snapshot(&r);
        apply_mention_anchor(&mut r, "src/lib.rs re-exports", ALL, PAGE1, true);
        assert_eq!(snapshot(&r), before);
        apply_mention_anchor(
            &mut r,
            "crates/graph/src/lib.rs re-exports",
            ALL,
            PAGE1,
            true,
        );
        assert_on_rungs(&r, 0.90, &["crates/graph/src/lib.rs"; 2]);
        assert_eq!(r[3].file_path, "crates/storage/src/lib.rs");
        assert!(close(r[3].score, 0.30));
    }

    #[test]
    fn slash_prose_and_directories_are_not_mentions() {
        assert!(extract_query_mentions("read/write lock").is_empty());
        assert!(extract_query_mentions("TCP/IP stack").is_empty());
        assert!(extract_query_mentions("24/7 uptime").is_empty());
        assert!(extract_query_mentions("under crates/graph/src").is_empty());
        assert!(extract_query_mentions("under crates/graph/src/").is_empty());
    }

    #[test]
    fn scan_window_bounds_matching_and_the_ladder() {
        // Geometric spacing keeps every neighbour more than one 5% rung
        // apart, so a lifted row lands exactly one place under the top.
        let pool: Vec<SearchResult> = (0..25)
            .map(|i| {
                let score = 0.9_f32.powi(i);
                if i == 19 {
                    mk("x/target.rs", score)
                } else {
                    mk(&format!("x/f{i}.rs"), score)
                }
            })
            .collect();

        let mut narrow = pool.clone();
        apply_mention_anchor(&mut narrow, "x/target.rs panics", 10, PAGE1, true);
        assert_eq!(
            snapshot(&narrow),
            snapshot(&pool),
            "rank 20 is outside a window of 10"
        );

        let mut wide = pool.clone();
        apply_mention_anchor(&mut wide, "x/target.rs panics", 25, PAGE1, true);
        assert_on_rungs(&wide, 1.0, &["x/target.rs"]);
    }

    #[test]
    fn stage_is_inert_on_every_page_but_the_first() {
        let pool: Vec<SearchResult> = (0..25)
            .map(|i| {
                let score = 0.9_f32.powi(i);
                if i == 19 {
                    mk("x/target.rs", score)
                } else {
                    mk(&format!("x/f{i}.rs"), score)
                }
            })
            .collect();
        let mut page2 = pool.clone();
        apply_mention_anchor(&mut page2, "x/target.rs panics", ALL, 10, true);
        assert_eq!(snapshot(&page2), snapshot(&pool));
        apply_offset_and_limit(&mut page2, 10, 10);
        let organic: Vec<String> = (10..20).map(|i| format!("x/f{i}.rs")).collect();
        let mut expected = organic.clone();
        expected[9] = "x/target.rs".to_string();
        let got: Vec<String> = page2.iter().map(|r| r.file_path.clone()).collect();
        assert_eq!(
            got, expected,
            "page 2 is a plain slice of the organic ranking"
        );

        let mut page1 = pool.clone();
        apply_mention_anchor(&mut page1, "x/target.rs panics", ALL, PAGE1, true);
        assert_on_rungs(&page1, 1.0, &["x/target.rs"]);
    }

    #[test]
    fn top_is_the_window_maximum_even_for_unsorted_input() {
        let mut r = vec![mk("x/a.rs", 0.50), mk("x/b.rs", 0.90), mk("c/t.rs", 0.10)];
        apply_mention_anchor(&mut r, "c/t.rs", ALL, PAGE1, true);
        let t = r.iter().find(|x| x.file_path == "c/t.rs").unwrap();
        assert!(close(t.score, rung(0.90, 0)));
    }

    #[test]
    fn a_mentioned_chunk_already_on_top_is_never_demoted() {
        let mut r = vec![
            mk("crates/graph/src/search.rs", 0.90),
            mk("crates/graph/src/index.rs", 0.89),
            mk("crates/graph/src/lookups.rs", 0.10),
        ];
        let before = snapshot(&r);
        apply_mention_anchor(
            &mut r,
            "crates/graph/src/search.rs is slow",
            ALL,
            PAGE1,
            true,
        );
        assert_eq!(snapshot(&r), before);
    }

    #[test]
    fn mention_free_query_is_byte_identical() {
        let mut r = ladder_fixture();
        let before = snapshot(&r);
        apply_mention_anchor(
            &mut r,
            "how does the search pipeline blend reranker scores with symbol boosts",
            ALL,
            PAGE1,
            true,
        );
        assert_eq!(snapshot(&r), before);
    }

    #[test]
    fn disabled_gate_is_inert_even_with_a_mention() {
        let mut r = ladder_fixture();
        let before = snapshot(&r);
        apply_mention_anchor(
            &mut r,
            "panicked at crates/graph/src/search.rs:123",
            ALL,
            PAGE1,
            false,
        );
        assert_eq!(snapshot(&r), before);
    }

    #[test]
    fn bare_identifiers_and_prose_are_not_mentions() {
        assert!(extract_query_mentions("apply_symbol_boost SearchResult reranker").is_empty());
        assert!(extract_query_mentions("e.g. the daemon, i.e. v0.26.1, costs 1.5 GB").is_empty());
        assert_eq!(
            extract_query_mentions("`Database::open()` and ns\\Repo then Foo.barBaz,"),
            vec![
                sym("Database", "open", true),
                sym("ns", "Repo", true),
                sym("Foo", "barBaz", false),
            ]
        );
    }

    #[test]
    fn file_like_tokens_never_become_symbol_mentions() {
        assert!(extract_query_mentions("bump Cargo.toml").is_empty());
        assert!(extract_query_mentions("update README.md").is_empty());
        assert!(extract_query_mentions("port Foo.kt").is_empty());
        assert!(extract_query_mentions("crash in parser.cc").is_empty());
    }

    #[test]
    fn parser_language_table_drives_the_path_extension_set() {
        for tok in [
            "x/a.mts", "x/b.pyi", "x/c.cu", "x/d.cxx", "x/e.go", "x/f.java", "x/g.php", "x/h.cc",
        ] {
            assert_eq!(extract_query_mentions(tok), vec![path(tok)], "{tok}");
        }
        assert!(extract_query_mentions("x/h.rb").is_empty());
        assert!(extract_query_mentions("x/notes.txt").is_empty());
    }

    #[test]
    fn windows_paths_are_rejected_rather_than_emitted() {
        assert!(extract_query_mentions(r"C:\proj\src\foo.rs:12").is_empty());
        assert!(extract_query_mentions(r"src\foo.rs").is_empty());
        assert!(extract_query_mentions(r"..\lib\bar.py").is_empty());
        // PHP namespaces keep working: both segments are identifiers.
        assert_eq!(
            extract_query_mentions(r"App\Http\Kernel"),
            vec![sym("Http", "Kernel", true)]
        );
    }

    #[test]
    fn path_suffix_match_respects_component_boundaries_and_case() {
        let indexed = "crates/graph/src/search.rs";
        assert!(path_matches_mention(indexed, "src/search.rs"));
        assert!(path_matches_mention(indexed, "crates/graph/src/search.rs"));
        assert!(path_matches_mention(
            indexed,
            "./crates/graph/src/search.rs"
        ));
        assert!(path_matches_mention(
            indexed,
            "/home/x/repo/crates/graph/src/search.rs"
        ));
        assert!(!path_matches_mention(indexed, "earch.rs"));
        assert!(!path_matches_mention(indexed, "Search.rs"));
        assert!(!path_matches_mention(indexed, "graph/search.rs"));
        assert!(!path_matches_mention(indexed, "search.rs"));
        assert!(!path_matches_mention(indexed, "/search.rs"));
        assert!(!path_matches_mention(indexed, "./search.rs"));
        assert!(!path_matches_mention(
            indexed,
            "/home/x/other/src/search.rs"
        ));
        assert!(!path_matches_mention("lib.rs", "/x/lib.rs"));
        assert!(path_matches_mention("src/lib.rs", "/x/src/lib.rs"));
    }
}

#[cfg(test)]
mod mention_anchor_pipeline_tests {
    use super::{MENTION_TOP_GAP_FRAC, search};
    use codesage_protocol::SearchRequest;
    use codesage_storage::Database;

    fn mk_embedding(v: f32) -> Vec<f32> {
        let mut e = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        for slot in e.iter_mut().take(10) {
            *slot = v;
        }
        e
    }

    fn seed(db: &Database) {
        for (path, text, v) in [
            ("src/lib.rs", "fn auth() { }", 0.1),
            // Spaced so the runner-up scores under the first 5% rung
            // (l2 0.1→0.3 over ten dims is score 0.80 against top 1.00).
            ("src/db.rs", "fn connect() { }", 0.3),
            ("src/reg.rs", "fn register() { }", 0.5),
            ("src/misc.rs", "fn handler() { }", 0.7),
        ] {
            db.insert_chunks(path, "rust", &[(text, 1, 5, mk_embedding(v).as_slice())])
                .unwrap();
        }
    }

    fn req(query: &str, limit: usize, offset: usize) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            limit: Some(limit),
            offset: Some(offset),
            languages: None,
            paths: None,
            adaptive_limit: false,
        }
    }

    #[test]
    fn search_lifts_the_named_file_under_the_semantic_top_on_page_one_only() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        let emb = mk_embedding(0.1);

        let baseline = search(&db, &emb, None, &req("handler panics", 10, 0)).unwrap();
        assert_eq!(baseline[0].file_path, "src/lib.rs");
        assert_eq!(
            baseline[3].file_path, "src/misc.rs",
            "farthest by embedding"
        );

        let query = "thread 'main' panicked at /home/x/repo/src/misc.rs:5";
        let page1 = search(&db, &emb, None, &req(query, 10, 0)).unwrap();
        assert_eq!(page1[0].file_path, "src/lib.rs");
        assert_eq!(page1[1].file_path, "src/misc.rs");
        let expected = page1[0].score * (1.0 - MENTION_TOP_GAP_FRAC);
        assert!((page1[1].score - expected).abs() < 1e-6);

        let page2 = search(&db, &emb, None, &req(query, 2, 2)).unwrap();
        let files: Vec<&str> = page2.iter().map(|r| r.file_path.as_str()).collect();
        assert_eq!(files, vec!["src/reg.rs", "src/misc.rs"]);
    }
}
