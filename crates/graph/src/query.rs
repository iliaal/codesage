use std::collections::{BinaryHeap, HashMap, HashSet};
use std::cmp::Ordering;

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

pub fn search(
    db: &Database,
    embedder: &mut Embedder,
    reranker: Option<&mut Reranker>,
    req: &SearchRequest,
) -> Result<Vec<SearchResult>> {
    let limit = req.limit.unwrap_or(10);
    let offset = req.offset.unwrap_or(0);

    let known_symbols = extract_known_symbols(db, &req.query);
    let has_symbols = !known_symbols.is_empty();
    let has_reranker = reranker.is_some();
    let overfetch = if has_reranker { RERANK_OVERFETCH } else if has_symbols { 3 } else { 1 };

    let query_embedding = embedder.embed_one(&req.query)?;
    let embedding_bytes = embedding_to_bytes(&query_embedding);

    let semantic_fetch = limit * overfetch;

    let rows = if req.paths.is_some() {
        let languages: Option<Vec<&str>> = req.languages.as_ref().map(|langs| {
            langs.iter().map(|l| l.as_str()).collect()
        });
        let paths: Option<Vec<&str>> = req.paths.as_ref().map(|p| {
            p.iter().map(|s| s.as_str()).collect()
        });
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
                let mut heap: BinaryHeap<DistRow> = BinaryHeap::new();

                for lang in langs {
                    let lang_rows =
                        db.search_knn(&embedding_bytes, fetch_k, Some(lang.as_str()))?;
                    for row in lang_rows {
                        heap.push(DistRow(row));
                        if heap.len() > fetch_k {
                            heap.pop();
                        }
                    }
                }

                let mut merged: Vec<RawSearchRow> =
                    heap.into_sorted_vec().into_iter().map(|d| d.0).collect();
                if offset > 0 && offset < merged.len() {
                    merged.drain(..offset);
                }
                merged.truncate(semantic_fetch);
                merged
            }
        }
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

    annotate_with_symbols(db, &mut results);

    if let Some(reranker) = reranker {
        apply_reranking(reranker, &req.query, &mut results);
    }

    results.truncate(limit);
    Ok(results)
}

struct DistRow(RawSearchRow);

impl PartialEq for DistRow {
    fn eq(&self, other: &Self) -> bool {
        self.0.distance == other.0.distance
    }
}
impl Eq for DistRow {}

impl PartialOrd for DistRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DistRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .distance
            .partial_cmp(&other.0.distance)
            .unwrap_or(Ordering::Equal)
    }
}

fn extract_known_symbols(db: &Database, query: &str) -> Vec<String> {
    let mut known = Vec::new();
    for token in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if token.len() < 3 || !looks_like_identifier(token) {
            continue;
        }
        if let Ok(syms) = db.find_symbols(token, None)
            && !syms.is_empty() {
                known.push(token.to_lowercase());
            }
    }
    known
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
        || s.chars().all(|c| c.is_alphanumeric() || c == '_')
            && s.len() >= 4
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

fn annotate_with_symbols(db: &Database, results: &mut [SearchResult]) {
    let mut cache: HashMap<String, Vec<Symbol>> = HashMap::new();

    for result in results.iter_mut() {
        let symbols = cache
            .entry(result.file_path.clone())
            .or_insert_with(|| db.symbols_for_file(&result.file_path).unwrap_or_default());

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
            let refs = db.find_references(&sym.name, None).unwrap_or_default();
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
        let syms_by_file = db.symbols_for_files(&distinct_files).unwrap_or_default();

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

    entries.sort_by(|a, b| a.distance.cmp(&b.distance).then_with(|| b.reasons.len().cmp(&a.reasons.len())));
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
            if let Ok(defs) = db.find_symbols(&sum.name, None) {
                for d in defs.into_iter().take(1) {
                    symbol_defs.push(d);
                }
            }
        }
    }

    if req.include_callees || req.include_callers {
        for sym in symbol_defs.clone().iter().take(5) {
            if req.include_callers
                && let Ok(refs) = db.find_references(&sym.name, None) {
                    for r in refs.into_iter().take(3) {
                        add_related_from_file(
                            db,
                            &r.from_file,
                            r.line,
                            &mut related,
                            &mut related_keys,
                        );
                    }
                }
            if req.include_callees {
                add_related_from_file(
                    db,
                    &sym.file_path,
                    sym.line_start,
                    &mut related,
                    &mut related_keys,
                );
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
        add_related_from_file(db, &def.file_path, def.line_start, &mut primary, &mut primary_keys);
    }

    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary_keys.clone();

    if req.include_callers {
        let refs = db.find_references(sym_name, None).unwrap_or_default();
        for r in refs.into_iter().take(req.limit) {
            add_related_from_file(db, &r.from_file, r.line, &mut related, &mut related_keys);
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
) {
    let chunks = match db.chunks_for_file(file_path) {
        Ok(c) => c,
        Err(_) => return,
    };
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
            annotate_with_symbols(db, std::slice::from_mut(&mut result));
            out.push(result);
        }
    }
}
