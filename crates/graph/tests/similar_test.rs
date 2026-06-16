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

    let hits = find_similar(&db, "alpha", 0.8, 10).unwrap();
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

#[test]
fn find_similar_unknown_symbol_is_empty() {
    let (_dir, db) = setup();
    assert!(find_similar(&db, "no_such_fn", 0.8, 10).unwrap().is_empty());
}
