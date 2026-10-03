use std::path::Path;

use codesage_features::map_features;
use codesage_graph::{assess_risk, full_index, incremental_index};
use codesage_protocol::{Language, TrustBoundary};
use codesage_storage::Database;

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

#[test]
fn java_roles_compose_with_imports_and_feed_features_and_risk() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "pom.xml",
        "<project><modelVersion>4.0.0</modelVersion></project>",
    );
    let path = "src/main/java/example/Orders.java";
    write(
        root.path(),
        path,
        "import org.springframework.web.bind.annotation.RestController;\nimport java.net.http.HttpClient;\n@RestController class Orders {}\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let expected = vec![
        TrustBoundary::Network,
        TrustBoundary::UserInput,
        TrustBoundary::ExternalApi,
        TrustBoundary::Serialization,
    ];
    assert_eq!(db.trust_boundaries_for_file_path(path).unwrap(), expected);
    let risk = assess_risk(&db, path).unwrap();
    assert_eq!(risk.trust_boundaries, expected);
    assert!(
        risk.notes
            .iter()
            .any(|note| note.contains("crosses 4 trust boundaries"))
    );
    let id = db.file_id_for_path(path).unwrap().unwrap();
    db.replace_file_trust_boundaries(id, &[]).unwrap();
    let baseline = assess_risk(&db, path).unwrap();
    assert!((risk.score - baseline.score - 0.08).abs() < 1e-10);
    db.replace_file_trust_boundaries(id, &expected).unwrap();
    map_features(root.path(), &db, &[]).unwrap();
    let features = db
        .list_features(None, Some(Language::Java), None, 100)
        .unwrap();
    assert_eq!(features.len(), 1);
    assert!(features[0].tags.iter().any(|tag| tag == "web-entrypoint"));
    assert_eq!(features[0].trust_boundaries, expected);

    write(
        root.path(),
        path,
        "import java.nio.file.Files;\nclass Orders {}\n",
    );
    map_features(root.path(), &db, &[]).unwrap();
    assert_eq!(
        db.trust_boundaries_for_file_path(path).unwrap(),
        vec![TrustBoundary::Filesystem]
    );
    assert!(
        db.list_features(None, Some(Language::Java), None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unavailable_java_source_retains_boundaries_and_features_with_partial_outcome() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "pom.xml", "<project/>");
    let path = "src/main/java/example/Orders.java";
    write(
        root.path(),
        path,
        "@org.springframework.web.bind.annotation.RestController class Orders {}\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    map_features(root.path(), &db, &[]).unwrap();
    let before = db.trust_boundaries_for_file_path(path).unwrap();
    std::fs::write(root.path().join(path), [0xff]).unwrap();
    let outcome = codesage_features::map_features_detailed(root.path(), &db, &[]).unwrap();
    assert!(
        outcome
            .mapper_errors
            .iter()
            .any(|error| error.starts_with("java:"))
    );
    assert_eq!(db.trust_boundaries_for_file_path(path).unwrap(), before);
    assert_eq!(db.feature_count().unwrap(), 1);
}

#[test]
fn source_aware_backfill_uses_current_java_refs_and_preserves_pending_rows_on_error() {
    let root = tempfile::tempdir().unwrap();
    let first = "First.java";
    let second = "Second.java";
    write(
        root.path(),
        first,
        "@org.springframework.web.bind.annotation.RestController class First {}\n",
    );
    write(
        root.path(),
        second,
        "@org.springframework.stereotype.Repository class Second {}\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let before = db.trust_boundaries_for_file_path(first).unwrap();
    db.execute_raw_for_tests("UPDATE files SET boundaries_derived_at = 0")
        .unwrap();
    write(
        root.path(),
        first,
        "import java.nio.file.Files; class First {}\n",
    );
    std::fs::write(root.path().join(second), [0xff]).unwrap();
    let pending = db.files_pending_boundary_derivation().unwrap();
    assert!(
        codesage_features::trust_boundary::derive_for_files_with_source(root.path(), &db, &pending)
            .is_err()
    );
    assert_eq!(db.trust_boundaries_for_file_path(first).unwrap(), before);
    assert_eq!(db.files_pending_boundary_derivation().unwrap().len(), 2);
    write(
        root.path(),
        second,
        "@org.springframework.stereotype.Repository class Second {}\n",
    );
    assert_eq!(
        codesage_features::trust_boundary::derive_for_files_with_source(root.path(), &db, &pending)
            .unwrap(),
        2
    );
    assert_eq!(
        db.trust_boundaries_for_file_path(first).unwrap(),
        vec![TrustBoundary::Filesystem]
    );
    assert_eq!(
        db.trust_boundaries_for_file_path(second).unwrap(),
        vec![TrustBoundary::Database, TrustBoundary::Serialization]
    );
    assert!(db.files_pending_boundary_derivation().unwrap().is_empty());
}

#[test]
fn unchanged_legacy_java_is_rederived_once_and_exclusions_remove_roles() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "pom.xml", "<project/>");
    let path = "src/main/java/example/Orders.java";
    write(
        root.path(),
        path,
        "import org.springframework.stereotype.Repository;\n@Repository class Orders {}\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let id = db.file_id_for_path(path).unwrap().unwrap();
    let before_hash = db.all_file_hashes().unwrap()[path].clone();
    for version in 1..=7 {
        db.replace_file_trust_boundaries(id, &[]).unwrap();
        db.record_file_interpretation(
            id,
            &format!(
                "codesage/structural/v1;parser-queries=4;extraction=7;trust-boundaries={version}"
            ),
        )
        .unwrap();
        let stats = incremental_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(stats.files_indexed, 1, "legacy version {version}");
        assert_eq!(db.all_file_hashes().unwrap()[path], before_hash);
        assert_eq!(
            db.trust_boundaries_for_file_path(path).unwrap(),
            vec![TrustBoundary::Database, TrustBoundary::Serialization]
        );
        assert_eq!(
            incremental_index(root.path(), &db, &[], false)
                .unwrap()
                .files_indexed,
            0
        );
    }
    let excludes = vec!["src/main/java/example".to_string()];
    incremental_index(root.path(), &db, &excludes, false).unwrap();
    map_features(root.path(), &db, &excludes).unwrap();
    assert!(db.trust_boundaries_for_file_path(path).unwrap().is_empty());
    assert!(
        db.list_features(None, Some(Language::Java), None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn named_main_declaration_changes_refresh_unchanged_test_source_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let user = "src/test/java/example/Plain.java";
    let sibling = "src/main/java/example/RestController.java";
    write(
        root.path(),
        user,
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Plain {}",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let expected = vec![
        TrustBoundary::Network,
        TrustBoundary::UserInput,
        TrustBoundary::Serialization,
    ];
    assert_eq!(db.trust_boundaries_for_file_path(user).unwrap(), expected);
    let hash = db.all_file_hashes().unwrap()[user].clone();
    let source = "package example; public @interface RestController {}";
    write(root.path(), sibling, source);
    let info = codesage_protocol::FileInfo {
        path: sibling.to_string(),
        language: Language::Java,
        content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
        is_test: false,
    };
    codesage_graph::index_files(root.path(), &db, &[info], false).unwrap();
    assert!(db.trust_boundaries_for_file_path(user).unwrap().is_empty());
    assert_eq!(db.all_file_hashes().unwrap()[user], hash);
    std::fs::remove_file(root.path().join(sibling)).unwrap();
    codesage_graph::remove_files_with_source(root.path(), &db, &[sibling.to_string()]).unwrap();
    assert_eq!(db.trust_boundaries_for_file_path(user).unwrap(), expected);
    assert_eq!(db.all_file_hashes().unwrap()[user], hash);
}

#[test]
fn named_java_index_refreshes_unchanged_sibling_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let user = "src/main/java/example/Plain.java";
    let sibling = "src/main/java/example/RestController.java";
    write(
        root.path(),
        user,
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Plain {}",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let hash = db.all_file_hashes().unwrap()[user].clone();
    let source = "package example; public @interface RestController {}";
    write(root.path(), sibling, source);
    let info = codesage_protocol::FileInfo {
        path: sibling.to_string(),
        language: Language::Java,
        content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
        is_test: false,
    };
    codesage_graph::index_files(root.path(), &db, &[info], false).unwrap();
    assert!(db.trust_boundaries_for_file_path(user).unwrap().is_empty());
    assert_eq!(db.all_file_hashes().unwrap()[user], hash);
    let source = "package other; public @interface RestController {}";
    write(root.path(), sibling, source);
    let info = codesage_protocol::FileInfo {
        path: sibling.to_string(),
        language: Language::Java,
        content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
        is_test: false,
    };
    codesage_graph::index_files(root.path(), &db, &[info], false).unwrap();
    assert_eq!(
        db.trust_boundaries_for_file_path(user).unwrap(),
        vec![
            TrustBoundary::Network,
            TrustBoundary::UserInput,
            TrustBoundary::Serialization
        ]
    );
    assert_eq!(db.all_file_hashes().unwrap()[user], hash);
}

#[test]
fn named_java_removal_refreshes_unchanged_sibling_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let user = "src/main/java/example/Plain.java";
    let sibling = "src/main/java/example/RestController.java";
    write(
        root.path(),
        user,
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Plain {}",
    );
    write(
        root.path(),
        sibling,
        "package example; public @interface RestController {}",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    assert!(db.trust_boundaries_for_file_path(user).unwrap().is_empty());
    let hash = db.all_file_hashes().unwrap()[user].clone();
    std::fs::remove_file(root.path().join(sibling)).unwrap();
    codesage_graph::remove_files_with_source(root.path(), &db, &[sibling.to_string()]).unwrap();
    assert_eq!(
        db.trust_boundaries_for_file_path(user).unwrap(),
        vec![
            TrustBoundary::Network,
            TrustBoundary::UserInput,
            TrustBoundary::Serialization
        ]
    );
    assert_eq!(db.all_file_hashes().unwrap()[user], hash);
}

#[test]
fn named_java_refresh_failures_retain_boundaries_and_allow_retry() {
    let root = tempfile::tempdir().unwrap();
    let user = "src/main/java/example/Plain.java";
    let sibling = "src/main/java/example/RestController.java";
    let broken = "src/main/java/other/Broken.java";
    write(root.path(), "pom.xml", "<project/>");
    write(
        root.path(),
        user,
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Plain {}",
    );
    write(root.path(), broken, "package other; class Broken {}");
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    map_features(root.path(), &db, &[]).unwrap();
    let before = db.trust_boundaries_for_file_path(user).unwrap();
    let source = "package example; public @interface RestController {}";
    write(root.path(), sibling, source);
    std::fs::write(root.path().join(broken), [0xff]).unwrap();
    let info = codesage_protocol::FileInfo {
        path: sibling.to_string(),
        language: Language::Java,
        content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
        is_test: false,
    };
    assert!(
        codesage_graph::index_files(root.path(), &db, std::slice::from_ref(&info), false).is_err()
    );
    assert_eq!(db.trust_boundaries_for_file_path(user).unwrap(), before);
    assert_eq!(db.feature_count().unwrap(), 1);
    assert!(
        !db.file_interpretation_matches(sibling, codesage_graph::STRUCTURAL_INTERPRETATION)
            .unwrap()
    );
    write(root.path(), broken, "package other; class Broken {}");
    codesage_graph::index_files(root.path(), &db, &[info], false).unwrap();
    assert!(db.trust_boundaries_for_file_path(user).unwrap().is_empty());
    std::fs::remove_file(root.path().join(sibling)).unwrap();
    std::fs::write(root.path().join(broken), [0xff]).unwrap();
    assert!(
        codesage_graph::remove_files_with_source(root.path(), &db, &[sibling.to_string()]).is_err()
    );
    assert!(db.trust_boundaries_for_file_path(user).unwrap().is_empty());
    assert_eq!(db.feature_count().unwrap(), 1);
    write(root.path(), broken, "package other; class Broken {}");
    codesage_graph::remove_files_with_source(root.path(), &db, &[sibling.to_string()]).unwrap();
    assert_eq!(db.trust_boundaries_for_file_path(user).unwrap(), before);
}

#[test]
fn failed_java_context_preserves_existing_rows_in_full_and_incremental_passes() {
    for full in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let healthy = "src/main/java/example/Healthy.java";
        let broken = "src/main/java/example/Broken.java";
        write(root.path(), "pom.xml", "<project/>");
        write(
            root.path(),
            healthy,
            "package example; import org.springframework.web.bind.annotation.*; @RestController class Healthy {}",
        );
        write(
            root.path(),
            broken,
            "package example; class Broken {} /* valid */",
        );
        write(root.path(), "independent.py", "def old(): pass");
        let db = Database::open_in_memory().unwrap();
        full_index(root.path(), &db, &[], false).unwrap();
        map_features(root.path(), &db, &[]).unwrap();
        let before = db.all_file_hashes().unwrap();
        let tags = db.trust_boundaries_for_file_path(healthy).unwrap();
        let risk = assess_risk(&db, healthy).unwrap();
        std::fs::write(
            root.path().join(broken),
            b"package example; class Broken {} /*\xff*/",
        )
        .unwrap();
        write(root.path(), "independent.py", "def updated(): pass");
        for _ in 0..2 {
            let stats = if full {
                full_index(root.path(), &db, &[], false)
            } else {
                incremental_index(root.path(), &db, &[], false)
            }
            .unwrap();
            assert_eq!(stats.files_failed, 2);
            assert!(stats.failed_paths.contains(&healthy.to_string()));
            assert!(stats.failed_paths.contains(&broken.to_string()));
            assert_eq!(db.trust_boundaries_for_file_path(healthy).unwrap(), tags);
            assert_eq!(assess_risk(&db, healthy).unwrap().score, risk.score);
            assert_eq!(db.all_file_hashes().unwrap()[healthy], before[healthy]);
            assert_eq!(db.all_file_hashes().unwrap()[broken], before[broken]);
            assert!(
                !db.file_interpretation_matches(healthy, codesage_graph::STRUCTURAL_INTERPRETATION)
                    .unwrap()
            );
            assert_eq!(db.feature_count().unwrap(), 1);
        }
        assert_ne!(
            db.all_file_hashes().unwrap()["independent.py"],
            before["independent.py"]
        );
        write(
            root.path(),
            broken,
            "package example; class Broken {} /* repaired */",
        );
        let stats = incremental_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(stats.files_failed, 0);
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(db.trust_boundaries_for_file_path(healthy).unwrap(), tags);
        assert_eq!(
            incremental_index(root.path(), &db, &[], false)
                .unwrap()
                .files_indexed,
            0
        );
    }
}

#[test]
fn named_existing_java_file_is_not_overwritten_without_declaration_context() {
    let root = tempfile::tempdir().unwrap();
    let healthy = "src/main/java/example/Healthy.java";
    let broken = "src/main/java/example/Broken.java";
    let source = "package example; import org.springframework.web.bind.annotation.*; @RestController class Healthy {}";
    write(root.path(), healthy, source);
    write(root.path(), broken, "package example; class Broken {}");
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let before = db.trust_boundaries_for_file_path(healthy).unwrap();
    let info = codesage_protocol::FileInfo {
        path: healthy.to_string(),
        language: Language::Java,
        content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
        is_test: false,
    };
    std::fs::write(
        root.path().join(broken),
        b"package example; class Broken {} /*\xff*/",
    )
    .unwrap();
    for _ in 0..2 {
        assert!(
            codesage_graph::index_files(root.path(), &db, std::slice::from_ref(&info), false)
                .is_err()
        );
        assert_eq!(db.trust_boundaries_for_file_path(healthy).unwrap(), before);
        assert!(
            !db.file_interpretation_matches(healthy, codesage_graph::STRUCTURAL_INTERPRETATION)
                .unwrap()
        );
    }
    write(
        root.path(),
        broken,
        "package example; class Broken {} /* repaired */",
    );
    codesage_graph::index_files(root.path(), &db, &[info], false).unwrap();
    assert_eq!(db.trust_boundaries_for_file_path(healthy).unwrap(), before);
    assert!(
        db.file_interpretation_matches(healthy, codesage_graph::STRUCTURAL_INTERPRETATION)
            .unwrap()
    );
}
