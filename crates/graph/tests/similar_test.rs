use codesage_graph::{find_similar, full_index};
use codesage_storage::Database;

/// Two structurally identical functions (differing only in identifiers and
/// literals) plus one unrelated function. `find_similar` on one clone should
/// surface the other and not the unrelated function.
fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha(items: &[i32]) -> i32 {\n    let mut total = 0;\n    for it in items {\n        if *it > 0 {\n            total += *it * 2;\n        } else {\n            total -= 1;\n        }\n    }\n    total\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.rs"),
        b"pub fn beta(values: &[i32]) -> i32 {\n    let mut acc = 0;\n    for v in values {\n        if *v > 0 {\n            acc += *v * 2;\n        } else {\n            acc -= 1;\n        }\n    }\n    acc\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.rs"),
        b"pub fn gamma(name: &str) -> String {\n    let mut out = String::new();\n    out.push_str(\"hello \");\n    out.push_str(name);\n    out.push('!');\n    out.push_str(\" and welcome\");\n    out\n}\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

#[test]
fn find_similar_surfaces_clone_not_unrelated() {
    let (_dir, db) = setup();

    let hits = find_similar(&db, "alpha", 0.8, 10).unwrap().results;
    assert!(
        hits.iter()
            .any(|h| h.name == "beta" && h.file_path == "b.rs"),
        "expected beta as a clone of alpha, got {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.name == "gamma"),
        "unrelated gamma must not be reported as a clone: {hits:?}"
    );
    let beta = hits.iter().find(|h| h.name == "beta").unwrap();
    assert!(
        beta.jaccard >= 0.8,
        "clone jaccard too low: {}",
        beta.jaccard
    );
}

/// A `sym:` handle resolves through the shared grammar and keys the
/// fingerprint lookup on the definition's bare name, so it returns the rows
/// the bare name does.
#[test]
fn find_similar_accepts_a_handle_and_matches_the_bare_name() {
    let (_dir, db) = setup();
    let rows = |results: &[codesage_protocol::SimilarSymbol]| -> Vec<(String, String, u32)> {
        results
            .iter()
            .map(|h| (h.name.clone(), h.file_path.clone(), h.line_start))
            .collect()
    };

    let by_name = find_similar(&db, "alpha", 0.8, 10).unwrap();
    assert!(!by_name.results.is_empty(), "fixture must yield a clone");

    let by_handle = find_similar(&db, "sym:a.rs#alpha", 0.8, 10).unwrap();
    assert_eq!(rows(&by_handle.results), rows(&by_name.results));
    let target = by_handle
        .target
        .expect("every row set carries its resolution");
    assert_eq!(
        target.sole().map(|c| c.handle.as_str()),
        Some("sym:a.rs#alpha"),
        "{target:?}"
    );

    // A guessed lead is not a definition: the raw spelling names no
    // fingerprint, so the rows stay empty while `target` carries the lead.
    let guessed = find_similar(&db, "Alpha", 0.8, 10).unwrap();
    assert!(guessed.results.is_empty(), "{:?}", guessed.results);
    assert!(guessed.target.expect("target").guessed());
}

#[test]
fn find_similar_unknown_symbol_is_empty() {
    let (_dir, db) = setup();
    let found = find_similar(&db, "no_such_fn", 0.8, 10).unwrap();
    assert!(found.results.is_empty());
    let target = found.target.expect("every row set carries its resolution");
    assert!(target.resolved.is_empty(), "{target:?}");
}
