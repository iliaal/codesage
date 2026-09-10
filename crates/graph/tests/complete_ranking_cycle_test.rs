use codesage_graph::{
    assess_risk, build_project_overview, index_files, session_start, top_risk_ranking,
};
use codesage_parser::discover::content_hash;
use codesage_protocol::{FileInfo, Language};
use codesage_storage::Database;

#[test]
fn complete_ranking_observes_same_shape_reindex_after_ordinary_cycle_cache_is_warm() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(&root.path().join("index.db")).unwrap();
    let mut files = Vec::new();
    for (path, source) in [
        ("a.php", "<?php\nnamespace App;\nuse App\\B;\nclass A {}\n"),
        ("c.php", "<?php\nnamespace App;\nclass C {}\n"),
        ("z.php", "<?php\nnamespace App;\nuse App\\A;\nclass B {}\n"),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
        files.push(FileInfo {
            path: path.into(),
            language: Language::Php,
            content_hash: content_hash(source.as_bytes()),
        });
    }
    index_files(root.path(), &db, &files, false).unwrap();
    let before = assess_risk(&db, "a.php").unwrap();
    assert!(before.in_cycle);
    assert_eq!(before.cycle_size, 2);
    let token = db.import_cycle_validity_token().unwrap();
    let old_edges = db.enumerate_file_import_edges().unwrap();

    let replacement = "<?php\nnamespace App;\nuse App\\C;\nclass B {}\n";
    std::fs::write(root.path().join("z.php"), replacement).unwrap();
    files[2].content_hash = content_hash(replacement.as_bytes());
    index_files(root.path(), &db, &files[2..], false).unwrap();
    assert_eq!(db.import_cycle_validity_token().unwrap(), token);
    let mut new_edges = db.enumerate_file_import_edges().unwrap();
    new_edges.sort();
    assert_ne!(new_edges, old_edges);
    assert_eq!(
        new_edges,
        vec![
            ("a.php".into(), "z.php".into()),
            ("z.php".into(), "c.php".into()),
        ]
    );

    let fresh = Database::open_in_memory().unwrap();
    index_files(root.path(), &fresh, &files, false).unwrap();
    let fresh_risk = assess_risk(&fresh, "a.php").unwrap();
    assert!(!fresh_risk.in_cycle);
    let expected = top_risk_ranking(&fresh).unwrap();
    let actual = top_risk_ranking(&db).unwrap();
    let overview = build_project_overview(root.path(), &db).unwrap();
    let snapshot = session_start(root.path(), &db, "after-reindex").unwrap();
    assert!(snapshot.cycles.is_empty());
    let expected = serde_json::to_value(expected.rows()).unwrap();
    assert_eq!(
        [
            serde_json::to_value(actual.rows()).unwrap(),
            serde_json::to_value(overview.top_risk_files).unwrap(),
            serde_json::to_value(snapshot.top_risk_files).unwrap(),
        ],
        [expected.clone(), expected.clone(), expected],
        "ranking, overview, and session must not reuse SCCs from before the reindex"
    );
}
