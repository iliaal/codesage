//! Near-clone detection using same-language MinHash fingerprints and LSH candidates.
//! Fingerprints compare code structure, ignoring identifiers and literals.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use codesage_parser::fingerprint::{Fingerprint, band_keys, jaccard};
use codesage_protocol::{FileCategory, FindSimilarResults, SimilarSymbol};
use codesage_storage::Database;
use codesage_storage::db::StoredFingerprint;

use crate::resolver::{ResolveOptions, matched_symbols, resolve_symbols};

type FingerprintToken = (u64, u64, i64);
type FingerprintCache = HashMap<String, (FingerprintToken, Arc<Vec<StoredFingerprint>>)>;

static FINGERPRINT_CACHE: LazyLock<Mutex<FingerprintCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn as_sig(fp: &[u64]) -> Option<&Fingerprint> {
    fp.try_into().ok()
}

/// Functions structurally similar to `symbol_name`, Jaccard ≥ `min_jaccard`,
/// capped at `limit`. Excludes test files and the target's own occurrence.
///
/// `symbol_name` takes the whole target grammar (see [`crate::resolver`]).
/// Fingerprints are stored by bare name, so the rows are keyed on the bare
/// name of whatever the input resolved to: a `sym:` handle or `Foo::run`
/// finds the same clones as `run`. Rows are the union across every
/// definition sharing that name, which is the answer a clone search wants;
/// `target` discloses when there was more than one, with a handle per
/// definition. An input that resolves to nothing falls back to the raw
/// spelling, so a fingerprint with no indexed symbol row is still reachable.
pub fn find_similar(
    db: &Database,
    symbol_name: &str,
    min_jaccard: f32,
    limit: usize,
) -> Result<FindSimilarResults> {
    let (target, symbols) = resolve_symbols(db, symbol_name, ResolveOptions::symbol())?;
    // NaN would bypass the score threshold comparison.
    let min_jaccard = if min_jaccard.is_finite() {
        min_jaccard.clamp(0.0, 1.0)
    } else {
        0.85
    };

    let matched = matched_symbols(&target, symbols);
    let mut names: Vec<&str> = matched.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        names.push(symbol_name.trim());
    }
    let mut targets = Vec::new();
    for name in names {
        targets.extend(db.fingerprints_named(name)?);
    }
    if targets.is_empty() {
        return Ok(FindSimilarResults {
            results: Vec::new(),
            target: Some(target),
        });
    }

    // Tree-sitter kind IDs are grammar-local; compare only within one language.
    let mut language_indexes: HashMap<String, LanguageFingerprintIndex> = HashMap::new();

    let mut best: HashMap<(String, u32), SimilarSymbol> = HashMap::new();
    for seed in &targets {
        let Some(tsig) = as_sig(&seed.fp) else {
            continue;
        };
        if !language_indexes.contains_key(&seed.language) {
            let rows = fingerprints_for_language_cached(db, &seed.language)?;
            language_indexes.insert(seed.language.clone(), build_language_index(rows));
        }
        let Some(index) = language_indexes.get(&seed.language) else {
            continue;
        };
        let mut candidates: HashSet<usize> = HashSet::new();
        for key in band_keys(tsig) {
            if let Some(ids) = index.buckets.get(&key) {
                candidates.extend(ids.iter().copied());
            }
        }
        for ci in candidates {
            let c = &index.rows[ci];
            if c.file_path == seed.file_path && c.line_start == seed.line_start {
                continue;
            }
            let Some(csig) = as_sig(&c.fp) else {
                continue;
            };
            let score = jaccard(tsig, csig);
            if score < min_jaccard {
                continue;
            }
            let entry = best
                .entry((c.file_path.clone(), c.line_start))
                .or_insert_with(|| SimilarSymbol {
                    name: c.name.clone(),
                    file_path: c.file_path.clone(),
                    line_start: c.line_start,
                    line_end: c.line_end,
                    kind: c.kind.clone(),
                    jaccard: 0.0,
                });
            if score > entry.jaccard {
                entry.jaccard = score;
            }
        }
    }

    let mut out: Vec<SimilarSymbol> = best.into_values().collect();
    // Break ties before truncation so HashMap iteration cannot change the result set.
    // `total_cmp` remains transitive even for NaN.
    out.sort_by(|a, b| {
        b.jaccard
            .total_cmp(&a.jaccard)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.line_start.cmp(&b.line_start))
    });
    out.truncate(limit);
    Ok(FindSimilarResults {
        results: out,
        target: Some(target),
    })
}

struct LanguageFingerprintIndex {
    rows: Arc<Vec<StoredFingerprint>>,
    buckets: HashMap<u64, Vec<usize>>,
}

fn build_language_index(rows: Arc<Vec<StoredFingerprint>>) -> LanguageFingerprintIndex {
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in rows.iter().enumerate() {
        if matches!(FileCategory::classify(&f.file_path), FileCategory::Test) {
            continue;
        }
        if let Some(sig) = as_sig(&f.fp) {
            for key in band_keys(sig) {
                buckets.entry(key).or_default().push(i);
            }
        }
    }
    LanguageFingerprintIndex { rows, buckets }
}

/// Bound per-language snapshots retained for the daemon's lifetime.
const FINGERPRINT_CACHE_CAP: usize = 32;

/// Recover poisoned cache state; validity tokens still reject stale snapshots.
fn lock_fingerprint_cache() -> std::sync::MutexGuard<'static, FingerprintCache> {
    FINGERPRINT_CACHE.lock().unwrap_or_else(|e| {
        tracing::warn!("fingerprint cache lock poisoned; recovering with guarded state");
        e.into_inner()
    })
}

fn fingerprints_for_language_cached(
    db: &Database,
    language: &str,
) -> Result<Arc<Vec<StoredFingerprint>>> {
    let (Some(key), Some(token)) = (db.fingerprint_cache_key(), db.structural_cache_token()?)
    else {
        return Ok(Arc::new(db.fingerprints_for_language(language)?));
    };
    let key = format!("{key}::{language}");
    if let Some((_, cached)) = lock_fingerprint_cache()
        .get(&key)
        .filter(|(cached_token, _)| *cached_token == token)
    {
        return Ok(Arc::clone(cached));
    }

    let all = Arc::new(db.fingerprints_for_language(language)?);
    let mut cache = lock_fingerprint_cache();
    if cache.len() >= FINGERPRINT_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, (token, Arc::clone(&all)));
    Ok(all)
}
