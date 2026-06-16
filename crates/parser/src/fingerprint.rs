//! Spike (CBM port): MinHash fingerprints over AST node-kind trigrams for
//! near-clone detection. Mirrors codebase-memory-mcp's `simhash/minhash.h`:
//! K=64 permutations, a leaf-token floor, and LSH banding for O(n) candidate
//! generation instead of O(n²) all-pairs.
//!
//! Fingerprints are only comparable *within a language* — tree-sitter
//! `kind_id`s are grammar-local integers, so a `block` in C and a `block` in
//! PHP need not share an id. Cross-language clone detection is out of scope
//! for this spike (and rarely what an agent wants anyway).

use std::collections::HashSet;

use codesage_protocol::{Language, SymbolKind};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor, Tree};

/// MinHash permutations. 64 gives ~±0.12 std error on the Jaccard estimate,
/// matching CBM's K and sufficient to separate clones at a 0.8 threshold.
pub const MINHASH_K: usize = 64;

/// Minimum leaf (token) nodes a function needs before we fingerprint it.
/// Below this the trigram set is too small for a stable estimate and every
/// one-line getter collides with every other. ~30 leaves ≈ BigCloneBench's
/// 50-raw-token floor.
pub const MIN_LEAF_NODES: usize = 30;

/// LSH banding: `MINHASH_K = LSH_BANDS * LSH_ROWS`. Two fingerprints become
/// candidates when they agree on every row of at least one band. 16×4 has a
/// ~50% trip probability around Jaccard 0.7, climbing sharply above that.
pub const LSH_BANDS: usize = 16;
pub const LSH_ROWS: usize = MINHASH_K / LSH_BANDS;

pub type Fingerprint = [u64; MINHASH_K];

/// One fingerprinted function/method.
#[derive(Debug, Clone)]
pub struct FunctionFingerprint {
    pub name: String,
    pub kind: SymbolKind,
    pub line_start: u32,
    pub line_end: u32,
    pub leaf_count: usize,
    pub fp: Fingerprint,
}

#[inline]
const fn splitmix64(x: u64) -> u64 {
    let x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

const SEEDS: [u64; MINHASH_K] = {
    let mut s = [0u64; MINHASH_K];
    let mut i = 0;
    while i < MINHASH_K {
        s[i] = splitmix64(i as u64 + 1);
        i += 1;
    }
    s
};

/// Pre-order walk of `node`, pushing each node's `kind_id` and counting leaves
/// (childless nodes — the actual source tokens). Order is preserved so the
/// trigram sequence reflects structure, not just a node-kind bag.
fn collect_preorder(node: &Node, kinds: &mut Vec<u16>, leaves: &mut usize) {
    kinds.push(node.kind_id());
    let mut cursor = node.walk();
    let mut had_child = false;
    for child in node.children(&mut cursor) {
        had_child = true;
        collect_preorder(&child, kinds, leaves);
    }
    if !had_child {
        *leaves += 1;
    }
}

/// MinHash of a pre-order node-kind sequence, hashing consecutive trigrams.
fn minhash(kinds: &[u16]) -> Fingerprint {
    let mut mins = [u64::MAX; MINHASH_K];
    for w in kinds.windows(3) {
        let v = ((w[0] as u64) << 32) | ((w[1] as u64) << 16) | (w[2] as u64);
        for i in 0..MINHASH_K {
            let h = splitmix64(v ^ SEEDS[i]);
            if h < mins[i] {
                mins[i] = h;
            }
        }
    }
    mins
}

/// Fingerprint a single definition subtree. `None` when the body is too small
/// to fingerprint reliably (see [`MIN_LEAF_NODES`]).
pub fn fingerprint_def(def: &Node) -> Option<(Fingerprint, usize)> {
    let mut kinds = Vec::new();
    let mut leaves = 0usize;
    collect_preorder(def, &mut kinds, &mut leaves);
    if leaves < MIN_LEAF_NODES || kinds.len() < 3 {
        return None;
    }
    Some((minhash(&kinds), leaves))
}

/// Fingerprint every function/method definition in a parsed file. Reuses the
/// same compiled symbol query and pattern→kind map as `extract_symbols`, so
/// the set of fingerprinted defs matches the set of indexed function symbols.
pub fn file_fingerprints(
    tree: &Tree,
    source: &[u8],
    language: Language,
) -> Vec<FunctionFingerprint> {
    let spec = crate::extract::symbol_query_for(language);
    let kind_map = crate::extract::kind_map_for(language);

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&spec.query, root, source);

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    while let Some(m) = matches.next() {
        let Some(kind) = kind_map(m.pattern_index) else {
            continue;
        };
        if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        let (Some(def_cap), Some(name_cap)) = (
            m.captures.iter().find(|c| c.index == spec.def_idx),
            m.captures.iter().find(|c| c.index == spec.name_idx),
        ) else {
            continue;
        };
        let def = def_cap.node;
        if !seen.insert((def.start_byte(), def.end_byte())) {
            continue;
        }
        let Some((fp, leaves)) = fingerprint_def(&def) else {
            continue;
        };
        // Normalize the captured name the same way `extract_symbols` does, so a
        // fingerprint's name matches what `find_symbol` stores (C++ out-of-line
        // `Foo::bar` captures as bare `bar`). Otherwise `find_similar bar` would
        // miss C++ methods.
        let captured = name_cap.node.utf8_text(source).unwrap_or("");
        let name = if language == Language::Cpp {
            crate::extract::cpp_bare_name(captured)
        } else {
            captured.to_string()
        };
        out.push(FunctionFingerprint {
            name,
            kind,
            line_start: def.start_position().row as u32 + 1,
            line_end: def.end_position().row as u32 + 1,
            leaf_count: leaves,
            fp,
        });
    }
    out
}

