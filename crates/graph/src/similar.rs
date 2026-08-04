//! Near-clone detection over stored MinHash fingerprints.
//!
//! `find_similar` resolves the target function fingerprint by name, loads
//! same-language fingerprints, builds an LSH band index (so we score
//! O(candidates) instead of O(n²) all-pairs), and returns the functions
//! structurally closest to the target, ranked by Jaccard.
//! Identifiers and literals are ignored — this matches code *shape*, which is
//! what surfaces copy-paste and divergent forks.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use codesage_parser::fingerprint::{Fingerprint, band_keys, jaccard};
use codesage_protocol::{FileCategory, SimilarSymbol};
use codesage_storage::Database;
use codesage_storage::db::StoredFingerprint;

type FingerprintToken = (i64, i64, i64, i64);
type FingerprintCache = HashMap<String, (FingerprintToken, Arc<Vec<StoredFingerprint>>)>;

static FINGERPRINT_CACHE: LazyLock<Mutex<FingerprintCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn as_sig(fp: &[u64]) -> Option<&Fingerprint> {
    fp.try_into().ok()
}

/// Functions structurally similar to `symbol_name`, Jaccard ≥ `min_jaccard`,
/// capped at `limit`. Test files are excluded from the candidate set (clone
/// scaffolding there is noise, not actionable). The target's own occurrence is
/// never returned as its own clone.
pub fn find_similar(
    db: &Database,
    symbol_name: &str,
    min_jaccard: f32,
    limit: usize,
) -> Result<Vec<SimilarSymbol>> {
    // Guard against NaN / out-of-range thresholds: NaN would bypass the `<`
    // filter (admitting everything) and a negative bound admits every LSH
    // candidate.
    let min_jaccard = if min_jaccard.is_finite() {
        min_jaccard.clamp(0.0, 1.0)
    } else {
        0.85
    };

    let targets = db.fingerprints_named(symbol_name)?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // LSH band indexes over non-test fingerprints, keyed by language then band.
    // Tree-sitter `kind_id`s are grammar-local, so fingerprints only compare
    // within a language.
    let mut language_indexes: HashMap<String, LanguageFingerprintIndex> = HashMap::new();

    // Best score per distinct (file, line) clone location.
    let mut best: HashMap<(String, u32), SimilarSymbol> = HashMap::new();
    for target in &targets {
        let Some(tsig) = as_sig(&target.fp) else {
            continue;
        };
        if !language_indexes.contains_key(&target.language) {
            let rows = fingerprints_for_language_cached(db, &target.language)?;
            language_indexes.insert(target.language.clone(), build_language_index(rows));
        }
        let Some(index) = language_indexes.get(&target.language) else {
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
            // Skip the target's own occurrence(s).
            if c.file_path == target.file_path && c.line_start == target.line_start {
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
    // Ties are the norm, not the exception — exact clones all score 1.0 — and
    // `best` is a HashMap, so without a total order the truncation below keeps
    // an arbitrary subset that changes between identical calls. `total_cmp`
    // rather than `partial_cmp().unwrap_or(Equal)`: the latter is intransitive
    // if a NaN ever reaches it, which can panic the sort and hang the client.
    out.sort_by(|a, b| {
        b.jaccard
            .total_cmp(&a.jaccard)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.line_start.cmp(&b.line_start))
    });
    out.truncate(limit);
    Ok(out)
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

fn fingerprints_for_language_cached(
    db: &Database,
    language: &str,
) -> Result<Arc<Vec<StoredFingerprint>>> {
    let Some(key) = db.fingerprint_cache_key() else {
        return Ok(Arc::new(db.fingerprints_for_language(language)?));
    };
    let key = format!("{key}::{language}");
    let token = db.fingerprint_validity_token()?;
    if let Some((_, cached)) = FINGERPRINT_CACHE
        .lock()
        .expect("fingerprint cache lock poisoned")
        .get(&key)
        .filter(|(cached_token, _)| *cached_token == token)
    {
        return Ok(Arc::clone(cached));
    }

    let all = Arc::new(db.fingerprints_for_language(language)?);
    FINGERPRINT_CACHE
        .lock()
        .expect("fingerprint cache lock poisoned")
        .insert(key, (token, Arc::clone(&all)));
    Ok(all)
}
