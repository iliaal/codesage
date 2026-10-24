use codesage_protocol::{
    FeatureConfidence, FeatureEntrypoint, FeatureFileRef, FeatureFileRole, FeatureKind,
    FeatureRecord, Language, TrustBoundary,
};
use codesage_storage::Database;

fn make_feature(id: &str, tags: &[&str]) -> FeatureRecord {
    FeatureRecord {
        feature_id: id.to_string(),
        title: format!("Feature {id}"),
        summary: String::new(),
        kind: FeatureKind::Library,
        source: "test".to_string(),
        confidence: FeatureConfidence::High,
        entry_path: format!("src/{id}.rs"),
        entry_symbol: None,
        entrypoints: Vec::new(),
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Rust,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        files: Vec::new(),
        trust_boundaries: Vec::new(),
    }
}

#[test]
fn readonly_legacy_features_use_empty_entrypoints_without_migrating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let mut feature = make_feature("feat_legacy", &["library"]);
    feature.files.push(FeatureFileRef {
        path: feature.entry_path.clone(),
        role: FeatureFileRole::Entry,
        reason: None,
    });
    {
        let db = Database::open(&path).unwrap();
        db.upsert_feature(&feature).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE features DROP COLUMN entrypoints;
            DELETE FROM schema_migrations WHERE name = '0029_feature_entrypoints';",
        )
        .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    {
        let db = Database::open_read_only(&path).unwrap();
        assert_eq!(
            db.load_feature(&feature.feature_id).unwrap(),
            Some(feature.clone())
        );
        assert_eq!(
            db.list_features(None, None, None, 0).unwrap(),
            vec![feature.clone()]
        );
        assert_eq!(
            db.features_for_file(&feature.entry_path).unwrap(),
            vec![feature]
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn grouped_entrypoints_survive_every_read_and_are_replaced_on_remap() {
    let db = Database::open_in_memory().unwrap();
    let mut feature = make_feature("feat_group", &["source-group"]);
    feature.entry_path = "ext/demo".into();
    feature.files = vec![FeatureFileRef {
        path: "ext/demo/api.c".into(),
        role: FeatureFileRole::Owned,
        reason: None,
    }];
    feature.entrypoints = vec![
        FeatureEntrypoint {
            path: "ext/demo/api.c".into(),
            symbol: "first".into(),
        },
        FeatureEntrypoint {
            path: "ext/demo/api.c".into(),
            symbol: "Second::run".into(),
        },
    ];
    db.upsert_feature(&feature).unwrap();
    assert_eq!(
        db.load_feature(&feature.feature_id)
            .unwrap()
            .unwrap()
            .entrypoints,
        feature.entrypoints
    );
    assert_eq!(
        db.list_features(None, None, None, 0).unwrap()[0].entrypoints,
        feature.entrypoints
    );
    assert_eq!(
        db.features_for_file("ext/demo/api.c").unwrap()[0].entrypoints,
        feature.entrypoints
    );
    feature.entrypoints.remove(0);
    db.upsert_feature(&feature).unwrap();
    assert_eq!(
        db.load_feature(&feature.feature_id)
            .unwrap()
            .unwrap()
            .entrypoints,
        feature.entrypoints
    );
}

#[test]
fn list_features_tag_filter_matches_substring_within_compound_tags() {
    let db = Database::open_in_memory().expect("open in-memory db");

    db.upsert_feature(&make_feature(
        "feat_a",
        &["framework:react-router", "route"],
    ))
    .unwrap();
    db.upsert_feature(&make_feature("feat_b", &["framework:laravel", "route"]))
        .unwrap();
    db.upsert_feature(&make_feature("feat_c", &["library", "rust"]))
        .unwrap();

    let framework_hits = db.list_features(None, None, Some("framework"), 0).unwrap();
    let ids: Vec<&str> = framework_hits
        .iter()
        .map(|f| f.feature_id.as_str())
        .collect();
    assert!(
        ids.contains(&"feat_a"),
        "substring 'framework' should match 'framework:react-router', got {ids:?}"
    );
    assert!(
        ids.contains(&"feat_b"),
        "substring 'framework' should match 'framework:laravel', got {ids:?}"
    );
    assert!(
        !ids.contains(&"feat_c"),
        "substring 'framework' must not match unrelated tags, got {ids:?}"
    );

    let exact = db
        .list_features(None, None, Some("framework:react-router"), 0)
        .unwrap();
    let exact_ids: Vec<&str> = exact.iter().map(|f| f.feature_id.as_str()).collect();
    assert_eq!(exact_ids, vec!["feat_a"]);

    let none = db
        .list_features(None, None, Some("nonexistent"), 0)
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn feature_exists_reports_presence_without_hydration() {
    let db = Database::open_in_memory().expect("open in-memory db");
    assert!(!db.feature_exists("feat_a").unwrap());

    db.upsert_feature(&make_feature("feat_a", &["route"]))
        .unwrap();
    assert!(db.feature_exists("feat_a").unwrap());
    assert!(!db.feature_exists("feat_missing").unwrap());

    assert!(db.load_feature("feat_a").unwrap().is_some());
    assert!(db.load_feature("feat_missing").unwrap().is_none());
}

#[test]
fn list_features_hydrates_files_and_boundaries_for_each_feature() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let mut first = make_feature("feat_a", &["framework:react-router"]);
    first.files = vec![
        FeatureFileRef {
            path: "src/a.rs".to_string(),
            role: FeatureFileRole::Owned,
            reason: Some("owned".to_string()),
        },
        FeatureFileRef {
            path: "tests/a.rs".to_string(),
            role: FeatureFileRole::Test,
            reason: None,
        },
    ];
    first.trust_boundaries = vec![TrustBoundary::Filesystem, TrustBoundary::Secrets];
    let mut second = make_feature("feat_b", &["library"]);
    second.files = vec![FeatureFileRef {
        path: "src/b.rs".to_string(),
        role: FeatureFileRole::Entry,
        reason: Some("entry".to_string()),
    }];
    second.trust_boundaries = vec![TrustBoundary::Network];

    db.upsert_feature(&first).unwrap();
    db.upsert_feature(&second).unwrap();

    let features = db.list_features(None, None, None, 0).unwrap();
    let feat_a = features
        .iter()
        .find(|f| f.feature_id == "feat_a")
        .expect("feat_a listed");
    let feat_b = features
        .iter()
        .find(|f| f.feature_id == "feat_b")
        .expect("feat_b listed");

    assert_eq!(feat_a.files.len(), 2);
    assert_eq!(feat_a.trust_boundaries.len(), 2);
    assert!(feat_a.trust_boundaries.contains(&TrustBoundary::Filesystem));
    assert_eq!(feat_b.files[0].path, "src/b.rs");
    assert_eq!(feat_b.trust_boundaries, vec![TrustBoundary::Network]);
}
