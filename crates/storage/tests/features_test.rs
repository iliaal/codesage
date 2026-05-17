use codesage_protocol::{FeatureConfidence, FeatureKind, FeatureRecord, Language};
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
fn list_features_tag_filter_matches_substring_within_compound_tags() {
    // Regression for the validated finding "list_features tag LIKE
    // pattern mismatch": doc says "tag substring" but the SQL used to
    // bind `%"{tag}"%` (literal quote anchors), so a filter of
    // `tag="framework"` would miss tags like `framework:react-router`.
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

    // Substring inside a compound tag — historically missed.
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

    // Full-tag exact match still works.
    let exact = db
        .list_features(None, None, Some("framework:react-router"), 0)
        .unwrap();
    let exact_ids: Vec<&str> = exact.iter().map(|f| f.feature_id.as_str()).collect();
    assert_eq!(exact_ids, vec!["feat_a"]);

    // Non-substring filter returns nothing.
    let none = db
        .list_features(None, None, Some("nonexistent"), 0)
        .unwrap();
    assert!(none.is_empty());
}
