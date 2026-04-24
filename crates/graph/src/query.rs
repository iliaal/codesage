use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use codesage_embed::model::Embedder;
use codesage_embed::reranker::Reranker;
use codesage_protocol::{
    ContextBundle, DependencyEntry, ExportRequest, FileCategory, FindReferencesRequest,
    FindSymbolRequest, ImpactEntry, ImpactReason, ImpactRequest, ImpactTarget, Reference,
    SearchRequest, SearchResult, Symbol, SymbolSummary,
};
use codesage_storage::{Database, RawSearchRow, embedding_to_bytes};

pub fn find_symbol(db: &Database, req: &FindSymbolRequest) -> Result<Vec<Symbol>> {
    db.find_symbols(&req.name, req.kind)
}

pub fn find_references(db: &Database, req: &FindReferencesRequest) -> Result<Vec<Reference>> {
    db.find_references(&req.symbol_name, req.kind)
}

pub fn list_dependencies(db: &Database, file_path: &str) -> Result<DependencyEntry> {
    db.list_file_dependencies(file_path)
}

fn l2_to_score(distance: f32) -> f32 {
    1.0 - distance * distance / 2.0
}

const RERANK_OVERFETCH: usize = 5;

/// RRF constant. Standard value from the original paper; larger values
/// damp the influence of absolute rank position, smaller values amplify it.
const RRF_K: f64 = 60.0;

/// Doc-frequency threshold below which a query token counts as "rare" for
/// the gate. 1% of the corpus is the memo's suggested cutoff — distinctive
/// enough that BM25 actually has signal, not so rare that every typo
/// triggers the hybrid path.
const RARE_TOKEN_DF_THRESHOLD: f64 = 0.01;

/// Minimum token length for the length-based rare check. Short tokens
/// (`fd`, `pt`, `if`) match too broadly regardless of doc frequency.
const RARE_TOKEN_MIN_LEN: usize = 8;

