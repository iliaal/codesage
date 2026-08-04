use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use anyhow::{Context, Result};
use codesage_parser::discover::{TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set};
use codesage_protocol::{Language, SearchRequest, SearchResult, Symbol, SymbolSummary};
use codesage_storage::{Database, RawSearchRow, SemanticValidityToken, embedding_to_bytes};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;

/// Parse a `Language` value out of a DB-stored language string. Every row was
/// written by `Language::as_str()`, so an unknown value means version skew (an
/// older binary reading an index a newer one wrote with a language it lacks) or
/// a corrupt/hand-edited DB.
///
/// This must not panic. It runs in the per-row mapping of every search/export,
/// and in the daemon a panic in a tool handler is silently swallowed by rmcp
/// (no `catch_unwind`), leaving the client hung waiting for a reply that never
/// comes. Degrade instead: warn once and keep the row with a placeholder label.
/// The content is what the caller searched for; the language tag is annotation.
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
    // Clamp to 0: for a distance > √2 (negative cosine similarity) the raw
    // formula is negative, and the downstream multiplicative stages
    // (`apply_path_penalties`, file/directory saturation, qualified-name boost)
    // then *invert* ranking on those rows — a 0.15 penalty multiplies a
    // negative score UP, promoting a worse match. With scores floored at 0 the
    // multiplicative stages are no-ops on the tail and order falls to the stable
    // sort + additive boosts. Mirrors the `.max(0.0)` clamp already used on
    // `rrf_merge`'s synthetic distance.
    (1.0 - distance * distance / 2.0).max(0.0)
}

const RERANK_OVERFETCH: usize = 5;

/// Upper bound on the candidate pool fed to KNN + cross-encoder reranking.
/// `limit + offset` times the overfetch factor would otherwise grow without
/// bound on deep pagination (`offset=1000` → 5000+ rows through the
/// cross-encoder); cap it so rerank cost stays bounded. Pages past this depth
/// return fewer / no results, which is acceptable for semantic search.
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

/// Minimum usable span of the semantic candidates' scores for the fused-score
/// rescale in [`rrf_merge`]. Below this, the min-max rescale would compress
/// every fused row into a band the flat +0.1 symbol boost dwarfs, so the
/// synthetic-span fallback fires instead. 0.05 keeps genuinely-informative
/// spans (typical KNN spreads are well above it) while catching both exact
/// ties and the near-tie degenerate corpora an epsilon test let through.
const MIN_FUSED_RESCALE_SPAN: f32 = 0.05;

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

/// Components of each `::`- or `\`-separated qualified name in the query.
///
/// Dotted names are deliberately excluded. `moduleref.create` splitting into
/// two OR terms is a measured win on the nest corpus, and the namespace-prefix
/// dilution this exists to address does not arise there: a dotted pair's left
/// side is a receiver, not a namespace shared by hundreds of chunks.
fn extract_qualified_name_groups(query: &str) -> Vec<Vec<String>> {
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

    // A qualified name's leading components are namespaces, and each one on
    // its own matches every chunk in that namespace: `Illuminate\Routing\Router`
    // became `"Illuminate" OR "Routing" OR "Router"`, where the first two pull
    // in most of the framework and outvote the symbol actually asked for. Drop
    // them and keep the tail, which is the symbol being asked about.
    //
    // Emitting the full name as an FTS5 phrase alongside was tried and
    // reverted: it measured -0.006 NDCG@10 on the 40 semble C++ queries, the
    // only corpus queries carrying a `::` at all.
    let mut suppressed: HashSet<String> = HashSet::new();
    for parts in extract_qualified_name_groups(query) {
        // The tail stays a term of its own: it is the symbol name, the most
        // selective component, and the spelling a caller may use unqualified.
        // It must still clear the code-shape filter, or a plain-lowercase tail
        // like `ModuleRef::create` would reintroduce as an OR term exactly the
        // common word this is removing.
        let tail_is_selective = parts.last().is_some_and(|t| token_looks_code_shaped(t));
        if tail_is_selective
            && let Some(tail) = parts.last()
            && seen.insert(tail.to_lowercase())
        {
            tokens.push(format!("\"{tail}\""));
        }
        // Dropping the prefix is only safe when something selective survives.
        // With a lowercase tail the phrase is the sole remaining term, and it
        // matches only where the components are adjacent — so a query like
        // `ColdFusion::register` against code that names the class but not
        // that exact pair would produce an empty MATCH and no BM25 leg at all.
        // Keep the prefix in that case and let the phrase add precision on top.
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
        // Dedupe by lowercased form — FTS5 is case-insensitive for this
        // tokenizer, so `Foo` and `foo` would collapse at MATCH time
        // anyway. Fewer OR-terms keeps the MATCH expression parseable.
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
    // Span of the semantic candidates' scores, used to rescale fused RRF
    // scores back onto the scale the downstream boost stages were tuned for.
    let (sem_min, sem_max) = semantic
        .iter()
        .map(|r| l2_to_score(r.distance))
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(s), hi.max(s))
        });
    let (sem_min, sem_max) = if sem_min.is_finite() && sem_max.is_finite() {
        (sem_min, sem_max)
    } else {
        // No semantic candidates (BM25-only fusion): fall back to the full
        // valid score range.
        (0.0, 1.0)
    };
    // When the semantic candidates' scores (near-)collapse — exact ties from
    // duplicated chunks, or scores a few ULPs apart on a tiny corpus — the
    // rescale below would compress every fused row into a band far narrower
    // than the flat +0.1 symbol boost, which could then reorder the fused
    // ranking freely. Anything under MIN_FUSED_RESCALE_SPAN gets a synthetic
    // span below the top score instead, so fused order survives with headroom
    // under the additive boost. An exact-epsilon test here was not enough:
    // spans like 1e-6 passed it and still collapsed the rescale.
    let (sem_min, sem_max) = if sem_max - sem_min < MIN_FUSED_RESCALE_SPAN {
        ((sem_max - 0.2).clamp(0.0, 1.0), sem_max.clamp(0.0, 1.0))
    } else {
        (sem_min, sem_max)
    };
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
    // `scores` is a HashMap, so ties must be broken explicitly or the
    // truncation below keeps an arbitrary subset that varies per call. Exact
    // f64 ties are reachable here, not just theoretical: with RRF_K=60 and
    // BM25_WEIGHT=2.0 a semantic rank-1 (1/62) plus a BM25 rank-63 (2/124)
    // sums bit-identically to a BM25 rank-1 (2/62), and the fetch depth
    // reaches those ranks.
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.file_path.cmp(&b.1.file_path))
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
    });
    ranked.truncate(limit);
    // Convert each fused RRF score to the `distance` field downstream code
    // reads through `l2_to_score`. Order alone is not enough: the boost
    // stages are additive (+0.1 per symbol token), so score *scale* matters.
    // Raw RRF scores top out around 3/(RRF_K+1) ≈ 0.05, which l2_to_score
    // would compress into a ~0.03-wide band — a single +0.1 boost on a
    // mid-ranked row would then overwrite the entire fused ranking. Min-max
    // rescale the fused scores onto the semantic candidates' span instead,
    // then invert l2_to_score (d = sqrt(2*(1-score))) so the downstream read
    // reproduces the rescaled score.
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

/// Reranker callback for [`search`] / [`export_context`]. Takes the query
/// text + candidate documents, returns one cross-encoder score per doc.
///
/// Boxed so the caller can hold the reranker behind whatever locking
/// discipline fits their runtime — `&mut Reranker` for the single-process
/// CLI, `Arc<Mutex<Reranker>>` for the multi-client MCP daemon. The lock
/// is acquired only when search() calls back, not for the duration of
/// the SQL retrieval, so concurrent agents on the same project can
/// interleave their SQL work while one is in the (slow) ORT call.
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
            // Fan-out per language (sqlite-vec's partition key forces
            // per-value queries) and merge in-memory. sort+truncate is
            // simpler than a bounded BinaryHeap and fetch_k stays bounded.
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

