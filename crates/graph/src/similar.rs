//! Near-clone detection over stored MinHash fingerprints.
//!
//! `find_similar` loads every function fingerprint, builds an LSH band index
//! (so we score O(candidates) instead of O(n²) all-pairs), and returns the
//! functions structurally closest to a named target, ranked by Jaccard.
//! Identifiers and literals are ignored — this matches code *shape*, which is
//! what surfaces copy-paste and divergent forks.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use codesage_parser::fingerprint::{Fingerprint, band_keys, jaccard};
use codesage_protocol::{FileCategory, SimilarSymbol};
use codesage_storage::Database;

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

    let all = db.all_fingerprints()?;
    let targets: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name == symbol_name)
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // LSH band index over non-test fingerprints, keyed by (language, band key).
    // Tree-sitter `kind_id`s are grammar-local, so fingerprints only compare
    // within a language — bucketing on language prevents cross-language
    // coincidental matches with meaningless Jaccard scores.
    let mut buckets: HashMap<(&str, u64), Vec<usize>> = HashMap::new();
    for (i, f) in all.iter().enumerate() {
        if matches!(FileCategory::classify(&f.file_path), FileCategory::Test) {
            continue;
        }
        if let Some(sig) = as_sig(&f.fp) {
            for key in band_keys(sig) {
                buckets
                    .entry((f.language.as_str(), key))
                    .or_default()
                    .push(i);
            }
        }
    }

    // Best score per distinct (file, line) clone location.
    let mut best: HashMap<(String, u32), SimilarSymbol> = HashMap::new();
    for &ti in &targets {
        let target = &all[ti];
        let Some(tsig) = as_sig(&target.fp) else {
            continue;
        };
        let mut candidates = HashSet::new();
        for key in band_keys(tsig) {
            if let Some(ids) = buckets.get(&(target.language.as_str(), key)) {
                candidates.extend(ids.iter().copied());
            }
        }
        for ci in candidates {
            let c = &all[ci];
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
    out.sort_by(|a, b| {
        b.jaccard
            .partial_cmp(&a.jaccard)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    Ok(out)
}