/// Estimated Jaccard similarity: fraction of MinHash positions that agree.
#[inline]
pub fn jaccard(a: &Fingerprint, b: &Fingerprint) -> f32 {
    let eq = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
    eq as f32 / MINHASH_K as f32
}

/// LSH band signatures for candidate bucketing. Two functions that share any
/// band key are worth scoring exactly; the rest are never compared.
pub fn band_keys(fp: &Fingerprint) -> [u64; LSH_BANDS] {
    let mut keys = [0u64; LSH_BANDS];
    for (b, key) in keys.iter_mut().enumerate() {
        // FNV-1a over the band's rows.
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for r in 0..LSH_ROWS {
            h ^= fp[b * LSH_ROWS + r];
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Mix the band index in so identical row-runs in different bands don't
        // collide into one bucket.
        *key = h ^ splitmix64(b as u64);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_file;

    fn fps(src: &str, lang: Language) -> Vec<FunctionFingerprint> {
        let tree = parse_file(src.as_bytes(), lang).unwrap();
        file_fingerprints(&tree, src.as_bytes(), lang)
    }

    #[test]
    fn near_clone_scores_high_unrelated_scores_low() {
        // Two structurally identical functions differing only in identifiers
        // and literals; one structurally unrelated function.
        let src = r#"
fn alpha(items: &[i32]) -> i32 {
    let mut total = 0;
    for it in items {
        if *it > 0 {
            total += *it * 2;
        } else {
            total -= 1;
        }
    }
    total
}

fn beta(values: &[i32]) -> i32 {
    let mut acc = 0;
    for v in values {
        if *v > 0 {
            acc += *v * 2;
        } else {
            acc -= 1;
        }
    }
    acc
}

fn gamma(name: &str) -> String {
    let mut out = String::new();
    out.push_str("hello ");
    out.push_str(name);
    out.push('!');
    out
}
"#;
        let f = fps(src, Language::Rust);
        assert_eq!(f.len(), 3, "expected 3 functions, got {}", f.len());
        let alpha = f.iter().find(|x| x.name == "alpha").unwrap();
        let beta = f.iter().find(|x| x.name == "beta").unwrap();
        let gamma = f.iter().find(|x| x.name == "gamma").unwrap();

        let clone = jaccard(&alpha.fp, &beta.fp);
        let unrelated = jaccard(&alpha.fp, &gamma.fp);
        assert!(
            clone > 0.85,
            "near-clone Jaccard should be high, got {clone}"
        );
        assert!(
            clone > unrelated + 0.3,
            "clone ({clone}) should clearly beat unrelated ({unrelated})"
        );

        // LSH must bucket the clones together and (very likely) not the
        // unrelated one.
        let ka = band_keys(&alpha.fp);
        let kb = band_keys(&beta.fp);
        assert!(
            ka.iter().any(|k| kb.contains(k)),
            "clones should share at least one LSH band"
        );
    }

    #[test]
    fn identical_bodies_are_jaccard_one() {
        let src = r#"
fn one(x: i32) -> i32 { let mut s = 0; for i in 0..x { s += i; if s > 100 { break; } } s }
fn two(x: i32) -> i32 { let mut s = 0; for i in 0..x { s += i; if s > 100 { break; } } s }
"#;
        let f = fps(src, Language::Rust);
        assert_eq!(f.len(), 2);
        assert!((jaccard(&f[0].fp, &f[1].fp) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tiny_functions_are_skipped() {
        // A trivial getter is below the leaf floor and should not fingerprint.
        let src = "fn id(x: i32) -> i32 { x }\n";
        assert!(fps(src, Language::Rust).is_empty());
    }
}