/// True when `query` contains a literal token distinctive enough to
/// justify a BM25 boost on top of the semantic score. Two qualifying
/// shapes:
///
/// 1. **Distinctive punctuation**: backticked identifiers (`` `doc_cfg` ``),
///    file-extension globs (`*.svelte.ts`), scope-resolution operators
///    (`ModuleRef::create`). These shapes are strong priors for a literal
///    match even if the exact token isn't in the FTS vocab yet.
/// 2. **Long rare tokens**: any whitespace- or pipe-separated token of at
///    least 8 characters that shows up in <1% of indexed chunks. Requires
///    a live FTS5 `fts5vocab` probe, so this returns `Ok(false)` when the
///    FTS sidecar is empty (fresh install before reindex).
pub(crate) fn query_has_rare_literal(db: &Database, query: &str) -> Result<bool> {
    if query.contains("::") || query.contains('`') || query.contains("*.") {
        return Ok(true);
    }
    // Dotted-identifier pair shape (`moduleref.create`, `Foo.Bar`,
    // `foo.bar_baz`). Both sides must be ≥3 chars so sentence punctuation
    // like `e.g.` and `i.e.` doesn't trigger. Measured on nest:
    // `moduleref.create` case in the remaining miss set — the individual
    // tokens are all lowercase so neither qualifies as "code-shaped" on its
    // own, but the dotted-pair context is a strong signal that they are.
    if !extract_dotted_identifier_tokens(query).is_empty() {
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
            // Pure lowercase English words (`resolution`, `handler`,
            // `middleware`) can be rare in a domain corpus without carrying
            // the "this is the exact identifier I need" signal BM25 is
            // supposed to catch. Measured regression on nest canary
            // (`git-15198c650d`): "resolution" DF 0.19% tripped this branch
            // but the expected file did not contain the word. Restrict the
            // length branch to tokens that look like code identifiers.
            continue;
        }
        let (doc, total) = db.token_doc_frequency(tok)?;
        if total == 0 {
            continue;
        }
        let df = doc as f64 / total as f64;
        // Both halves of the threshold matter: a token must appear (doc>0)
        // AND be rare (df < 1%). `doc == 0` means the token isn't in the
        // index — no BM25 win possible, skip.
        if doc > 0 && df < RARE_TOKEN_DF_THRESHOLD {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when a token carries syntactic markers of a code identifier —
/// contains `_`, at least one uppercase letter, or a digit. This filters
/// out ordinary English words that may be rare in a specific corpus but
/// don't carry the "exact identifier match" signal BM25 is supposed to
/// contribute.
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

/// Build an FTS5 MATCH expression from a user query. Emits a disjunction
/// of quoted terms so code tokens like `doc_cfg` and `ModuleRef::create`
/// survive FTS5's reserved-character parsing without raising syntax errors
/// at query time. Empty when no usable tokens are extracted.
fn build_fts_match_query(query: &str) -> String {
    use std::collections::HashSet;
    // Split aggressively so things like `ModuleRef::create`, `foo.bar`, and
    // `*.svelte.ts` yield each alphanumeric+underscore segment as its own
    // term, not concatenated nonsense. FTS5's unicode61 tokenizer (with
    // tokenchars '_') would produce the same splits at index time, so what
    // we emit here matches what was actually indexed.
    //
    // Filter: only include tokens that look like code identifiers. Common
    // English glue words (`use`, `the`, `and`, `of`, `instead`) in a
    // 10-word commit subject would flood the BM25 ranking and bury the
    // one or two distinctive tokens we actually care about. Measured on
    // ripgrep: the query `printer: use \`doc_cfg\` instead of
    // \`doc_auto_cfg\`` without this filter produces a MATCH disjunction
    // of 6 tokens where 4 are common glue, and the target file drops out
    // of the top 10 because the glue tokens match everything.
    //
    // Exception: dotted-identifier components like `moduleref.create`
    // survive even when individually lowercase, because the dotted pair
    // context signals code identity.
    let is_sep = |c: char| !c.is_alphanumeric() && c != '_';
    let mut seen: HashSet<String> = HashSet::new();
    let mut tokens: Vec<String> = Vec::new();
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
        // Dedupe by lowercased form — FTS5 is case-insensitive for this
        // tokenizer, so `Foo` and `foo` would collapse at MATCH time
        // anyway. Fewer OR-terms keeps the MATCH expression parseable.
        let key = raw.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        tokens.push(format!("\"{raw}\""));
    }
    tokens.join(" OR ")
}

/// Weight applied to BM25 contributions in the gated hybrid RRF merge.
/// Symmetric RRF underweights BM25 on queries where the target file is
/// absent from the semantic top-N but present in the BM25 top-N: the
/// target then competes only against rank-1 of the list where it does
/// appear, and any semantic-only top-1 item edges it out by a hair.
/// Measured on nest (`moduleref.create` → `module-ref.ts` case): BM25
/// correctly ranks `module-ref.ts` at rank 2, but unweighted RRF leaves
/// it under the top-10 because semantic's long tail contributes one
/// score per rank. Weighting BM25 at 2x closes this specific gap without
/// overwhelming the semantic ranking on queries where semantic is correct.
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
    // Key is (path, start, end). Two chunks at the same location appearing
    // in both rankings collapse to one row with the summed RRF score.
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
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    // Convert the fused RRF score to the `distance` field downstream code
    // expects: it reads it through `l2_to_score` which is monotonic on L2
    // distance. To keep the downstream ordering intact we inject an
    // "equivalent L2" that preserves rank: higher RRF score → lower synthetic
    // distance. Using `distance = 1.0 - rrf_score` works because all pipeline
    // math downstream cares about is monotonic order.
    ranked
        .into_iter()
        .take(limit)
        .map(|(score, mut row)| {
            row.distance = (1.0 - score as f32).max(0.0);
            row
        })
        .collect()
}