pub fn search(
    db: &Database,
    query_embedding: &[f32],
    rerank: Option<RerankFn<'_>>,
    req: &SearchRequest,
) -> Result<Vec<SearchResult>> {
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

    // Gate: is this a query where BM25 would help? Two distinctive shapes
    // covered by `query_has_rare_literal`: backticked identifiers / glob
    // patterns / `::` scoped lookups, and long tokens (>=8 chars) that show
    // up in <1% of chunks. Retrospective analysis on external corpora
    // (ripgrep, nestjs/nest) measured these as the specific failure mode
    // semantic-only retrieval misses. See
    // `notes/20260411-code-intelligence-landscape.md` §1.4 for the memo chain.
    // `CODESAGE_HYBRID` overrides the gate for ablation: `always` fuses every
    // query, `never` disables fusion outright, and the default keys off the
    // rare-literal test above. It exists to settle default-on vs conditional
    // with a measurement rather than an argument.
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

    // Hybrid BM25+semantic fusion, only when the gate triggered. Keeps the
    // semantic-only path identical to pre-hybrid behavior for the 80%+ of
    // queries that don't contain a rare literal, so the ecosystem default
    // doesn't get copy-pasted in where the memo's net-negative finding still
    // applies. `fused` records whether RRF fusion actually ran — the gate can
    // fire while BM25 contributes nothing (empty MATCH expression, no hits),
    // in which case rows stay purely semantic.
    let mut fused = false;
    let rows = if hybrid_gate {
        let match_expr = build_fts_match_query(&req.query);
        if match_expr.is_empty() {
            rows
        } else {
            // Filter the BM25 leg to the FULL requested language set. Left
            // unfiltered, foreign-language rows would occupy rrf_merge slots
            // only to be retained away afterwards — pushing matching-language
            // candidates out of the fused pool entirely.
            let bm25_languages: Option<Vec<&str>> = req
                .languages
                .as_ref()
                .map(|ls| ls.iter().map(|l| l.as_str()).collect());
            let bm25_paths: Option<Vec<&str>> = req
                .paths
                .as_ref()
                .map(|p| p.iter().map(|s| s.as_str()).collect());
            match bm25_search_candidates(
                db,
                &match_expr,
                semantic_fetch,
                bm25_languages.as_deref(),
                bm25_paths.as_deref(),
            ) {
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

    // When a rare-token match drove the fused ranking it should stay the
    // dominant signal, so fused queries pin the blend to the SHORT_ID weight
    // (0.35) rather than skipping the cross-encoder outright. Skipping cost
    // scope-qualified C++ queries the reranker entirely: `absl::`/`fmt::` is
    // ordinary namespacing, not the rare literal the code-literal gate is
    // calibrated for, and those queries scored 0.777 against 0.870 for the
    // rest of the C++ corpus. Keyed on `fused`, not `hybrid_gate`: a gated
    // query whose BM25 leg came back empty is still purely semantic.
    // The ripgrep canary in `project_hybrid_bm25_rrf.md` (reranker demoting
    // `lib.rs` out of top-10 on ``use `doc_cfg` `` queries) is what the
    // reduced weight has to keep passing.
    if let Some(mut rerank) = rerank
        && (!fused || fused_rerank_enabled())
    {
        let weight_override = fused.then_some(RERANK_WEIGHT_SHORT_ID);
        apply_reranking(&mut rerank, &req.query, &mut results, weight_override);
    }

    // Path penalties run AFTER reranking so they land on the final blended
    // score. Ahead of it they were diluted: the merge is
    // `(1-w)*score + w*ce_norm` with `ce_norm` renormalized to [0,1], so a
    // pre-blend demote survived at only `1-w` strength — 40% on natural
    // language queries, where most of these penalties matter. Reranking reads
    // only chunk content and scores every candidate, so the set it sees is
    // unchanged by the move.
    if path_penalty_enabled() {
        apply_path_penalties(&mut results, &req.query);
    }

    if version_demote_enabled() {
        apply_version_demote(&mut results, &req.query);
    }

    // Also after reranking, and for a stronger reason than the penalties: a
    // filename-stem match is metadata the cross-encoder cannot see. It scores
    // chunk content only, so blending a pre-rerank stem boost would dilute a
    // signal the reranker had no way to form an opinion about.
    if stem_match_boost_enabled() {
        apply_stem_match_boost(&mut results, &req.query);
    }

    if file_saturation_enabled() {
        apply_file_saturation(&mut results);
    }

    if dir_saturation_enabled() {
        apply_directory_saturation(&mut results);
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

/// Identifier test for the adaptive-rerank SHORT_ID branch. Stricter than
/// [`looks_like_identifier`]: the token must carry an explicit identifier
/// signal — `_` (snake_case), `-` (kebab-case), an uppercase letter
/// (camel/PascalCase), `::` scoping, or a digit. `looks_like_identifier`'s
/// length clause admits any all-lowercase word ≥4 chars because its caller
/// verifies the token against the symbol table; here there is no existence
/// check, so a plain English word ("authentication") must not qualify.
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
            if content_lower.contains(sym) {
                boost += 0.1;
            }
        }
        result.score += boost;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

// Multiplicative ×2.0 boost when a query-derived identifier token matches the
// *qualified name* of a symbol overlapping the chunk. Pattern ported from
// code-review-graph commit c04af36 ("feat: deterministic eval pipeline,
// multi-hop benchmark, search lift") as the §2.12 A/B candidate.
//
// Different from `apply_symbol_boost`: that one matches tokens against raw
// chunk *content* (a chunk mentioning `parse_config` in a comment gets the
// +0.1); this one matches against the chunk's overlapping *symbols*'
// qualified names. Stronger signal, harder boost.
//
// Anti-trigger filter: a previous A/B run (notes/20260526-search-lift-ab-
// report.md) showed pure lexical matching produces false positives when a
// query word doubles as a Rust trait/method name. Query "load default
// command options..." picked up `Mode::default` and bumped the wrong file
// over the right one. Anti-trigger tokens (common English verbs and Rust
// trait methods) only fire when the query token matches a NON-LEAF segment
// of the qualified-name — i.e., the type/module part, not the method part.
// `default` matching `Mode::default` (leaf-only) is suppressed; `ignore`
// matching `Ignore::add_child_path` (root match) survives.
//
// Idempotent: each result is boosted at most once even if multiple
// qualified-names match — the goal is "this chunk contains the named
// symbol, definitively," not "the more names matched, the harder to boost."
//
// Default-off; opt-in via `CODESAGE_QUALIFIED_NAME_BOOST=1` for the §2.12
// A/B. Annotation must have already populated `result.symbols`.
const QUALIFIED_NAME_BOOST_FACTOR: f32 = 2.0;

// Query words that double as Rust trait methods / common method names. When a
// query token here matches a qualified-name's LEAF segment only, the boost
// is suppressed — leaf-only matches are statistically dominated by
// false-positives (e.g. `Mode::default` matched on the English word "default"
// in a config-loading query). Stem-based filter still allows the boost when
// the token matches a non-leaf (type/module) segment of the qualified-name.
const ANTI_TRIGGER_TOKENS: &[&str] = &[
    // Rust trait methods
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
    // Common accessor patterns
    "get",
    "set",
    "has",
    "is",
    "len",
    "size",
    // Verbs that double as method names
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
    // Iter
    "next",
    "iter",
    // Common but-not-distinctive
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
        // Empty qn (shouldn't happen in practice) — fall back to bare name.
        return !is_anti_trigger(token) && name == token;
    }
    if is_anti_trigger(token) {
        // Anti-trigger tokens only fire when matching a NON-LEAF segment.
        // For `Mode::default` (segments ["mode", "default"]) and token
        // `default`, non-leaf is ["mode"] — no match, suppress. For
        // `Config::load` (segments ["config", "load"]) and token `config`,
        // non-leaf is ["config"] — match, allow.
        if segments.len() < 2 {
            // Bare name: the lone segment is the leaf, no non-leaf to check.
            return false;
        }
        segments[..segments.len() - 1].contains(&token)
    } else {
        // Non-anti-trigger: any segment match. Matches both root (e.g. token
        // `ignore` against `Ignore::add_child_path` — the §2.12 win case)
        // and leaf (e.g. token `login` against `AuthService::login`).
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

// Default-off, unlike the other gates: this exists to A/B the §2.12
// hypothesis without shipping the new boost to existing users. Promote to
// default-on if the bench shows lift.
fn qualified_name_boost_enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        matches!(
            std::env::var(tuning::QUALIFIED_NAME_BOOST).as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("True") | Ok("yes")
        )
    })
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
    // The keyword alternation is constant; build it once. Only `escaped`
    // (the symbol) varies between calls, so the full regex still has to compile
    // per distinct symbol, but the per-call string churn is gone.
    static KW_ALTS: OnceLock<String> = OnceLock::new();
    let kw_alts = KW_ALTS.get_or_init(|| {
        DEFINITION_KEYWORDS
            .iter()
            .map(|k| regex::escape(k))
            .collect::<Vec<_>>()
            .join("|")
    });
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

/// Environment toggles for the search scoring pipeline, named once here so
/// the `CODESAGE_*` strings aren't scattered as duplicated literals. Every
/// toggle is read once per process and cached (`OnceLock`).
///
/// Boolean gates, default-on — disable with `=0` / `=false` (see
/// `env_default_on`):
///
/// - `DEFINITION_BOOST`: keyword + symbol-name definition-chunk boost
/// - `STEM_SCAN`: non-candidate stem-scan chunk injection
/// - `TEST_QUERY_AWARE`: test-intent query classification (lifts the
///   test-path demote on test-shaped queries)
/// - `PATH_PENALTY`: test / compat / examples / barrel / `.d.ts` path demotes
/// - `FILE_SATURATION`: per-file chunk-count decay
/// - `DIR_SATURATION`: per-directory chunk-count decay
/// - `ADAPTIVE_RERANK`: query-shape-adaptive rerank blend weight
///
/// Boolean gate, default-off — enable with `=1` / `=true` / `=yes`:
///
/// - `QUALIFIED_NAME_BOOST`: ×2 qualified-name symbol-match boost (§2.12 A/B)
///
/// Numeric overrides (invalid values fall back to the built-in default):
///
/// - `DIR_SATURATION_THRESHOLD`: integer ≥ 1, default 2
/// - `DIR_SATURATION_DECAY`: float in (0.0, 1.0], default 0.75
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
    pub(super) const FUSED_RERANK: &str = "CODESAGE_FUSED_RERANK";
    pub(super) const STEM_MATCH_BOOST: &str = "CODESAGE_STEM_MATCH_BOOST";
    pub(super) const HYBRID: &str = "CODESAGE_HYBRID";
}

// The `is_symbol_query` gate makes the definition boost provably inert on NL
// queries (commit subjects, "how does X work" prose); manual A/B on
// bare-symbol queries against the nest index showed the definition chunk
// surfaces over reference / method-body chunks (ApplicationConfig: rank-1
// score 0.76 → def chunk 1.45; MicroservicesModule: 0.65 → 2.57).
/// True unless the named env var is explicitly set to `0` / `false`. The
/// default-on gate shape shared by the post-retrieval scoring stages.
pub(crate) fn env_default_on(var: &str) -> bool {
    !matches!(std::env::var(var).as_deref(), Ok("0") | Ok("false"))
}

/// True only when the named env var is explicitly set to `1` / `true`. The
/// gate shape for stages that are not validated well enough to ship on.
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

/// How the BM25+RRF fusion gate behaves. Default `Gated` keys off
/// `query_has_rare_literal`; the other two exist so the default-on question is
/// answerable by measurement.
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
        // Anything else, including unset and typos, keeps shipped behavior.
        _ => HybridMode::Gated,
    })
}

