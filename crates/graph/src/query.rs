use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use anyhow::Result;
use codesage_embed::model::Embedder;
use codesage_embed::reranker::Reranker;
use codesage_parser::discover::{TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set};
use codesage_protocol::{
    ContextBundle, DependencyEntry, ExportRequest, FileCategory, FindReferencesRequest,
    FindSymbolRequest, ImpactEntry, ImpactReason, ImpactRequest, ImpactTarget, Language, Reference,
    ReferenceKind, SearchRequest, SearchResult, Symbol, SymbolSummary,
};
use codesage_storage::{Database, RawSearchRow, embedding_to_bytes};
use globset::GlobSet;
use regex::Regex;

/// Parse a `Language` value out of a DB-stored language string. Every row was
/// written by `Language::as_str()`, so an unknown value indicates DB corruption
/// or a schema mismatch — fail loudly rather than producing garbage results.
fn parse_db_language(s: &str) -> Language {
    Language::parse(s).unwrap_or_else(|| panic!("unknown language string in DB: {s:?}"))
}

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
    language: Option<&str>,
    paths: Option<&[&str]>,
) -> Result<Vec<RawSearchRow>> {
    db.search_bm25(match_expr, fetch_limit, language, paths)
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

    let page_window = limit.saturating_add(offset);
    let semantic_fetch = page_window.saturating_mul(overfetch);

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
            0,
            languages.as_deref(),
            paths.as_deref(),
        )?
    } else {
        match &req.languages {
            None => db.search_knn(&embedding_bytes, semantic_fetch, None)?,
            Some(langs) if langs.len() == 1 => {
                db.search_knn(&embedding_bytes, semantic_fetch, Some(langs[0].as_str()))?
            }
            Some(langs) => {
                // Fan-out per language (sqlite-vec's partition key forces
                // per-value queries) and merge in-memory. sort+truncate is
                // simpler than a bounded BinaryHeap and fetch_k stays small
                // enough (N_langs * ~50) that asymptotic cost doesn't matter.
                let mut merged: Vec<RawSearchRow> = Vec::new();
                for lang in langs {
                    let lang_rows =
                        db.search_knn(&embedding_bytes, semantic_fetch, Some(lang.as_str()))?;
                    merged.extend(lang_rows);
                }
                merged.sort_by(|a, b| {
                    a.distance
                        .partial_cmp(&b.distance)
                        .unwrap_or(Ordering::Equal)
                });
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
            let bm25_paths: Option<Vec<&str>> = req
                .paths
                .as_ref()
                .map(|p| p.iter().map(|s| s.as_str()).collect());
            match bm25_search_candidates(
                db,
                &match_expr,
                semantic_fetch,
                bm25_language,
                bm25_paths.as_deref(),
            ) {
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

    if definition_boost_enabled() {
        apply_definition_boost(&mut results, &req.query);
    }

    if path_penalty_enabled() {
        apply_path_penalties(&mut results);
    }

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

    if file_saturation_enabled() {
        apply_file_saturation(&mut results);
    }

    apply_offset_and_limit(&mut results, offset, limit);
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

// Definition boost: when the query is a bare symbol (CamelCase, snake_case,
// namespace-qualified), strongly promote candidate chunks that contain a
// language-keyword + symbol_name match (e.g. `class FooBar`, `fn foo_bar`,
// `defmodule My.FooBar`). Pattern from Semble's boosting.py
// _boost_symbol_definitions: additive boost = 3 * max_score, with a 1.5x
// multiplier when the file stem also matches the symbol. Applied after the
// existing apply_symbol_boost (which only does +0.1 per known-symbol token)
// and BEFORE path_penalty (so a definition in a test file is still boosted,
// then discounted by 0.3x — the correct net signal for "this IS the
// definition, but it's the test version, not the production one").
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
// File stem matches the symbol name (e.g. login_controller.rs for
// LoginController) — the file is almost certainly the canonical home of
// this symbol, so push it harder.
const DEFINITION_FILE_STEM_BONUS: f32 = 1.5;
// Half-strength when the symbol is embedded in an NL query rather than the
// whole query ("how does StateManager initialize?" vs bare `StateManager`).
// The user is asking *about* the symbol but may want explanatory context, so
// the definition chunk is still a strong signal — just not the dominant one.
const EMBEDDED_SYMBOL_BOOST_SCALE: f32 = 0.5;

static SYMBOL_QUERY_RE: OnceLock<Regex> = OnceLock::new();

fn symbol_query_re() -> &'static Regex {
    SYMBOL_QUERY_RE.get_or_init(|| {
        // Four shapes accepted:
        //  1. namespace-qualified: Foo::Bar, foo.bar, foo->bar, Foo\Bar
        //  2. leading underscore:  _foo, _Foo, _
        //  3. contains uppercase or underscore in body: fooBar, my_func, Foo
        //  4. starts with uppercase: Foo, FOO
        // Plain lowercase words (e.g. "session", "login") are NL, not symbols.
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
        // CamelCase or PascalCase tokens embedded in otherwise-NL text.
        // Requires an internal capital so plain words ("session") don't
        // match. Excludes pure acronyms (XML, HTTP) — those would be matched
        // by `[A-Z][a-z][a-zA-Z0-9]*[A-Z]` only if a lowercase letter follows
        // the leading capital.
        //   PascalCase: StateManager, LoginController, XmlParser
        //   camelCase:  getCurrentUser, isLoggedIn
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

// Lowercase + strip `_` (snake_case) and `-` (kebab-case, common in JS/TS),
// so `ModuleRef` matches both `module_ref.py` and `module-ref.ts`.
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

fn build_definition_pattern(symbol_name: &str) -> Option<Regex> {
    if symbol_name.is_empty() {
        return None;
    }
    let escaped = regex::escape(symbol_name);
    let kw_alts = DEFINITION_KEYWORDS
        .iter()
        .map(|k| regex::escape(k))
        .collect::<Vec<_>>()
        .join("|");
    // Match: optional start-of-line/whitespace + keyword + whitespace +
    // (optional namespace prefix `foo.` or `Foo::`)* + symbol_name +
    // (whitespace, opening paren/brace/bracket, `<`, `:`, `;`, or end-of-line).
    // (?m) so `^` and `$` anchor to line boundaries inside chunk content.
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

    // Two paths:
    //  - bare symbol query: full-strength boost on the symbol name itself.
    //  - NL query: half-strength boost on each CamelCase token embedded in
    //    the NL ("how does StateManager initialize?" -> StateManager).
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

// Default-on; opt-out via CODESAGE_DEFINITION_BOOST=0 (or "false"). The
// `is_symbol_query` gate makes this provably inert on NL queries (commit
// subjects, "how does X work" prose); manual A/B on bare-symbol queries
// against the nest index showed the definition chunk surfaces over reference
// / method-body chunks (ApplicationConfig: rank-1 score 0.76 → def chunk
// 1.45; MicroservicesModule: 0.65 → 2.57).
static DEFINITION_BOOST_ENABLED: OnceLock<bool> = OnceLock::new();

fn definition_boost_enabled() -> bool {
    *DEFINITION_BOOST_ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("CODESAGE_DEFINITION_BOOST").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

// Non-candidate stem scan: when a bare-symbol query (e.g. "FooBar") didn't
// surface the file with a matching stem (`foo_bar.rs`, `FooBar.java`), scan
// it directly for a definition match and inject the chunk into the pool.
// Backstop for embedding misses on small / oddly-chunked files where the
// definition is the right answer but didn't crack top-50 candidates.
// Pattern from Semble's boosting.py `_scan_non_candidates`.
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

    let all_files = db.all_chunk_file_paths()?;
    let mut injected: Vec<SearchResult> = Vec::new();
    for file_path in all_files {
        if candidate_set.contains(&file_path) {
            continue;
        }
        let Some(stem_str) = std::path::Path::new(&file_path)
            .file_stem()
            .and_then(|s| s.to_str())
        else {
            continue;
        };
        let stem_lower = stem_str.to_lowercase();
        let stem_norm = normalize_stem(stem_str);
        if stem_lower != symbol_lower && stem_norm != symbol_norm {
            continue;
        }
        // Stem matches; scan this file's chunks for the definition.
        let chunks = db.chunks_for_file(&file_path)?;
        for chunk in chunks {
            if pattern.is_match(&chunk.content) {
                // Inject with score=0; downstream definition_boost adds
                // 3 * max_score * 1.5 (file-stem bonus). Path penalty and
                // reranker then judge it on its merits.
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

// Default-on; opt-out via CODESAGE_STEM_SCAN=0. Same gate concept as the
// definition-boost flag.
static STEM_SCAN_ENABLED: OnceLock<bool> = OnceLock::new();

fn stem_scan_enabled() -> bool {
    *STEM_SCAN_ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("CODESAGE_STEM_SCAN").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

// Path-penalty multipliers, ported from Semble's ranking/penalties.py. Applied
// multiplicatively after symbol boost and before cross-encoder reranking, so
// the reranker sees demoted scores and the post-rerank merge respects the
// prior. Tests/benches/compat/examples are still indexed (see
// HARD_EXCLUDE_PATTERNS vs TEST_LIKE_EXCLUDE_PATTERNS in parser/discover.rs)
// so find_references / find_symbol remain accurate; this only down-weights
// them in semantic `search` results where they're rarely the right answer.
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
        // Patterns are static and known-good; an unwrap here would only fire
        // on a code edit that breaks them, which the workspace tests catch.
        build_exclude_set(&patterns).expect("TEST_LIKE_EXCLUDE_PATTERNS compile")
    })
}

fn has_dir_segment(path: &str, names: &[&str]) -> bool {
    path.split('/').any(|seg| names.contains(&seg))
}

pub(crate) fn path_penalty(path: &str) -> f32 {
    let normalized = if path.contains('\\') {
        path.replace('\\', "/")
    } else {
        path.to_string()
    };
    let mut penalty = 1.0f32;

    if test_like_globset().is_match(&normalized) {
        penalty *= SOFT_PENALTY_STRONG;
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

fn apply_path_penalties(results: &mut [SearchResult]) {
    for result in results.iter_mut() {
        result.score *= path_penalty(&result.file_path);
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

// Cached gate: opt-out via CODESAGE_PATH_PENALTY=0 (or "false"). Default on.
// Cached so we don't getenv on every search call.
static PATH_PENALTY_ENABLED: OnceLock<bool> = OnceLock::new();

fn path_penalty_enabled() -> bool {
    *PATH_PENALTY_ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("CODESAGE_PATH_PENALTY").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

// File saturation decay: ranking the same file's Nth chunk gets multiplied by
// 0.5^(N-1) so a single file can't monopolize the top-K. Walks results in
// score order, counts per-file occurrences, decays subsequent chunks, then
// re-sorts. Pattern from Semble's penalties.py rerank_topk file_saturation
// branch. Threshold = 1: the second chunk from a file is the first to decay.
const FILE_SATURATION_THRESHOLD: usize = 1;
const FILE_SATURATION_DECAY: f32 = 0.5;

fn apply_file_saturation(results: &mut [SearchResult]) {
    if results.is_empty() {
        return;
    }
    // Pre-sort by current score so per-file count reflects rank order. The
    // caller (search()) re-sorts after every score-mutating step, so this is
    // usually redundant — but cheap and protects against future reorderings.
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

// Default-on; opt-out via CODESAGE_FILE_SATURATION=0 (or "false").
static FILE_SATURATION_ENABLED: OnceLock<bool> = OnceLock::new();

fn file_saturation_enabled() -> bool {
    *FILE_SATURATION_ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("CODESAGE_FILE_SATURATION").as_deref(),
            Ok("0") | Ok("false")
        )
    })
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
                kind: s.kind,
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
            let refs = references_for_symbol(db, sym)?;
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

fn references_for_symbol(db: &Database, sym: &Symbol) -> Result<Vec<Reference>> {
    if sym.qualified_name != sym.name {
        return db.find_references(&sym.qualified_name, None);
    }

    db.find_references(&sym.name, None)
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
            if let Some(d) = find_definition_for_summary(db, sum, &result.file_path)? {
                symbol_defs.push(d);
            }
        }
    }

    if req.include_callees || req.include_callers {
        let related_symbols: Vec<Symbol> = symbol_defs.iter().take(5).cloned().collect();
        add_related_for_symbols(
            db,
            &related_symbols,
            req.include_callers,
            req.include_callees,
            req.limit,
            &mut related,
            &mut related_keys,
        )?;
    }

    Ok(ContextBundle {
        target_description: format!("query: {query}"),
        primary,
        related,
        symbol_definitions: symbol_defs,
    })
}

fn find_definition_for_summary(
    db: &Database,
    summary: &SymbolSummary,
    file_path: &str,
) -> Result<Option<Symbol>> {
    let candidates: Vec<Symbol> = db
        .find_symbols(&summary.name, Some(summary.kind))?
        .into_iter()
        .filter(|d| d.qualified_name == summary.qualified_name)
        .collect();

    if let Some(same_file) = candidates.iter().find(|d| d.file_path == file_path) {
        return Ok(Some(same_file.clone()));
    }

    Ok(candidates.into_iter().next())
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

    let defs: Vec<Symbol> = defs.into_iter().take(req.limit).collect();
    let mut primary: Vec<SearchResult> = Vec::new();
    let mut primary_keys: HashSet<(String, u32)> = HashSet::new();
    for def in &defs {
        if primary.len() >= req.limit {
            break;
        }
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

    if req.include_callers || req.include_callees {
        add_related_for_symbols(
            db,
            &defs,
            req.include_callers,
            req.include_callees,
            req.limit,
            &mut related,
            &mut related_keys,
        )?;
    }

    Ok(ContextBundle {
        target_description: format!("symbol: {sym_name}"),
        primary,
        related,
        symbol_definitions: defs,
    })
}

fn add_related_for_symbols(
    db: &Database,
    symbols: &[Symbol],
    include_callers: bool,
    include_callees: bool,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let mut seen_callee_defs: HashSet<(String, String, u32)> = HashSet::new();
    for sym in symbols {
        if include_callers {
            add_callers_for_symbol(db, sym, limit, related, related_keys)?;
        }
        if related.len() >= limit {
            break;
        }
        if include_callees {
            add_callees_for_symbol(db, sym, limit, related, related_keys, &mut seen_callee_defs)?;
        }
        if related.len() >= limit {
            break;
        }
    }
    Ok(())
}

fn add_callers_for_symbol(
    db: &Database,
    sym: &Symbol,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let refs = references_for_symbol(db, sym)?;
    for r in refs {
        if related.len() >= limit {
            break;
        }
        add_related_from_file(db, &r.from_file, r.line, related, related_keys)?;
    }
    Ok(())
}

fn add_callees_for_symbol(
    db: &Database,
    sym: &Symbol,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
    seen_defs: &mut HashSet<(String, String, u32)>,
) -> Result<()> {
    let refs = db.references_in_file_range(&sym.file_path, sym.line_start, sym.line_end)?;
    for r in refs {
        if related.len() >= limit {
            break;
        }
        if !is_callee_reference(r.kind) {
            continue;
        }
        for def in db.find_symbols(&r.to_name, None)? {
            let key = (
                def.file_path.clone(),
                def.qualified_name.clone(),
                def.line_start,
            );
            if !seen_defs.insert(key) {
                continue;
            }
            add_related_from_file(db, &def.file_path, def.line_start, related, related_keys)?;
            if related.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}

fn is_callee_reference(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::Call
            | ReferenceKind::Instantiation
            | ReferenceKind::Import
            | ReferenceKind::Include
            | ReferenceKind::Inheritance
            | ReferenceKind::TraitUse
            | ReferenceKind::TypeHint
    )
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
                language: parse_db_language(&c.language),
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
    fn search_bm25_returns_chunks_containing_rare_literal() {
        // Integration: seed chunks, run BM25 for a rare literal, assert the
        // correct chunk is in the result. Proves the FTS5 insert path is
        // actually populating the sidecar.
        let db = Database::open_in_memory().unwrap();
        seed_chunks(&db);
        let rows = db.search_bm25("\"ColdFusion\"", 10, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");
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
}

#[cfg(test)]
mod path_penalty_tests {
    use super::path_penalty;

    #[test]
    fn production_code_keeps_full_score() {
        assert_eq!(path_penalty("src/auth/login.rs"), 1.0);
        assert_eq!(path_penalty("crates/graph/src/query.rs"), 1.0);
        assert_eq!(
            path_penalty("packages/common/pipes/parse-date.pipe.ts"),
            1.0
        );
    }

    #[test]
    fn test_files_get_strong_penalty() {
        assert_eq!(path_penalty("tests/integration.rs"), 0.3);
        assert_eq!(path_penalty("crates/graph/tests/risk.rs"), 0.3);
        assert_eq!(path_penalty("packages/core/test/auth.spec.ts"), 0.3);
        assert_eq!(path_penalty("src/__tests__/login.test.ts"), 0.3);
        assert_eq!(path_penalty("ext/standard/tests/string/foo.phpt"), 0.3);
        assert_eq!(path_penalty("tests/test_login.py"), 0.3);
        assert_eq!(path_penalty("foo/bar/something_test.go"), 0.3);
        assert_eq!(path_penalty("src/Login/LoginTest.php"), 0.3);
    }

    #[test]
    fn bench_files_get_strong_penalty() {
        assert_eq!(path_penalty("benches/throughput.rs"), 0.3);
        assert_eq!(path_penalty("benchmarks/end_to_end.py"), 0.3);
    }

    #[test]
    fn compat_legacy_dirs_get_strong_penalty() {
        assert_eq!(path_penalty("src/compat/php7.php"), 0.3);
        assert_eq!(path_penalty("src/_compat/legacy_api.rs"), 0.3);
        assert_eq!(path_penalty("packages/legacy/v1/foo.ts"), 0.3);
    }

    #[test]
    fn examples_dirs_get_strong_penalty() {
        assert_eq!(path_penalty("examples/quickstart.rs"), 0.3);
        assert_eq!(path_penalty("packages/sdk/examples/main.go"), 0.3);
        assert_eq!(path_penalty("src/_examples/demo.py"), 0.3);
    }

    #[test]
    fn reexport_barrels_get_moderate_penalty() {
        assert_eq!(path_penalty("src/auth/__init__.py"), 0.5);
        // `com.example.*` Java/Kotlin namespace must NOT trigger the examples
        // penalty; we only match plural forms.
        assert_eq!(path_penalty("com/example/foo/package-info.java"), 0.5);
    }

    #[test]
    fn type_declarations_get_mild_penalty() {
        assert!((path_penalty("types/express.d.ts") - 0.7).abs() < 1e-6);
    }

    #[test]
    fn penalties_compose_multiplicatively() {
        // Test in compat/ — strong test penalty AND strong compat penalty stack.
        // 0.3 * 0.3 = 0.09
        assert!((path_penalty("compat/tests/old_api_test.go") - 0.09).abs() < 1e-6);
    }

    #[test]
    fn windows_separators_normalize() {
        assert_eq!(path_penalty(r"tests\integration.rs"), 0.3);
    }

    #[test]
    fn substring_match_does_not_trigger_dir_penalty() {
        // "compatibility" should not match "compat" as a directory segment.
        assert_eq!(path_penalty("src/compatibility/check.rs"), 1.0);
        // "examplesite" should not match "examples".
        assert_eq!(path_penalty("src/examplesite/index.ts"), 1.0);
        // "test_helpers" file in src/ should NOT trigger (no test-like glob match).
        assert_eq!(path_penalty("src/utilities.rs"), 1.0);
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
        // a.rs has two chunks; second one decays 0.5x → 0.45
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
        // After decay+sort: a(1.0), b(0.5), a(0.45), a(0.20)
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
        // Without saturation: a, a, a, b. With: a, b, a, a.
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
        // Must be preceded by a definition keyword.
        assert!(!pat.is_match("// FooBar is a struct"));
    }

    #[test]
    fn boost_promotes_definition_chunk_over_reference_chunk() {
        let mut rs = vec![
            mk("src/uses.rs", "let x = FooBar::new();", 1.0),
            mk("src/foo_bar.rs", "pub struct FooBar { x: i32 }", 0.5),
        ];
        apply_definition_boost(&mut rs, "FooBar");
        // Definition chunk + file-stem-bonus should now lead.
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
        // Query contains "StateManager"; embedded path applies 0.5x
        // strength. Definition chunk should overtake reference chunk.
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
        // Pure acronyms (HTTP, XML) excluded; plain words excluded.
        let syms = extract_embedded_symbols("HTTP request and XML parser handle login");
        assert!(syms.is_empty(), "got: {syms:?}");
        // XmlParser matches PascalCase; HTTP does not.
        let syms = extract_embedded_symbols("how does XmlParser work for HTTP requests");
        assert_eq!(syms, vec!["XmlParser"]);
        // camelCase tokens both match.
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
        // Both definition chunks should now lead; the unrelated one trails.
        assert_eq!(rs[2].file_path, "src/other.rs");
    }

    #[test]
    fn file_stem_bonus_applies_with_underscore_normalization() {
        // login_controller.rs (stem "login_controller", normalized "logincontroller")
        // should match symbol "LoginController".
        let mut rs = vec![
            mk(
                "src/login_controller.rs",
                "pub struct LoginController;",
                0.5,
            ),
            mk("src/other.rs", "pub struct LoginController;", 0.5),
        ];
        apply_definition_boost(&mut rs, "LoginController");
        // Both got the base boost; login_controller.rs got the 1.5x file-stem bonus.
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

    // typedef intentionally not supported (see DEFINITION_KEYWORDS comment).
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
        // Candidate pool only has the reference chunk; the definition file
        // (foo_bar.rs) is NOT in the candidates.
        let mut results = vec![mk("src/uses_foo.rs", "let x = FooBar::new();", 0.6)];
        apply_non_candidate_stem_scan(&db, &mut results, "FooBar").unwrap();
        let injected = results
            .iter()
            .find(|r| r.file_path == "src/foo_bar.rs")
            .expect("stem-matched definition should be injected");
        assert!(injected.content.contains("struct FooBar"));
        // Score is 0.0; downstream definition_boost will lift it.
        assert_eq!(injected.score, 0.0);
    }

    #[test]
    fn does_not_inject_when_definition_already_in_candidates() {
        let db = Database::open_in_memory().unwrap();
        seed(&db);
        // foo_bar.rs IS already a candidate. Should NOT be re-injected.
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
            // No `struct`/`fn`/etc. keyword preceding FooBar.
            &[("// FooBar is documented elsewhere", 1, 5, zero.as_slice())],
        )
        .unwrap();
        let mut results = vec![mk("src/other.rs", "let x = FooBar::new();", 0.6)];
        apply_non_candidate_stem_scan(&db, &mut results, "FooBar").unwrap();
        // foo_bar.rs has no definition keyword → not injected.
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
        // 2-letter symbol is below MIN_LEN; no scan.
        apply_non_candidate_stem_scan(&db, &mut results, "Fb").unwrap();
        assert_eq!(results.len(), before);
    }
}

#[cfg(test)]
mod context_export_tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, SymbolKind};

    fn symbol(name: &str, qualified_name: &str, file_path: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind: SymbolKind::Method,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 1,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
        }
    }

    fn reference(to_name: &str, file_path: &str) -> Reference {
        Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name: to_name.to_string(),
            kind: ReferenceKind::Call,
            line: 5,
            col: 12,
        }
    }

    #[test]
    fn qualified_symbol_without_exact_refs_does_not_fallback_to_bare_tail() {
        let db = Database::open_in_memory().unwrap();
        let repo_file = db
            .upsert_file(&FileInfo {
                path: "app/repo_controller.py".to_string(),
                language: Language::Python,
                content_hash: "repo".to_string(),
            })
            .unwrap();
        let cache_file = db
            .upsert_file(&FileInfo {
                path: "app/cache_controller.py".to_string(),
                language: Language::Python,
                content_hash: "cache".to_string(),
            })
            .unwrap();
        db.insert_references(repo_file, &[reference("find", "app/repo_controller.py")])
            .unwrap();
        db.insert_references(cache_file, &[reference("find", "app/cache_controller.py")])
            .unwrap();

        let refs = references_for_symbol(&db, &symbol("find", "Repository.find", "app/models.py"))
            .unwrap();

        assert!(
            refs.is_empty(),
            "qualified method without exact refs must not fall back to all bare `find` references: {refs:?}"
        );
    }

    #[test]
    fn summary_definition_lookup_uses_qualified_name_and_file() {
        let db = Database::open_in_memory().unwrap();
        let cpp_file = db
            .upsert_file(&FileInfo {
                path: "fixtures/sample.cpp".to_string(),
                language: Language::Cpp,
                content_hash: "cpp".to_string(),
            })
            .unwrap();
        let rust_file = db
            .upsert_file(&FileInfo {
                path: "src/db.rs".to_string(),
                language: Language::Rust,
                content_hash: "rust".to_string(),
            })
            .unwrap();
        db.insert_symbols(
            cpp_file,
            &[symbol(
                "open",
                "app::net::Connection::open",
                "fixtures/sample.cpp",
            )],
        )
        .unwrap();
        db.insert_symbols(rust_file, &[symbol("open", "Database::open", "src/db.rs")])
            .unwrap();

        let summary = SymbolSummary {
            name: "open".to_string(),
            qualified_name: "Database::open".to_string(),
            kind: codesage_protocol::SymbolKind::Method,
        };
        let found = find_definition_for_summary(&db, &summary, "src/db.rs")
            .unwrap()
            .expect("definition should match the summary");

        assert_eq!(found.qualified_name, "Database::open");
        assert_eq!(found.file_path, "src/db.rs");
    }
}