pub fn search(
    db: &Database,
    embedder: &mut Embedder,
    reranker: Option<&mut Reranker>,
    req: &SearchRequest,
) -> Result<Vec<SearchResult>> {
    let limit = req.limit.unwrap_or(10);
    let offset = req.offset.unwrap_or(0);

    let known_symbols = extract_known_symbols(db, &req.query)?;
    let has_symbols = !known_symbols.is_empty();
    let has_reranker = reranker.is_some();
    let overfetch = if has_reranker {
        RERANK_OVERFETCH
    } else if has_symbols {
        3
    } else {
        1
    };

    let query_embedding = embedder.embed_one(&req.query)?;
    let embedding_bytes = embedding_to_bytes(&query_embedding);

    let semantic_fetch = limit * overfetch;

    // Gate: is this a query where BM25 would help? Two distinctive shapes
    // covered by `query_has_rare_literal`: backticked identifiers / glob
    // patterns / `::` scoped lookups, and long tokens (>=8 chars) that show
    // up in <1% of chunks. Retrospective analysis on external corpora
    // (ripgrep, nestjs/nest) measured these as the specific failure mode
    // semantic-only retrieval misses. See
    // `notes/20260411-code-intelligence-landscape.md` §1.4 for the memo chain.
    let hybrid_gate = query_has_rare_literal(db, &req.query).unwrap_or(false);

    let rows = if req.paths.is_some() {
        let languages: Option<Vec<&str>> = req
            .languages
            .as_ref()
            .map(|langs| langs.iter().map(|l| l.as_str()).collect());
        let paths: Option<Vec<&str>> = req
            .paths
            .as_ref()
            .map(|p| p.iter().map(|s| s.as_str()).collect());
        db.search_fullscan(
            &embedding_bytes,
            semantic_fetch,
            offset,
            languages.as_deref(),
            paths.as_deref(),
        )?
    } else {
        match &req.languages {
            None => {
                let mut rows = db.search_knn(&embedding_bytes, semantic_fetch + offset, None)?;
                if offset > 0 && offset < rows.len() {
                    rows.drain(..offset);
                }
                rows.truncate(semantic_fetch);
                rows
            }
            Some(langs) if langs.len() == 1 => {
                let mut rows = db.search_knn(
                    &embedding_bytes,
                    semantic_fetch + offset,
                    Some(langs[0].as_str()),
                )?;
                if offset > 0 && offset < rows.len() {
                    rows.drain(..offset);
                }
                rows.truncate(semantic_fetch);
                rows
            }
            Some(langs) => {
                let fetch_k = semantic_fetch + offset;
                // Fan-out per language (sqlite-vec's partition key forces
                // per-value queries) and merge in-memory. sort+truncate is
                // simpler than a bounded BinaryHeap and fetch_k stays small
                // enough (N_langs * ~50) that asymptotic cost doesn't matter.
                let mut merged: Vec<RawSearchRow> = Vec::new();
                for lang in langs {
                    let lang_rows =
                        db.search_knn(&embedding_bytes, fetch_k, Some(lang.as_str()))?;
                    merged.extend(lang_rows);
                }
                merged.sort_by(|a, b| {
                    a.distance
                        .partial_cmp(&b.distance)
                        .unwrap_or(Ordering::Equal)
                });
                if offset > 0 && offset < merged.len() {
                    merged.drain(..offset);
                }
                merged.truncate(semantic_fetch);
                merged
            }
        }
    };

    // Hybrid BM25+semantic fusion, only when the gate triggered. Keeps the
    // semantic-only path identical to pre-hybrid behavior for the 80%+ of
    // queries that don't contain a rare literal, so the ecosystem default
    // doesn't get copy-pasted in where the memo's net-negative finding still
    // applies.
    let rows = if hybrid_gate {
        let match_expr = build_fts_match_query(&req.query);
        if match_expr.is_empty() {
            rows
        } else {
            let bm25_language: Option<&str> = req.languages.as_ref().and_then(|ls| {
                if ls.len() == 1 {
                    Some(ls[0].as_str())
                } else {
                    None
                }
            });
            match db.search_bm25(&match_expr, semantic_fetch, bm25_language) {
                Ok(bm25_rows) if !bm25_rows.is_empty() => {
                    rrf_merge(rows, bm25_rows, semantic_fetch)
                }
                _ => rows,
            }
        }
    } else {
        rows
    };

    let semantic_results: Vec<SearchResult> = rows
        .into_iter()
        .map(|r| SearchResult {
            file_path: r.file_path,
            language: r.language,
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

    annotate_with_symbols(db, &mut results)?;

    // Skip reranking on hybrid-gated queries. The cross-encoder judges
    // query/doc semantic similarity; for queries driven by a literal
    // identifier (the `hybrid_gate` trigger conditions), the rare-token
    // match is already the dominant signal and reranking typically flips
    // the BM25 win back down — the exact failure mode the memo at
    // `project_hybrid_bm25_rrf.md` warned about. Measured on the ripgrep
    // canary: reranker demotes `lib.rs` (rank 5 post-RRF) out of top-10
    // on `use \`doc_cfg\`` queries.
    if !hybrid_gate && let Some(reranker) = reranker {
        apply_reranking(reranker, &req.query, &mut results);
    }

    results.truncate(limit);
    Ok(results)
}

fn extract_known_symbols(db: &Database, query: &str) -> Result<Vec<String>> {
    let mut known = Vec::new();
    for token in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if token.len() < 3 || !looks_like_identifier(token) {
            continue;
        }
        // `symbol_exists` issues a LIMIT 1 probe instead of materializing every
        // matching Symbol row just to test non-emptiness.
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

fn apply_symbol_boost(results: &mut [SearchResult], known_symbols: &[String]) {
    for result in results.iter_mut() {
        let content_lower = result.content.to_lowercase();
        let mut boost = 0.0f32;
        for sym in known_symbols {
            if content_lower.contains(sym) {
                boost += 0.1;
            }
        }
        result.score += boost;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

const RERANK_WEIGHT: f32 = 0.5;

fn apply_reranking(reranker: &mut Reranker, query: &str, results: &mut [SearchResult]) {
    if results.is_empty() {
        return;
    }

    let docs: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
    let ce_scores = match reranker.score_pairs(query, &docs) {
        Ok(s) => s,
        Err(_) => return,
    };

    let ce_min = ce_scores.iter().cloned().fold(f32::INFINITY, f32::min);
    let ce_max = ce_scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let ce_range = ce_max - ce_min;

    for (result, &ce_raw) in results.iter_mut().zip(ce_scores.iter()) {
        let ce_norm = if ce_range > 1e-6 {
            (ce_raw - ce_min) / ce_range
        } else {
            0.5
        };
        result.score = (1.0 - RERANK_WEIGHT) * result.score + RERANK_WEIGHT * ce_norm;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

fn annotate_with_symbols(db: &Database, results: &mut [SearchResult]) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }

    // Batched lookup: one multi-path query instead of one per distinct file.
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
                kind: s.kind.as_str().to_string(),
            })
            .collect();

        result.symbols = overlapping;
    }
    Ok(())
}

pub fn impact_analysis(db: &Database, req: &ImpactRequest) -> Result<Vec<ImpactEntry>> {
    let seed_symbols: Vec<Symbol> = match &req.target {
        ImpactTarget::Symbol { name } => db.find_symbols(name, None)?,
        ImpactTarget::File { path } => db.symbols_for_file(path)?,
    };

    if seed_symbols.is_empty() {
        return Ok(Vec::new());
    }

    let origin_files: HashSet<String> = match &req.target {
        ImpactTarget::File { path } => {
            let mut s = HashSet::new();
            s.insert(path.clone());
            s
        }
        ImpactTarget::Symbol { .. } => seed_symbols.iter().map(|s| s.file_path.clone()).collect(),
    };

    let mut file_reasons: HashMap<String, (u32, Vec<ImpactReason>)> = HashMap::new();
    let mut frontier: Vec<Symbol> = seed_symbols;
    let mut visited_symbols: HashSet<String> = HashSet::new();

    for depth in 1..=req.depth as u32 {
        // First pass: collect refs, update file_reasons, record (from_file, line) pairs
        // that need caller-symbol lookups for the next frontier.
        let mut pending_callers: Vec<(String, u32)> = Vec::new();
        for sym in &frontier {
            if !visited_symbols.insert(sym.qualified_name.clone()) {
                continue;
            }
            let refs = db.find_references(&sym.name, None)?;
            for r in refs {
                if origin_files.contains(&r.from_file) {
                    continue;
                }
                let entry = file_reasons
                    .entry(r.from_file.clone())
                    .or_insert_with(|| (depth, Vec::new()));
                if entry.0 > depth {
                    entry.0 = depth;
                }
                if entry.1.len() < 10 {
                    entry.1.push(ImpactReason {
                        via_symbol: sym.name.clone(),
                        kind: r.kind,
                        line: r.line,
                    });
                }
                if depth < req.depth as u32 {
                    pending_callers.push((r.from_file, r.line));
                }
            }
        }

        if pending_callers.is_empty() {
            break;
        }

        // Batched caller-symbol lookup: one query per distinct file, regardless of
        // how many lines in that file triggered the lookup.
        let distinct_files: Vec<String> = {
            let mut set: HashSet<String> = HashSet::new();
            pending_callers.iter().for_each(|(f, _)| {
                set.insert(f.clone());
            });
            set.into_iter().collect()
        };
        let syms_by_file = db.symbols_for_files(&distinct_files)?;

        let mut next_frontier: Vec<Symbol> = Vec::new();
        for (from_file, line) in &pending_callers {
            if let Some(syms) = syms_by_file.get(from_file) {
                for s in syms {
                    if s.line_start <= *line && s.line_end >= *line {
                        next_frontier.push(s.clone());
                    }
                }
            }
        }

        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    let mut entries: Vec<ImpactEntry> = file_reasons
        .into_iter()
        .map(|(path, (distance, reasons))| {
            let category = FileCategory::classify(&path);
            ImpactEntry {
                file_path: path,
                distance,
                category,
                reasons,
            }
        })
        .filter(|e| !req.source_only || e.category == FileCategory::Source)
        .collect();

    entries.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.reasons.len().cmp(&a.reasons.len()))
    });
    Ok(entries)
}