static STEM_MATCH_BOOST_ENABLED: OnceLock<bool> = OnceLock::new();

// Default OFF, and the A/B is why rather than caution: measured on the semble
// corpus it is +0.001 pooled, which is noise. Per language, rust +0.008 and
// cpp -0.004; c, php and typescript do not move. Four queries improve, one
// regresses, and the regression is large (nlohmann "ADL-based to_json and
// from_json", 1.000 -> 0.500) because adl_serializer.hpp is the target while
// the query names to_json and from_json.
//
// Worth keeping rather than deleting: rust +0.008 with no rust regression is
// the only positive movement any proposal has produced for the language with
// the largest remaining gap (0.785 against semble's 0.856), so this is a
// reasonable per-project opt-in for Rust-heavy repos. It is not a default.
fn stem_match_boost_enabled() -> bool {
    *STEM_MATCH_BOOST_ENABLED.get_or_init(|| env_default_off(tuning::STEM_MATCH_BOOST))
}

static FUSED_RERANK_ENABLED: OnceLock<bool> = OnceLock::new();

// Default OFF: measured net-negative. Scope-qualified C++ queries really do
// miss the cross-encoder — 20 of 60 cpp corpus queries trip the code-literal
// gate and scored 0.777 against 0.870 for the rest — but reranking them at
// 0.35 does not recover it. On the semble corpus the change is -0.0012 pooled
// (c -0.008, cpp +0.004, ts -0.002) and turns 3 regressions into 6, trading
// wins of +0.37/+0.15/+0.13 against losses of -0.50/-0.37/-0.11. That spread
// with no net gain is the failure mode `project_hybrid_bm25_rrf.md` predicted:
// where BM25 fired, it is already the better signal.
//
// Kept as a gate rather than reverted because the underlying gap is real and
// the plumbing is the expensive part; a future attempt likely needs a
// different remedy (a narrower gate that excludes `::` from the code-literal
// test, say) rather than a different weight.
fn fused_rerank_enabled() -> bool {
    *FUSED_RERANK_ENABLED.get_or_init(|| env_default_off(tuning::FUSED_RERANK))
}

static DEFINITION_BOOST_ENABLED: OnceLock<bool> = OnceLock::new();

fn definition_boost_enabled() -> bool {
    *DEFINITION_BOOST_ENABLED.get_or_init(|| env_default_on(tuning::DEFINITION_BOOST))
}

/// Precomputed lookup from a file's stem — lowercased, and separator-
/// normalized via [`normalize_stem`] — to every indexed file path sharing
/// it. Replaces the per-query `all_chunk_file_paths()` full scan plus
/// per-file lowercasing that used to run on every symbol-shaped search.
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

