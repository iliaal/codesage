use codesage_graph::{full_index, impact_analysis};
use codesage_protocol::{ExportRequest, FileCategory, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn setup_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
    ).unwrap();

    std::fs::write(
        root.join("Controller.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller {\n  public function show(Repository $repo, $id) { return $repo->find($id); }\n}\n",
    ).unwrap();

    std::fs::write(
        root.join("Service.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Service {\n  public function run(Repository $repo) { return $repo->find(1); }\n}\n",
    ).unwrap();

    std::fs::write(
        root.join("RepositoryTest.php"),
        b"<?php\nnamespace Tests;\nuse App\\Repository;\nclass RepositoryTest {\n  public function testFind() { $r = new Repository(); $r->find(1); }\n}\n",
    ).unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[]).unwrap();
    (dir, db)
}

#[test]
fn impact_by_symbol_finds_direct_callers() {
    let (_dir, db) = setup_project();

    let req = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "Repository".to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let entries = impact_analysis(&db, &req).unwrap();
    assert!(entries.len() >= 2, "expected at least 2 affected files, got {}", entries.len());

    let paths: Vec<String> = entries.iter().map(|e| e.file_path.clone()).collect();
    assert!(paths.iter().any(|p| p.ends_with("Controller.php")));
    assert!(paths.iter().any(|p| p.ends_with("Service.php")));

    for e in &entries {
        assert_eq!(e.distance, 1, "{} should be distance 1", e.file_path);
        assert!(!e.reasons.is_empty());
    }
}

#[test]
fn impact_source_only_filters_tests() {
    let (_dir, db) = setup_project();

    let req_all = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "Repository".to_string(),
        },
        depth: 1,
        source_only: false,
    };
    let all = impact_analysis(&db, &req_all).unwrap();
    let has_test = all.iter().any(|e| e.category == FileCategory::Test);
    assert!(has_test, "unfiltered run should include RepositoryTest.php");

    let req_src = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "Repository".to_string(),
        },
        depth: 1,
        source_only: true,
    };
    let src = impact_analysis(&db, &req_src).unwrap();
    assert!(src.iter().all(|e| e.category == FileCategory::Source));
    assert!(src.len() < all.len());
}

#[test]
fn impact_by_file_excludes_origin() {
    let (_dir, db) = setup_project();

    let req = ImpactRequest {
        target: ImpactTarget::File {
            path: "Repository.php".to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let entries = impact_analysis(&db, &req).unwrap();
    assert!(!entries.is_empty());
    for e in &entries {
        assert!(!e.file_path.ends_with("Repository.php"));
    }
}

#[test]
fn impact_unknown_symbol_returns_empty() {
    let (_dir, db) = setup_project();

    let req = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "NonExistentSymbol".to_string(),
        },
        depth: 2,
        source_only: false,
    };

    let entries = impact_analysis(&db, &req).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn export_context_for_symbol_returns_definition() {
    let (_dir, db) = setup_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("Repository".to_string()),
        limit: 5,
        include_callers: false,
        include_callees: false,
    };

    let bundle = codesage_graph::query::export_context_for_symbol(&db, "Repository", &req)
        .unwrap();

    assert!(bundle.target_description.contains("Repository"));
    assert!(
        !bundle.symbol_definitions.is_empty(),
        "should have found Repository definition"
    );
    assert_eq!(bundle.symbol_definitions[0].name, "Repository");
    assert!(bundle.related.is_empty(), "callers not requested");
}

#[test]
fn export_context_for_symbol_with_callers() {
    let (_dir, db) = setup_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("Repository".to_string()),
        limit: 10,
        include_callers: true,
        include_callees: false,
    };

    let bundle = codesage_graph::query::export_context_for_symbol(&db, "Repository", &req)
        .unwrap();

    assert!(
        !bundle.symbol_definitions.is_empty(),
        "should have found the definition"
    );
}

#[test]
fn export_context_unknown_symbol_returns_empty_bundle() {
    let (_dir, db) = setup_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("NoSuchSymbol".to_string()),
        limit: 5,
        include_callers: true,
        include_callees: false,
    };

    let bundle = codesage_graph::query::export_context_for_symbol(&db, "NoSuchSymbol", &req)
        .unwrap();
    assert!(bundle.primary.is_empty());
    assert!(bundle.symbol_definitions.is_empty());
    assert!(bundle.target_description.contains("not found"));
}