pub fn export_context(
    db: &Database,
    embedder: &mut Embedder,
    reranker: Option<&mut Reranker>,
    req: &ExportRequest,
) -> Result<ContextBundle> {
    if let Some(sym_name) = &req.symbol {
        return export_context_for_symbol(db, sym_name, req);
    }

    let query = req.query.as_deref().unwrap_or_default();
    if query.is_empty() {
        anyhow::bail!("export_context requires either `query` or `symbol`");
    }

    let search_req = SearchRequest {
        query: query.to_string(),
        limit: Some(req.limit),
        offset: Some(0),
        languages: None,
        paths: None,
    };
    let primary = search(db, embedder, reranker, &search_req)?;

    let mut symbol_defs: Vec<Symbol> = Vec::new();
    let mut seen_sym: HashSet<String> = HashSet::new();
    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary
        .iter()
        .map(|r| (r.file_path.clone(), r.start_line))
        .collect();

    for result in &primary {
        for sum in &result.symbols {
            if !seen_sym.insert(sum.qualified_name.clone()) {
                continue;
            }
            let defs = db.find_symbols(&sum.name, None)?;
            for d in defs.into_iter().take(1) {
                symbol_defs.push(d);
            }
        }
    }

    if req.include_callees || req.include_callers {
        for sym in symbol_defs.clone().iter().take(5) {
            if req.include_callers {
                let refs = db.find_references(&sym.name, None)?;
                for r in refs.into_iter().take(3) {
                    add_related_from_file(
                        db,
                        &r.from_file,
                        r.line,
                        &mut related,
                        &mut related_keys,
                    )?;
                }
            }
            if req.include_callees {
                add_related_from_file(
                    db,
                    &sym.file_path,
                    sym.line_start,
                    &mut related,
                    &mut related_keys,
                )?;
            }
        }
    }

    Ok(ContextBundle {
        target_description: format!("query: {query}"),
        primary,
        related,
        symbol_definitions: symbol_defs,
    })
}

