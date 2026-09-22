use codesage_graph::{assess_risk, assess_risk_batch, assess_risk_diff, find_similar, index_files};
use codesage_parser::discover::content_hash;
use codesage_protocol::{FileInfo, Language};
use codesage_storage::Database;
use std::path::Path;

fn index_source(root: &Path, db: &Database, path: &str, language: Language, source: &str) {
    std::fs::write(root.join(path), source).unwrap();
    let file = FileInfo {
        path: path.into(),
        language,
        content_hash: content_hash(source.as_bytes()),
        is_test: false,
    };
    index_files(root, db, &[file], false).unwrap();
}

fn seed_cycle(root: &Path, db: &Database) {
    for (path, source) in [
        ("a.php", "<?php\nnamespace App;\nuse App\\B;\nclass A {}\n"),
        ("c.php", "<?php\nnamespace App;\nclass C {}\n"),
        ("z.php", "<?php\nnamespace App;\nuse App\\A;\nclass B {}\n"),
    ] {
        index_source(root, db, path, Language::Php, source);
    }
}

fn assert_cycle_consumers(db: &Database, expected: bool) {
    let paths = vec!["a.php".to_string()];
    let risks = [
        assess_risk(db, "a.php").unwrap(),
        assess_risk_batch(db, &paths).unwrap().files.remove(0),
        assess_risk_diff(db, &paths).unwrap().files.remove(0),
    ];
    for risk in risks {
        assert_eq!(risk.in_cycle, expected, "{}", risk.file);
        assert_eq!(risk.cycle_size, if expected { 2 } else { 0 });
    }
}

#[test]
fn cache_invalidation_risk_import_retarget_same_and_cross_connection() {
    for cross_connection in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index.db");
        let reader = Database::open(&path).unwrap();
        seed_cycle(root.path(), &reader);
        let writer = Database::open(&path).unwrap();
        let writer = if cross_connection { &writer } else { &reader };
        assert_cycle_consumers(&reader, true);
        let shallow = reader.import_cycle_validity_token().unwrap();
        index_source(
            root.path(),
            writer,
            "z.php",
            Language::Php,
            "<?php\nnamespace App;\nuse App\\C;\nclass B {}\n",
        );
        assert_eq!(reader.import_cycle_validity_token().unwrap(), shallow);
        assert_cycle_consumers(&reader, false);
    }
}

const CLONE: &str = "pub fn alpha(items: &[i32]) -> i32 {\n    let mut total = 0;\n    for it in items {\n        if *it > 0 {\n            total += *it * 2;\n        } else {\n            total -= 1;\n        }\n    }\n    total\n}\n";

fn seed_similar(root: &Path, db: &Database) {
    index_source(root, db, "a.rs", Language::Rust, CLONE);
    index_source(
        root,
        db,
        "z.rs",
        Language::Rust,
        &CLONE.replace("alpha", "bravo"),
    );
}

fn similar_names(db: &Database) -> Vec<String> {
    find_similar(db, "alpha", 1.0, 10)
        .unwrap()
        .into_iter()
        .map(|hit| hit.name)
        .collect()
}

#[test]
fn cache_invalidation_similar_rename_same_and_cross_connection() {
    for cross_connection in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index.db");
        let reader = Database::open(&path).unwrap();
        seed_similar(root.path(), &reader);
        let writer = Database::open(&path).unwrap();
        let writer = if cross_connection { &writer } else { &reader };
        assert_eq!(similar_names(&reader), ["bravo"]);
        let shallow = reader.fingerprint_validity_token().unwrap();
        index_source(
            root.path(),
            writer,
            "z.rs",
            Language::Rust,
            &CLONE.replace("alpha", "delta"),
        );
        assert_eq!(reader.fingerprint_validity_token().unwrap(), shallow);
        assert_eq!(similar_names(&reader), ["delta"]);
    }
}

#[test]
fn cache_invalidation_similar_equal_leaf_structural_edit() {
    for cross_connection in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index.db");
        let reader = Database::open(&path).unwrap();
        seed_similar(root.path(), &reader);
        let writer = Database::open(&path).unwrap();
        let writer = if cross_connection { &writer } else { &reader };
        assert_eq!(similar_names(&reader), ["bravo"]);
        let before = reader.fingerprints_named("bravo").unwrap().remove(0);
        let shallow = reader.fingerprint_validity_token().unwrap();
        let changed = CLONE
            .replace("alpha", "bravo")
            .replace(" > ", " == ")
            .replace(" * 2", " / 2");
        index_source(root.path(), writer, "z.rs", Language::Rust, &changed);
        let after = reader.fingerprints_named("bravo").unwrap().remove(0);
        assert_eq!(after.leaf_count, before.leaf_count);
        assert_ne!(after.fp, before.fp);
        assert_eq!(reader.fingerprint_validity_token().unwrap(), shallow);
        assert!(
            similar_names(&reader).is_empty(),
            "edited function is no longer an exact structural clone"
        );
    }
}

#[test]
fn cache_invalidation_replaced_database_does_not_reuse_path_cache() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.db");
    let reader = Database::open(&path).unwrap();
    seed_cycle(root.path(), &reader);
    seed_similar(root.path(), &reader);
    assert_cycle_consumers(&reader, true);
    assert_eq!(similar_names(&reader), ["bravo"]);
    let cycle_token = reader.import_cycle_validity_token().unwrap();
    let fingerprint_token = reader.fingerprint_validity_token().unwrap();
    drop(reader);

    let replacement = root.path().join("replacement.db");
    let writer = Database::open(&replacement).unwrap();
    for (file, source) in [
        ("a.php", "<?php\nnamespace App;\nuse App\\B;\nclass A {}\n"),
        ("c.php", "<?php\nnamespace App;\nclass C {}\n"),
        ("z.php", "<?php\nnamespace App;\nuse App\\C;\nclass B {}\n"),
    ] {
        index_source(root.path(), &writer, file, Language::Php, source);
    }
    index_source(root.path(), &writer, "a.rs", Language::Rust, CLONE);
    index_source(
        root.path(),
        &writer,
        "z.rs",
        Language::Rust,
        &CLONE.replace("alpha", "delta"),
    );
    drop(writer);
    std::fs::rename(&replacement, &path).unwrap();

    let reader = Database::open(&path).unwrap();
    assert_eq!(reader.import_cycle_validity_token().unwrap(), cycle_token);
    assert_eq!(
        reader.fingerprint_validity_token().unwrap(),
        fingerprint_token
    );
    assert_cycle_consumers(&reader, false);
    assert_eq!(similar_names(&reader), ["delta"]);
}

#[test]
fn cache_invalidation_read_snapshot_does_not_reuse_or_publish_old_view() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.db");
    let reader = Database::open(&path).unwrap();
    seed_similar(root.path(), &reader);
    let writer = Database::open(&path).unwrap();
    assert_eq!(similar_names(&reader), ["bravo"]);
    let snapshot = reader.read_snapshot().unwrap();
    index_source(
        root.path(),
        &writer,
        "z.rs",
        Language::Rust,
        &CLONE.replace("alpha", "delta"),
    );
    assert_eq!(similar_names(&reader), ["bravo"]);
    drop(snapshot);
    assert_eq!(similar_names(&reader), ["delta"]);
}