/// Process-lifetime cache of per-project [`StemIndex`]es, keyed by
/// (db file path, chunk table). Entries rebuild when the cheap validity
/// token from `semantic_files` changes, so the watcher's reindex naturally
/// invalidates them. Values hold only stems + paths, so memory stays small
/// even with many projects pooled in one daemon.
static STEM_CACHE: LazyLock<Mutex<StemCacheMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn stem_index_from_cache(
    cache: &Mutex<StemCacheMap>,
    key: StemCacheKey,
    db: &Database,
) -> Result<Arc<StemIndex>> {
    // Read the validity token, then check the cache under the lock and drop
    // it immediately. Building a cold or invalidated index (a full
    // `all_chunk_file_paths` scan) must happen OUTSIDE the global lock: this
    // cache is shared across every pooled project in the daemon, so holding
    // it across a build would serialize the stem stage of concurrent searches
    // on unrelated projects behind one project's rebuild.
    let token = db.semantic_files_validity_token()?;
    {
        let cache = cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hit) = cache.get(&key)
            && hit.token == token
        {
            return Ok(Arc::clone(hit));
        }
    }
    // Build unlocked. A concurrent builder racing on the same key produces an
    // equivalent index, so a duplicate build is wasted work but never wrong.
    let built = Arc::new(StemIndex::build(db, token)?);
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    // Re-check: a racer may have inserted a fresh entry for the same token
    // while we built. Prefer the already-cached Arc so concurrent callers
    // converge on one instance.
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
        // In-memory handles have no stable identity to key a process cache
        // on; build fresh (the single-shot CLI cost profile).
        None => {
            let token = db.semantic_files_validity_token()?;
            Ok(Arc::new(StemIndex::build(db, token)?))
        }
    }
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

    let stem_index = stem_index_for(db)?;
    let mut injected: Vec<SearchResult> = Vec::new();
    for file_path in stem_index.matching_paths(&symbol_lower, &symbol_norm) {
        if candidate_set.contains(&file_path) {
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

static STEM_SCAN_ENABLED: OnceLock<bool> = OnceLock::new();

fn stem_scan_enabled() -> bool {
    *STEM_SCAN_ENABLED.get_or_init(|| env_default_on(tuning::STEM_SCAN))
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

// Query-aware path penalty. `query_is_test_shaped` tells the function whether
// the user query mentions test/spec/fixture intent — when true, we skip the
// test-like demote so legitimate test-intent queries surface test files and
// they compete on merit ("find the test for X" surfaces them above the
// production file). Non-test-shaped queries get the full 0.15x demote
// (0.3x baseline × 0.5x extra) — §2.11 motivation: 0.3x alone wasn't enough
// to surface `InterceptorManager.js` over 9 sibling `*.test.js` files on the
// axios "request and response interceptors" query.
// Compat/examples/d.ts/re-export demotes are query-independent.
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

// Declaration headers in C projects describe an API; the `.c` file implements
// it, and the implementation is what a behavior query wants. Measured on the
// semble corpus, 22% of files ranked above a C target were headers while only
// 2% of C targets were.
//
// Gating on the row language is what makes this safe, and it must not be
// relaxed to "any header": in C++ the header IS the implementation
// (nlohmann-json, abseil and fmtlib are header-only, and every C++ target in
// that corpus is a header). Simulated ungated the demote costs C++ 0.134.
// The discovery layer already resolves a bare `.h` to Cpp for any project
// carrying an unambiguous C++ extension, so `Language::C` here means the
// project really is C.
//
// `-inl.h` / `_inl.h` carry inline definitions rather than declarations —
// libuv's `heap-inl.h` is a legitimate target — so they are exempt.
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

// Host-platform directories. A project carrying parallel per-platform trees
// (libuv's `src/unix/` and `src/win/`) answers most behavior queries with the
// tree that actually runs on the caller's machine.
//
// Default OFF. The mechanism is verified — win/tcp.c outranking unix/tcp.c is
// a clean mirror pair across 10 libuv queries — but every measured point comes
// from that one repo, and its ground truth may encode the same host assumption
// the rule does. Needs validation on C repos outside the corpus before this
// can be considered for default-on.
const FOREIGN_PLATFORM_DIR_NAMES: &[&str] = &["win", "win32", "windows"];

fn foreign_platform_penalty(path: &str) -> f32 {
    if cfg!(windows) {
        return 1.0;
    }
    let normalized = path.replace('\\', "/");
    if has_dir_segment(&normalized, FOREIGN_PLATFORM_DIR_NAMES) {
        SOFT_PENALTY_MILD
    } else {
        1.0
    }
}

// Whole-token match so "windowsize" or "rewind" can't trip the guard.
const PLATFORM_INTENT_KEYWORDS: &[&str] = &[
    "windows", "win32", "win64", "iocp", "msvc", "mingw", "winapi",
];

fn query_names_foreign_platform(query: &str) -> bool {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            let lower = t.to_ascii_lowercase();
            PLATFORM_INTENT_KEYWORDS.contains(&lower.as_str())
        })
}

// Extra multiplier applied to test-like paths when the query is non-test-shaped.
// Stacks on top of SOFT_PENALTY_STRONG, so total = 0.3 * 0.5 = 0.15x.
// 0.15x is empirically motivated: the §2.11 axios case had 9 test files at
// ~0.3x dominating top-10. A 0.5x extra demote (→ 0.15x total) was the smallest
// nudge that surfaced the conceptual target in a follow-up spot check.
const EXTRA_TEST_DEMOTE_NON_TEST_QUERY: f32 = 0.5;

// Test-intent keywords for query classification. Whole-token match against the
// query (case-insensitive). Tokens checked are alphanumeric runs split out of
// the query — so "Login.test.js" → ["login", "test", "js"] which would
// (correctly) classify as a test-shaped query, while "interceptors" doesn't
// match.
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

// When disabled, query_is_test_shaped always returns false → path_penalty
// behaves as the pre-§1.22 fixed 0.3x for test-like paths regardless of query.
static TEST_QUERY_AWARE_ENABLED: OnceLock<bool> = OnceLock::new();

fn test_query_aware_enabled() -> bool {
    *TEST_QUERY_AWARE_ENABLED.get_or_init(|| env_default_on(tuning::TEST_QUERY_AWARE))
}

fn apply_path_penalties(results: &mut [SearchResult], query: &str) {
    let is_test_query = query_is_test_shaped(query);
    let demote_foreign_platform = platform_demote_enabled() && !query_names_foreign_platform(query);
    // The header demote expresses a preference for the implementing `.c` over
    // the header declaring it, so it only means anything when a `.c` is in the
    // running. Header-only projects whose dialect resolves to C — fmtlib's
    // `include/fmt` is all `.h` — would otherwise have every candidate demoted
    // uniformly except the `-inl.h` exemption, which promotes that one file to
    // rank 1 for free. Measured: it cost fmtlib three queries.
    let has_c_implementation = results
        .iter()
        .any(|r| r.language == Language::C && r.file_path.ends_with(".c"));
    for result in results.iter_mut() {
        let mut penalty = path_penalty_for_query(&result.file_path, is_test_query);
        if has_c_implementation {
            penalty *= declaration_header_penalty(&result.file_path, result.language);
        }
        if demote_foreign_platform {
            penalty *= foreign_platform_penalty(&result.file_path);
        }
        result.score *= penalty;
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

// Bounded multiplier for a query token that names a candidate's file stem.
// Deliberately mild: a move-to-front variant scored better on abseil but cost
// nlohmann's "ADL-based to_json and from_json conversion hooks" a full rank
// (to_json.hpp / from_json.hpp jumping over the adl_serializer.hpp target).
//
// What 1.2x actually bounds is the score ratio, not the rank movement: a
// boosted candidate passes every result scoring below `its_score * 1.2`. Where
// results are tightly clustered that can still be several positions. It caps
// how far a boost can reach, not how many places it can travel.
const STEM_MATCH_BOOST: f32 = 1.2;
// Counted in characters, not bytes — see `stem_match_tokens`.
const STEM_MATCH_MIN_TOKEN_LEN: usize = 4;

/// Query tokens specific enough to be worth matching against a file stem.
///
/// The gate is an identifier signal: an underscore, a digit, or mixed case
/// carrying at least one lowercase letter. That admits `path_router`,
/// `StrSplit`, `ABSL_LOG` and `Semaphore` while rejecting the bare all-caps
/// acronyms that are ordinary prose vocabulary — `JSON`, `HTTP`, `MIME`. The
/// exclusion is load-bearing: boosting on `JSON` pulls nlohmann's `json.hpp`
/// up on nearly every query in that repo, which flipped a 9-query regression
/// in the advisory simulation.
///
/// Note `ABSL_LOG` qualifies through the underscore rather than through case,
/// which is why the rule is "has an identifier signal" and not "is not
/// all-caps".
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

// Generalizes DEFINITION_FILE_STEM_BONUS, which only reaches results that
// already matched the definition regex. That regex structurally cannot fire
// for a C++ free function (`absl::StrSplit` has no class/struct keyword) or a
// macro-attributed declaration (`class ABSL_LOCKABLE Mutex` breaks the
// keyword+name pattern), which is exactly where the filename is the clearest
// available signal.
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

// Packages that ship several major versions side by side (zod's `v3/` and
// `v4/`) give the ranker no reason to prefer the current one, so the older
// tree wins on raw similarity. Demote candidates below the highest version
// present in the candidate set.
//
// The maximum is taken from the candidates rather than a repo census, which
// keeps the rule stateless and makes it a no-op whenever the candidates all
// share one version. Mild rather than strong, because an old line can still be
// actively maintained.
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

// Defaults retuned 2026-05-16 (was threshold=3, decay=0.85). A/B harness at
// /tmp/dir-saturation-ab.py across laravel-framework + redux + flask showed
// (2, 0.75) consistently wins or ties: laravel-framework NDCG@10 0.773 → 0.779,
// flask 0.906 → 0.909, redux unchanged at 0.957, no recall regressions.
// Earlier 0.85 decay was a guess; the §2.11 finding (laravel still surfaced
// QueueManager.php only at rank 5) suggested room for a steeper decay, and
// dropping the threshold from 3 to 2 cuts the noise earlier without
// over-demoting clean repos.
const DIR_SATURATION_THRESHOLD_DEFAULT: usize = 2;
const DIR_SATURATION_DECAY_DEFAULT: f32 = 0.75;

// Env overrides for tuning without rebuilds. Cached on first read.
// `CODESAGE_DIR_SATURATION_THRESHOLD` accepts a positive integer; values < 1
// fall back to the default. `CODESAGE_DIR_SATURATION_DECAY` accepts a float in
// (0.0, 1.0]; values outside that range fall back to the default. A decay of
// 1.0 effectively disables the signal (no decrement past threshold).
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

/// Penalize chunks past threshold from the same parent directory, after
/// `file_saturation` handles same-file. Motivated by the §2.10 semble-
/// corpus laravel-framework finding: the query "queue connection
/// resolution and connectors" returned 10 top results all from
/// `Queue/Connectors/*Connector.php` (10 different files, same dir),
/// pushing the conceptual target `QueueManager.php` off the page.
/// Per-file saturation didn't catch it because each Connector is a
/// distinct file. This signal applies after file_saturation so the two
/// stack naturally — a file that's also in an oversaturated dir gets
/// hit twice.
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

const RERANK_WEIGHT_DEFAULT: f32 = 0.5;
const RERANK_WEIGHT_SHORT_ID: f32 = 0.35;
const RERANK_WEIGHT_NATLANG: f32 = 0.6;

/// Pick the rerank/semantic blend weight based on query shape. Adopted
/// from semble's adaptive-α signal — see `notes/20260516-semble-
/// classification.md`. Codesage's pipeline has no BM25 stage, so the
/// natural mapping is to vary the **rerank vs semantic** blend instead
/// of the **BM25 vs dense** blend.
///
/// - **Short identifier queries** (`FooBar`, `parse_config`,
///   `Middleware`): trust the cross-encoder less. Symbol-boost and
///   definition-boost already promote the right candidates; the
///   cross-encoder's semantic judgement adds noise on bare identifiers.
///   Weight: `RERANK_WEIGHT_SHORT_ID = 0.35`.
/// - **Natural-language queries** (≥3 alphabetic words, e.g. "queue
///   connection resolution and connectors"): trust the cross-encoder
///   more. Semantic-only retrieval can over-cluster on sibling files;
///   the cross-encoder's query/doc relevance scoring untangles them.
///   Weight: `RERANK_WEIGHT_NATLANG = 0.6`.
/// - **Mixed / fallback**: keep the historical 0.5.
fn adaptive_rerank_weight(query: &str) -> f32 {
    if !adaptive_rerank_weight_enabled() {
        return RERANK_WEIGHT_DEFAULT;
    }
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return RERANK_WEIGHT_DEFAULT;
    }
    // Short identifier shape: single token carrying an explicit identifier
    // signal (snake_case, kebab-case, camel/PascalCase, `::` scoping, or a
    // digit — see `looks_like_short_identifier`). A single all-lowercase
    // alphabetic word ("authentication") is natural language, not an
    // identifier, and falls through to the default weight.
    let single_token = !trimmed.chars().any(char::is_whitespace);
    if single_token && looks_like_short_identifier(trimmed) {
        return RERANK_WEIGHT_SHORT_ID;
    }
    // Natural language: 3+ alphabetic words, none of them looking like
    // a hard identifier.
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

/// `weight_override` forces a blend weight instead of deriving one from the
/// query shape. Fused (BM25/RRF) queries use it to keep the rare-token prior
/// dominant while still consulting the cross-encoder.
fn apply_reranking(
    rerank: &mut RerankFn<'_>,
    query: &str,
    results: &mut [SearchResult],
    weight_override: Option<f32>,
) {
    if results.is_empty() {
        return;
    }

    let docs: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
    let ce_scores = match rerank(query, &docs) {
        Ok(s) => s,
        Err(_) => return,
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
}

pub(crate) fn annotate_with_symbols(db: &Database, results: &mut [SearchResult]) -> Result<()> {
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

        // Single language in the set: only the rust hit.
        let rows = db
            .search_bm25("\"ColdFusion\"", 10, Some(&["rust"]), None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/reg.rs");

        // Two languages: both hits, nothing else.
        let mut rows = db
            .search_bm25("\"ColdFusion\"", 10, Some(&["rust", "php"]), None)
            .unwrap();
        rows.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        let paths: Vec<&str> = rows.iter().map(|r| r.file_path.as_str()).collect();
        assert_eq!(paths, ["src/legacy.php", "src/reg.rs"]);

        // Empty set behaves like no filter.
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
        // Regression for the unfiltered multi-language BM25 leg: with 2+
        // requested languages the BM25 candidates used to come back
        // unfiltered, so foreign-language rows occupied the rrf_merge slots
        // and were only retained away afterwards — pushing the
        // matching-language BM25 hit out of the fused pool entirely.
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
        }
    }

    #[test]
    fn gated_query_with_empty_bm25_still_applies_reranking() {
        // `Zzz::qqq` fires the hybrid gate (`::`), but no indexed chunk
        // contains "Zzz", so BM25 comes back empty and the rows stay purely
        // semantic. The reranker must still run — skipping it too would drop
        // BOTH ranking stages for this query.
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
        // "ColdFusion" is in src/reg.rs, so BM25 has hits and fusion runs. The
        // reranker stays skipped: reranking fused queries at the reduced
        // weight measured net-negative on the semble corpus (see
        // `fused_rerank_enabled`), so where BM25 fired it keeps the ranking.
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
        // The point of pinning fused queries to RERANK_WEIGHT_SHORT_ID rather
        // than letting them take the natural-language weight. A rare-token
        // winner leading by 0.60 with the cross-encoder ranking it last
        // survives at 0.35 and does not at 0.6.
        //
        // This is a margin, not an invariant. Against a maximally hostile
        // cross-encoder the lead has to clear w/(1-w) — about 0.54 at weight
        // 0.35, and an unreachable 1.5 at 0.6 — so a narrow fused win can
        // still be flipped. The empirical guard is the ripgrep canary in
        // `project_hybrid_bm25_rrf.md`, not this test.
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
        apply_reranking(
            &mut rerank,
            "ColdFusion::register",
            &mut kept,
            Some(RERANK_WEIGHT_SHORT_ID),
        );
        assert_eq!(
            kept[0].file_path, "src/reg.rs",
            "fused winner should survive at the SHORT_ID weight"
        );

        let mut lost = vec![mk("src/reg.rs", 0.95), mk("src/lib.rs", 0.35)];
        let mut rerank: RerankFn = Box::new(ce);
        apply_reranking(
            &mut rerank,
            "ColdFusion::register",
            &mut lost,
            Some(RERANK_WEIGHT_NATLANG),
        );
        assert_eq!(
            lost[0].file_path, "src/lib.rs",
            "same disagreement should flip the order at the natural-language weight"
        );
    }

    #[test]
    fn fused_scores_rescale_to_semantic_span_so_flat_boost_cannot_invert() {
        // Regression for score compression: raw RRF scores (≤ ~3/61) read
        // through l2_to_score used to land every fused row in ~[0.52, 0.55],
        // so a flat +0.1 symbol boost on a mid-ranked row (2-3x the whole
        // spread) overwrote the fused ranking. Rescaled onto the semantic
        // candidates' span, the fused top hit keeps a margin a single +0.1
        // boost can't erase.
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
        // Fused scores span the semantic candidates' range, not ~[0.52, 0.55].
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

        // A single +0.1 boost on the mid-ranked row must not displace the
        // fused top hit. Under the old 1.0-rrf_score compression, mid would
        // jump from ~0.516 to ~0.616 past top's ~0.548.
        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }

    #[test]
    fn fused_rescale_spreads_rows_when_all_semantic_scores_are_equal() {
        // Degenerate corpus: every semantic candidate shares one score
        // (duplicated chunks / tiny index), so sem_max == sem_min. Without the
        // synthetic-span fallback the rescale maps all fused rows to that one
        // value, and a single +0.1 boost on a mid row then reorders the fused
        // ranking freely. The fallback opens a span below the shared score so
        // fused order is preserved with headroom the boost can't erase.
        let mk_row = |path: &str, content: &str| RawSearchRow {
            file_path: path.into(),
            language: "rust".into(),
            content: content.into(),
            start_line: 1,
            end_line: 1,
            distance: 0.2, // identical for every row => l2_to_score = 0.98
        };
        // Fused order follows semantic rank: r0 (top) .. r3 (bottom).
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

        // The rescale must not collapse: the top fused hit keeps a strictly
        // higher score than the bottom one.
        assert_eq!(results[0].file_path, "top.rs");
        assert!(
            results[0].score > results.last().unwrap().score,
            "equal semantic scores must still yield a spread fused ranking: {:?}",
            results.iter().map(|r| r.score).collect::<Vec<_>>()
        );

        // A single +0.1 boost on the mid row must not displace the top hit.
        // Without the fallback every score would be 0.98, so the boosted row
        // (1.08) would jump straight past the top.
        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }

    #[test]
    fn fused_rescale_uses_synthetic_span_on_near_tied_semantic_scores() {
        // Scores differing by a few ULPs (~1e-6 span) passed the old exact-tie
        // epsilon check, so the min-max rescale compressed every fused row
        // into a sub-microscopic band a flat +0.1 boost reordered at will.
        // Near-ties must take the synthetic-span path exactly like exact ties.
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

        // A single +0.1 boost on the mid row must not displace the top hit.
        apply_symbol_boost(&mut results, &["known_sym".to_string()]);
        assert_eq!(results[0].file_path, "top.rs");
    }
}

#[cfg(test)]
mod path_penalty_tests {
    use super::path_penalty_for_query;

    // The non-test-query shape — the production `search()` path for ordinary
    // queries. Test-like paths get 0.3 (baseline) * 0.5 (extra) = 0.15;
    // compat/examples/re-export/d.ts demotes are query-independent.
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
        // Query-independent: a test-shaped query does not lift the compat demote.
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
        // `com.example.*` Java/Kotlin namespace must NOT trigger the examples
        // penalty; we only match plural forms.
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
        // "compatibility" should not match "compat" as a directory segment.
        assert_penalty("src/compatibility/check.rs", 1.0);
        // "examplesite" should not match "examples".
        assert_penalty("src/examplesite/index.ts", 1.0);
        // "test_helpers" file in src/ should NOT trigger (no test-like glob match).
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
        // "testimony" contains "test" as a prefix but is not a whole token.
        assert!(!query_is_test_shaped("testimony"));
        // "contest" similarly.
        assert!(!query_is_test_shaped("contest results"));
    }

    #[test]
    fn non_test_query_demotes_tests_harder() {
        // 0.3 (baseline) * 0.5 (extra) = 0.15
        let p = path_penalty_for_query("tests/integration.rs", false);
        assert!((p - 0.15).abs() < 1e-6, "got {}", p);
        let p = path_penalty_for_query("src/__tests__/login.test.ts", false);
        assert!((p - 0.15).abs() < 1e-6, "got {}", p);
    }

    #[test]
    fn test_query_lifts_test_penalty() {
        // Test-shaped query → no test-like demote (compete on merit).
        assert_eq!(path_penalty_for_query("tests/integration.rs", true), 1.0);
        assert_eq!(
            path_penalty_for_query("src/__tests__/login.test.ts", true),
            1.0
        );
        // Compat/examples/d.ts demotes still apply regardless (query-independent).
        assert!(
            (path_penalty_for_query("src/compat/php7.php", true) - 0.3).abs() < 1e-6,
            "compat dir should still demote on test queries"
        );
    }

    #[test]
    fn axios_interceptor_failure_mode_repro() {
        // §2.11 finding: query "request and response interceptors" surfaced
        // 9 test files in top-10, all with "interceptor" in path. Even
        // post-0.3x demote they beat the production target. With the new
        // 0.15x extra-demote on non-test queries the production file should
        // surface above the test cluster.
        //
        // Synthetic candidate set: production target with mediocre similarity
        // (0.65), test files with strong similarity (0.85) because they
        // contain "interceptor" in path and body.
        let mut results = vec![
            mk("tests/browser/interceptors.browser.test.js", 0.85),
            mk("tests/smoke/esm/tests/interceptors.smoke.test.js", 0.84),
            mk("tests/smoke/cjs/tests/interceptors.smoke.test.cjs", 0.83),
            mk("lib/core/InterceptorManager.js", 0.65),
            mk("tests/unit/regression.test.js", 0.60),
        ];

        apply_path_penalties(&mut results, "request and response interceptors");

        // After query-aware demote: tests → 0.85*0.15=0.1275 etc., production
        // stays at 0.65. Production target should now be rank 1.
        assert_eq!(results[0].file_path, "lib/core/InterceptorManager.js");
    }

    #[test]
    fn test_query_does_not_regress_test_for_x_case() {
        // Inverse: user asks "test for InterceptorManager". Test files
        // should NOT be demoted; the test file with the strongest match
        // should win.
        let mut results = vec![
            mk("tests/browser/interceptors.browser.test.js", 0.85),
            mk("lib/core/InterceptorManager.js", 0.80),
            mk("tests/unit/regression.test.js", 0.60),
        ];

        apply_path_penalties(&mut results, "test for InterceptorManager");

        // Test file stays at rank 1; production target stays at rank 2.
        // Without the lift, the production file (0.80) would beat the
        // demoted test file (0.85*0.3=0.255).
        assert_eq!(
            results[0].file_path,
            "tests/browser/interceptors.browser.test.js"
        );
        assert_eq!(results[1].file_path, "lib/core/InterceptorManager.js");
    }

    #[test]
    fn l2_to_score_clamps_negative_similarity_to_zero() {
        use super::l2_to_score;
        // distance > √2 ⇒ negative cosine similarity ⇒ raw formula goes
        // negative. Must clamp to 0 so the multiplicative ranking stages don't
        // invert order on the tail.
        assert_eq!(l2_to_score(2.0), 0.0); // 1 - 4/2 = -1 → 0
        assert_eq!(l2_to_score(1.6), 0.0); // 1 - 2.56/2 = -0.28 → 0
        assert!(l2_to_score(1.41) >= 0.0);
        // Strong matches are unchanged.
        assert!((l2_to_score(0.0) - 1.0).abs() < 1e-6);
        assert!((l2_to_score(1.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn negative_similarity_does_not_invert_path_penalty_ranking() {
        use super::l2_to_score;
        // A weak-match query returns KNN rows past √2 distance (negative cosine
        // similarity). The production file is the *better* match (smaller
        // distance) but is a non-test path, so the test path's 0.15 penalty
        // would multiply its larger-magnitude negative score UP and overtake the
        // production file. Build scores exactly the way search() does.
        let prod_score = l2_to_score(1.5); // better match
        let test_score = l2_to_score(1.6); // worse match
        let mut results = vec![
            mk("src/auth/session.rs", prod_score),
            mk("tests/auth/session_test.rs", test_score),
        ];
        // Non-test-shaped query so the test path gets the 0.15 demote.
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

        // Indexing a new file bumps the semantic_files validity token, which
        // must rebuild the cache entry so the new stem becomes visible.
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
        // 5-word natural-language phrase (the laravel-framework
        // failure case from the §2.10 semble-corpus bench).
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
        // Two words isn't enough for the natlang branch; not a single
        // identifier either. Fall back to default.
        assert_eq!(adaptive_rerank_weight("http server"), RERANK_WEIGHT_DEFAULT);
        // Identifier-shaped but two tokens → not the short_id branch.
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
        // All-lowercase alphabetic single words are natural language, not
        // identifiers — they must not get the reduced cross-encoder weight.
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
        apply_reranking(&mut rerank, QUERY, &mut results, None);

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
        apply_reranking(&mut rerank, QUERY, &mut results, None);

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
        apply_reranking(&mut rerank, QUERY, &mut results, None);
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
        // 5 chunks all from `Queue/Connectors/` mimicking the
        // laravel-framework failure mode. With default threshold=2 and
        // decay=0.75, the first 2 keep their scores and chunks 3-5 decay
        // by 0.75^excess.
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
        // First 2 chunks at the threshold keep their scores.
        assert!((by_path["Queue/Connectors/AConnector.php"] - 0.95).abs() < 1e-6);
        assert!((by_path["Queue/Connectors/BConnector.php"] - 0.94).abs() < 1e-6);
        // Chunks 3-5 decay; absolute value strictly < pre-decay.
        assert!(by_path["Queue/Connectors/CConnector.php"] < 0.93);
        assert!(by_path["Queue/Connectors/DConnector.php"] < 0.92);
        assert!(by_path["Queue/Connectors/EConnector.php"] < 0.91);
        // QueueManager untouched (different dir).
        assert!((by_path["Queue/QueueManager.php"] - 0.80).abs() < 1e-6);
    }

    #[test]
    fn no_penalty_below_threshold() {
        let mut results = vec![mk("src/a.rs", 0.9), mk("src/b.rs", 0.8)];
        let before: Vec<f32> = results.iter().map(|r| r.score).collect();
        apply_directory_saturation(&mut results);
        // 2 chunks from `src/` is at threshold — no decay (decay starts when
        // `already >= THRESHOLD`, i.e. on the 3rd chunk).
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
        // No `/` in the path means parent_dir is `""`. All three are
        // in the same bucket; only the 4th would decay.
        let mut results = vec![
            mk("README.md", 0.9),
            mk("Cargo.toml", 0.8),
            mk("Makefile", 0.7),
        ];
        apply_directory_saturation(&mut results);
        assert_eq!(results.len(), 3); // sanity
    }

    // ---- §2.12 qualified-name boost with anti-trigger filter ----

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
        // The §2.12 false-positive case: query "load default command
        // options..." extracts `default`; chunk has `Mode::default`. The
        // ×2.0 boost is suppressed because `default` matches only the
        // leaf segment of `mode::default`.
        assert!(!qualified_name_matches(
            "default",
            "mode::default",
            "default"
        ));
        // Same for plain `default` (bare name, no separator) — the lone
        // segment is the leaf, no non-leaf to anchor.
        assert!(!qualified_name_matches("default", "default", "default"));
    }

    #[test]
    fn anti_trigger_root_match_does_boost() {
        // The §2.12 true-positive that should NOT regress: query word
        // matches a type/module-level qualified-name segment. Even when
        // the token is in the anti-trigger list (e.g. `config`), a root
        // match indicates the user is asking about that type/module.
        assert!(qualified_name_matches("config", "config::load", "load"));
        assert!(qualified_name_matches("default", "default::clone", "clone"));
    }

    #[test]
    fn non_anti_trigger_any_segment_match_boosts() {
        // Tokens NOT in the anti-trigger list match any segment (leaf
        // included) — these are distinctive enough that lexical match
        // signals real intent. The §2.12 win case (`Ignore::add_child_path`
        // matched on token `ignore`) survives because `ignore` isn't
        // in the anti-trigger list, AND would also pass the anti-trigger
        // stem filter via the root-match path.
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
        // End-to-end regression: the original §2.12 A/B regression case.
        // Query word `default` extracts as known symbol; one chunk has
        // `Mode::default` (leaf-only); the boost must NOT lift it.
        let mut results = vec![
            mk_with_symbols("src/correct.rs", 0.80, vec![]),
            mk_with_symbols("src/wrong.rs", 0.77, vec![("default", "Mode::default")]),
        ];
        apply_qualified_name_boost(&mut results, &["default".to_string()]);
        // wrong.rs stays at 0.77 (no boost — anti-trigger leaf match);
        // correct.rs stays at 0.80 — original ranking preserved.
        assert_eq!(results[0].file_path, "src/correct.rs");
        assert!((results[1].score - 0.77).abs() < 1e-6);
    }

    #[test]
    fn qualified_name_boost_idempotent_per_chunk() {
        // Two matching symbols in one chunk still get ×2.0 once.
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
        SOFT_PENALTY_MILD, apply_version_demote, declaration_header_penalty,
        foreign_platform_penalty, query_names_foreign_platform, query_names_version,
        version_dir_of,
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
        // curl: cfilters.h took rank 1 over the connect.c that implements it.
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
        // nlohmann-json, abseil and fmtlib are header-only: the header IS the
        // implementation, and every C++ target in the semble corpus is one.
        // Demoting here cost C++ 0.134 in simulation.
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
        // libuv's heap-inl.h carries definitions and is a legitimate target.
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
        // Not a version segment: needs digits after the `v`.
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
        // "v3 compatibility error types and ZodError" wants v3 on purpose.
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
        assert!(query_names_foreign_platform("windows named pipe handling"));
        assert!(query_names_foreign_platform("IOCP completion port"));
        // "window size" must not read as Windows intent.
        assert!(!query_names_foreign_platform(
            "tty terminal raw mode and window size"
        ));
    }

    #[test]
    #[cfg_attr(windows, ignore = "rule is host-conditional and inert on Windows")]
    fn demotes_foreign_platform_directories() {
        assert_eq!(foreign_platform_penalty("src/win/tcp.c"), SOFT_PENALTY_MILD);
        assert_eq!(foreign_platform_penalty("src/unix/tcp.c"), 1.0);
        // Substring of a longer segment must not match.
        assert_eq!(foreign_platform_penalty("src/window/tcp.c"), 1.0);
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
        // fmtlib's benchmark root is all `.h`, and its dialect resolves to C
        // because nothing there carries an unambiguous C++ extension. Demoting
        // every candidate but the `-inl.h` exemption promoted format-inl.h to
        // rank 1 and cost three queries.
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
        // curl: connect.c implements what cfilters.h declares.
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
        // `Router` qualifies on mixed case, the same rule that admits the
        // cited `Semaphore` case. A leading capital is not distinguished from
        // an internal one, so sentence-initial words can enter; requiring an
        // internal capital would reject `Semaphore` too. The corpus A/B is
        // what decides whether that extra noise costs anything.
        assert_eq!(
            stem_match_tokens("Router path_router implementation"),
            vec!["pathrouter", "router"]
        );
        // Mixed case with a lowercase letter.
        assert_eq!(
            stem_match_tokens("absl::StrSplit and StrJoin"),
            vec!["strjoin", "strsplit"]
        );
        // Underscore qualifies even though the token is all-caps.
        assert_eq!(
            stem_match_tokens("logging macros ABSL_LOG"),
            vec!["absllog"]
        );
    }

    #[test]
    fn rejects_bare_acronyms_and_plain_words() {
        // Boosting json.hpp on the word JSON regressed 9 nlohmann queries.
        assert!(stem_match_tokens("JSON parser and tokenizer").is_empty());
        assert!(stem_match_tokens("HTTP client request sending").is_empty());
        assert!(stem_match_tokens("how formatters transform log records").is_empty());
        // Too short to be specific.
        assert!(stem_match_tokens("Foo").is_empty());
    }

    #[test]
    fn boosts_the_file_the_query_names() {
        // Covers the helper's own matching, not a gap in the definition
        // boost: for a bare `Semaphore` query that boost already discriminates
        // these two via DEFINITION_FILE_STEM_BONUS. The gap this stage exists
        // to fill is the case where the definition regex cannot fire at all —
        // a C++ free function, or a macro-attributed declaration.
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
        // 1.2x cannot overturn THIS lead. It does not generalize to "the
        // target loses at most one place": the multiplier bounds the score
        // ratio a boost can overcome, so against tightly clustered results a
        // boosted candidate can pass several at once. Measured, the real
        // nlohmann "ADL-based to_json and from_json" query regresses
        // 1.000 -> 0.500, where the target's lead is far under the 0.40 here.
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
        // `Äbc` is 3 characters but 4 UTF-8 bytes; a byte-length gate would
        // admit it through the mixed-case rule despite being under the
        // documented four-character minimum.
        assert!(stem_match_tokens("Äbc").is_empty());
        // Four real characters still qualify.
        assert_eq!(stem_match_tokens("Äbcd"), vec!["äbcd"]);
    }
}

#[cfg(test)]
mod scoped_fts_evidence_tests {
    use super::build_fts_match_query;

    #[test]
    fn namespace_components_never_reach_the_match_expression() {
        // The premise behind "scope-qualified terms are OR-joined, diluting
        // the signal" does not hold: a lowercase namespace prefix is not
        // code-shaped, so it is filtered before any OR-join. There is no
        // `absl` term to conjoin with `StrSplit`.
        for (query, expect_absent) in [
            ("absl::StrSplit for splitting", "absl"),
            ("how fmt::format works", "fmt"),
            ("std::vector usage", "std"),
            ("call ModuleRef::create", "create"),
        ] {
            let q = build_fts_match_query(query);
            assert!(
                !q.contains(&format!("\"{expect_absent}\"")),
                "{query:?} produced {q:?}, which still carries {expect_absent:?}"
            );
        }
    }

    #[test]
    fn a_lowercase_namespace_query_yields_only_the_identifier_terms() {
        // One mechanical case, not a corpus-wide claim: with a lowercase
        // namespace the surviving terms are the identifiers the query names.
        let q = build_fts_match_query("absl::StrSplit and StrJoin for splitting and joining");
        assert!(q.contains("\"StrSplit\""));
        assert!(q.contains("\"StrJoin\""));
        assert!(!q.contains("\"absl\""));
    }

    #[test]
    fn every_corpus_query_carrying_a_scope_is_unchanged_by_the_suppression() {
        // These are the 20 semble queries (of 1251) that carry a `::` in a
        // CodeSage-supported language, verbatim. Every prefix is plain
        // lowercase, so the code-shape filter already dropped it before this
        // change and the emitted terms must be identical to the old behavior.
        // Pinned because a full benchmark arm over these two repos costs ~16
        // minutes and this settles the same question in milliseconds.
        for (query, expect) in [
            (
                "absl::StrCat and StrAppend for efficient string",
                "\"StrCat\" OR \"StrAppend\"",
            ),
            (
                "absl::string_view for non-owning string references",
                "\"string_view\"",
            ),
            (
                "absl::flat_hash_map and flat_hash_set hash tables",
                "\"flat_hash_map\" OR \"flat_hash_set\"",
            ),
            ("how fmt::format and fmt::print format strings", ""),
            // A lowercase tail is filtered like any lowercase token, before
            // and after — the suppression never gets a selective tail to keep.
            ("std::filesystem path formatting support", ""),
        ] {
            assert_eq!(
                build_fts_match_query(query),
                expect,
                "query {query:?} must emit the same terms as before the change"
            );
        }
    }

    #[test]
    fn a_code_shaped_namespace_component_is_dropped_leaving_the_tail() {
        // `absl`/`fmt`/`std` are filtered for being plain lowercase, not for
        // being namespaces — so a namespace carrying an underscore or a
        // capital used to reach the disjunction and dilute it. It now emits a
        // phrase plus the tail, and no standalone prefix term.
        let q = build_fts_match_query("foo_bar::Thing lookup");
        assert!(q.contains("\"Thing\""), "tail stays selectable: {q:?}");
        assert!(
            !q.contains("\"foo_bar\""),
            "prefix must not remain a term of its own: {q:?}"
        );

        // PHP backslash-qualified names are the clearest instance: every
        // component is capitalised, so before this all of them survived and
        // the two namespace components outvoted the class.
        let q = build_fts_match_query("Illuminate\\Routing\\Router dispatch");
        assert!(q.contains("\"Router\""), "the class survives: {q:?}");
        assert!(!q.contains("\"Illuminate\""), "got {q:?}");
        assert!(!q.contains("\"Routing\""), "got {q:?}");
    }

    #[test]
    fn a_suppressed_prefix_is_still_dropped_when_it_repeats_elsewhere() {
        // The prefix is suppressed by name, so a later standalone mention does
        // not smuggle it back in as its own OR term.
        let q = build_fts_match_query("Illuminate\\Routing\\Router and Illuminate helpers");
        assert!(!q.contains("\"Illuminate\""), "got {q:?}");
        assert!(q.contains("\"Router\""));
    }

    #[test]
    fn the_dotted_pair_route_bypasses_the_code_shape_filter() {
        // extract_dotted_identifier_tokens runs BEFORE the filter, so a
        // lowercase dotted pair reaches the disjunction where the same
        // components behind `::` would not.
        let q = build_fts_match_query("fix moduleref.create edge case");
        assert!(q.contains("\"moduleref\""));
        assert!(
            q.contains("\"create\""),
            "dotted route admits lowercase: {q:?}"
        );
    }
}
