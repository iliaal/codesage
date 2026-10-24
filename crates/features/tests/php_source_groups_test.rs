use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use codesage_features::map_features;
use codesage_protocol::{FeatureFileRole, FeatureRecord};
use codesage_storage::Database;

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn groups(db: &Database) -> Vec<FeatureRecord> {
    db.list_features(None, None, Some("source-group"), 0)
        .unwrap()
}

#[test]
fn php_groups_cover_all_sources_once_and_keep_apis_after_edit_and_remap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "ext/demo/config.m4",
        "PHP_NEW_EXTENSION(demo, demo.c)\n",
    );
    let mut expected = BTreeSet::new();
    for i in 0..95 {
        let path = format!("ext/demo/flat/part_{i:03}.c");
        write(root, &path, &format!("PHP_FUNCTION(part_{i:03}) {{}}\n"));
        expected.insert(path);
    }
    for i in 0..45 {
        let path = format!("ext/demo/nested/deeper/type_{i:03}.h");
        write(root, &path, "struct Member { int value; };\n");
        expected.insert(path);
    }
    let api_path = "ext/demo/many.c";
    let many = (0..100)
        .map(|i| format!("PHP_FUNCTION(many_{i:03}) {{}}\n"))
        .collect::<String>();
    write(root, api_path, &many);
    expected.insert(api_path.into());
    write(root, "ext/demo/excluded.c", "PHP_FUNCTION(excluded) {}\n");
    write(
        root,
        "ext/demo/tests/helper.c",
        "PHP_FUNCTION(test_only) {}\n",
    );
    write(root, "ext/demo/demo_arginfo.h", "#define GENERATED 1\n");
    let db = Database::open_in_memory().unwrap();
    let excludes = vec!["ext/demo/excluded.c".to_string()];
    map_features(root, &db, &excludes).unwrap();
    let first = groups(&db);
    assert_eq!(first.len(), 6);
    assert!(first.iter().all(|group| {
        group
            .files
            .iter()
            .filter(|file| file.role == FeatureFileRole::Owned)
            .count()
            <= 40
    }));
    let mut ownership = BTreeMap::new();
    for group in &first {
        assert!(
            !group.files.iter().any(|file| file.path == group.entry_path),
            "a directory anchor must not expand ownership beyond its bounded group"
        );
        for file in group
            .files
            .iter()
            .filter(|file| file.role == FeatureFileRole::Owned)
        {
            assert!(
                ownership
                    .insert(file.path.clone(), group.feature_id.clone())
                    .is_none(),
                "duplicate owner for {}",
                file.path
            );
        }
    }
    assert_eq!(ownership.keys().cloned().collect::<BTreeSet<_>>(), expected);
    let anchors = first
        .iter()
        .flat_map(|group| group.entrypoints.iter())
        .collect::<Vec<_>>();
    assert_eq!(anchors.len(), 195);
    assert!(anchors.iter().all(|entry| expected.contains(&entry.path)));
    assert!(
        !anchors
            .iter()
            .any(|entry| matches!(entry.symbol.as_str(), "excluded" | "test_only"))
    );
    let owner = db.features_for_file(api_path).unwrap();
    assert_eq!(owner.len(), 1);
    assert_eq!(owner[0].entrypoints.len(), 100);
    let original_id = owner[0].feature_id.clone();
    map_features(root, &db, &excludes).unwrap();
    assert_eq!(first, groups(&db));
    write(root, api_path, "PHP_FUNCTION(replacement) {}\n");
    map_features(root, &db, &excludes).unwrap();
    let replaced = db.features_for_file(api_path).unwrap();
    assert_eq!(replaced[0].feature_id, original_id);
    assert_eq!(replaced[0].entrypoints.len(), 1);
    assert_eq!(replaced[0].entrypoints[0].symbol, "replacement");
    assert_eq!(
        groups(&db)
            .iter()
            .flat_map(|group| &group.entrypoints)
            .count(),
        96
    );
}