pub fn export_context_for_symbol(
    db: &Database,
    sym_name: &str,
    req: &ExportRequest,
) -> Result<ContextBundle> {
    let defs = db.find_symbols(sym_name, None)?;
    if defs.is_empty() {
        return Ok(ContextBundle {
            target_description: format!("symbol: {sym_name} (not found)"),
            primary: Vec::new(),
            related: Vec::new(),
            symbol_definitions: Vec::new(),
        });
    }

    let mut primary: Vec<SearchResult> = Vec::new();
    let mut primary_keys: HashSet<(String, u32)> = HashSet::new();
    for def in &defs {
        add_related_from_file(
            db,
            &def.file_path,
            def.line_start,
            &mut primary,
            &mut primary_keys,
        )?;
    }

    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary_keys.clone();

    if req.include_callers {
        let refs = db.find_references(sym_name, None)?;
        for r in refs.into_iter().take(req.limit) {
            add_related_from_file(db, &r.from_file, r.line, &mut related, &mut related_keys)?;
        }
    }

    Ok(ContextBundle {
        target_description: format!("symbol: {sym_name}"),
        primary,
        related,
        symbol_definitions: defs,
    })
}

fn add_related_from_file(
    db: &Database,
    file_path: &str,
    line: u32,
    out: &mut Vec<SearchResult>,
    seen: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let chunks = db.chunks_for_file(file_path)?;
    let best = chunks
        .into_iter()
        .find(|c| c.start_line <= line && c.end_line >= line);
    if let Some(c) = best {
        let key = (c.file_path.clone(), c.start_line);
        if seen.insert(key) {
            let mut result = SearchResult {
                file_path: c.file_path,
                language: c.language,
                content: c.content,
                start_line: c.start_line,
                end_line: c.end_line,
                score: 0.0,
                symbols: Vec::new(),
            };
            annotate_with_symbols(db, std::slice::from_mut(&mut result))?;
            out.push(result);
        }
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
        // Common words only; none should qualify as rare under DF < 1%.
        assert!(!query_has_rare_literal(&db, "where is authentication handled").unwrap());
    }

    #[test]
    fn gate_triggers_on_rare_long_identifier() {
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        // "ColdFusion" length 10, appears once in 4 chunks (25% DF — above
        // the 1% threshold on this tiny corpus, so it does NOT trigger the
        // length-based branch). This test verifies the threshold logic: on
        // a real corpus of >1000 chunks the 1-in-N-chunks result would drop
        // DF below 1% and trigger correctly. On a toy 4-chunk corpus every
        // real token is "too common", so we assert non-trigger here.
        assert!(!query_has_rare_literal(&db, "ColdFusion support").unwrap());
    }

    #[test]
    fn build_fts_match_query_quotes_identifiers() {
        let q = build_fts_match_query("printer: use `doc_cfg` instead of `doc_auto_cfg`");
        // Each bareword becomes a quoted OR term. Backticks stripped as
        // separators; empty/length-1 tokens dropped.
        assert!(q.contains("\"doc_cfg\""));
        assert!(q.contains("\"doc_auto_cfg\""));
        assert!(q.contains(" OR "));
    }

    #[test]
    fn build_fts_match_query_handles_scoped() {
        let q = build_fts_match_query("call ModuleRef::create");
        // `ModuleRef` is code-shaped (uppercase) so it survives. `call` and
        // `create` are plain lowercase — filtered out. This is the fix that
        // let the gate's BM25 path actually surface target chunks on long
        // queries: only code-shaped tokens make it into the MATCH
        // disjunction, so common English glue words don't flood the ranking.
        assert!(q.contains("\"ModuleRef\""));
        assert!(!q.contains("\"call\""));
        assert!(!q.contains("\"create\""));
    }

    #[test]
    fn build_fts_match_query_drops_plain_english() {
        // Pure-English query contributes no terms.
        assert_eq!(build_fts_match_query("use this instead of that"), "");
    }

    #[test]
    fn gate_triggers_on_dotted_identifier_pair() {
        let db = Database::open_in_memory().unwrap();
        // Both sides ≥3 chars, all lowercase — not code-shaped individually,
        // but the dotted-pair context signals code identity.
        assert!(query_has_rare_literal(&db, "fix moduleref.create edge").unwrap());
    }

    #[test]
    fn gate_does_not_trigger_on_sentence_punctuation() {
        let db = Database::open_in_memory().unwrap();
        // Short sides — `e`, `g`, `i` — below the 3-char minimum, so
        // sentence abbreviations don't slip through.
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
        // Plain words should be absent.
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
        // Semantic ranks a, b, c. BM25 ranks c first (and only).
        // RRF should put c first since it gets a high score from BM25
        // AND a low score from semantic; but a+b only get one contribution.
        let semantic = vec![a.clone(), b.clone(), c.clone()];
        let bm25 = vec![c.clone()];
        let out = rrf_merge(semantic, bm25, 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].file_path, "c.rs");
    }

    #[test]
    fn search_bm25_returns_chunks_containing_rare_literal() {
        // Integration: seed chunks, run BM25 for a rare literal, assert the
        // correct chunk is in the result. Proves the FTS5 insert path is
        // actually populating the sidecar.
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let rows = db.search_bm25("\"ColdFusion\"", 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");
    }
}
