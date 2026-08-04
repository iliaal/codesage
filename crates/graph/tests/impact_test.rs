use codesage_graph::{full_index, impact_analysis};
use codesage_protocol::{
    DEFAULT_EMBEDDING_DIM, ExportRequest, FileCategory, ImpactRequest, ImpactTarget,
};
use codesage_storage::Database;

fn insert_chunk(db: &Database, file_path: &str, language: &str, content: &str, end_line: u32) {
    let embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];
    db.insert_chunks(
        file_path,
        language,
        &[(content, 1, end_line, embedding.as_slice())],
    )
    .unwrap();
}

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
    full_index(root, &db, &[], false).unwrap();
    insert_chunk(
        &db,
        "Repository.php",
        "php",
        "<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
        4,
    );
    insert_chunk(
        &db,
        "Controller.php",
        "php",
        "<?php\nnamespace App;\nuse App\\Repository;\nclass Controller {\n  public function show(Repository $repo, $id) { return $repo->find($id); }\n}\n",
        4,
    );
    insert_chunk(
        &db,
        "Service.php",
        "php",
        "<?php\nnamespace App;\nuse App\\Repository;\nclass Service {\n  public function run(Repository $repo) { return $repo->find(1); }\n}\n",
        4,
    );
    (dir, db)
}

fn setup_ambiguous_python_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir(root.join("app")).unwrap();
    std::fs::write(
        root.join("app/models.py"),
        b"class Repository:\n    def find(self, id):\n        return id\n\nclass Cache:\n    def find(self, key):\n        return key\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/repo_controller.py"),
        b"from app.models import Repository\n\ndef handle_repo():\n    repo = Repository()\n    return repo.find(1)\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/cache_controller.py"),
        b"from app.models import Cache\n\ndef handle_cache():\n    cache = Cache()\n    return cache.find(\"x\")\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    insert_chunk(
        &db,
        "app/models.py",
        "python",
        "class Repository:\n    def find(self, id):\n        return id\n\nclass Cache:\n    def find(self, key):\n        return key\n",
        7,
    );
    insert_chunk(
        &db,
        "app/repo_controller.py",
        "python",
        "from app.models import Repository\n\ndef handle_repo():\n    repo = Repository()\n    return repo.find(1)\n",
        5,
    );
    insert_chunk(
        &db,
        "app/cache_controller.py",
        "python",
        "from app.models import Cache\n\ndef handle_cache():\n    cache = Cache()\n    return cache.find(\"x\")\n",
        5,
    );
    (dir, db)
}

fn setup_qualified_rust_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("db.rs"),
        b"pub struct Database;\nimpl Database {\n    pub fn open() -> Self { Database }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("conn.rs"),
        b"pub struct Connection;\nimpl Connection {\n    pub fn open() -> Self { Connection }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("db_user.rs"),
        b"use crate::db::Database;\npub fn make_db() { let _ = Database::open(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("conn_user.rs"),
        b"use crate::conn::Connection;\npub fn make_conn() { let _ = Connection::open(); }\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    insert_chunk(
        &db,
        "db.rs",
        "rust",
        "pub struct Database;\nimpl Database {\n    pub fn open() -> Self { Database }\n}\n",
        4,
    );
    insert_chunk(
        &db,
        "conn.rs",
        "rust",
        "pub struct Connection;\nimpl Connection {\n    pub fn open() -> Self { Connection }\n}\n",
        4,
    );
    insert_chunk(
        &db,
        "db_user.rs",
        "rust",
        "use crate::db::Database;\npub fn make_db() { let _ = Database::open(); }\n",
        2,
    );
    insert_chunk(
        &db,
        "conn_user.rs",
        "rust",
        "use crate::conn::Connection;\npub fn make_conn() { let _ = Connection::open(); }\n",
        2,
    );
    (dir, db)
}

fn setup_callee_rust_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("helpers.rs"), b"pub fn helper() {}\n").unwrap();
    std::fs::write(
        root.join("service.rs"),
        b"use crate::helpers::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    insert_chunk(&db, "helpers.rs", "rust", "pub fn helper() {}\n", 1);
    insert_chunk(
        &db,
        "service.rs",
        "rust",
        "use crate::helpers::helper;\npub fn run() { helper(); }\n",
        2,
    );
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
    assert!(
        entries.len() >= 2,
        "expected at least 2 affected files, got {}",
        entries.len()
    );

    let paths: Vec<String> = entries.iter().map(|e| e.file_path.clone()).collect();
    assert!(paths.iter().any(|p| p.ends_with("Controller.php")));
    assert!(paths.iter().any(|p| p.ends_with("Service.php")));

    for e in &entries {
        assert_eq!(e.distance, 1, "{} should be distance 1", e.file_path);
        assert!(!e.reasons.is_empty());
    }
}

#[test]
fn impact_by_qualified_symbol_does_not_include_same_tail_references() {
    let (_dir, db) = setup_qualified_rust_project();

    let req = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "Database::open".to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let entries = impact_analysis(&db, &req).unwrap();
    let paths: Vec<String> = entries.iter().map(|e| e.file_path.clone()).collect();

    assert!(
        paths.iter().any(|p| p.ends_with("db_user.rs")),
        "Database::open caller should be reported, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("conn_user.rs")),
        "Connection::open caller must not be reported for Database::open: {entries:?}"
    );
}

#[test]
fn impact_by_qualified_python_method_does_not_fallback_to_same_tail_calls() {
    let (_dir, db) = setup_ambiguous_python_project();

    let req = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "Repository.find".to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let entries = impact_analysis(&db, &req).unwrap();

    assert!(
        entries.is_empty(),
        "qualified dynamic-language methods without exact refs must not report ambiguous bare-name callers: {entries:?}"
    );
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
    let src_paths: Vec<String> = src.iter().map(|e| e.file_path.clone()).collect();
    assert!(src.iter().all(|e| e.category == FileCategory::Source));
    assert!(
        src_paths.iter().any(|p| p.ends_with("Controller.php")),
        "source_only should retain Controller.php caller, got {src_paths:?}"
    );
    assert!(
        src_paths.iter().any(|p| p.ends_with("Service.php")),
        "source_only should retain Service.php caller, got {src_paths:?}"
    );
    assert!(
        !src_paths.iter().any(|p| p.ends_with("RepositoryTest.php")),
        "source_only must exclude RepositoryTest.php, got {src_paths:?}"
    );
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

    let bundle = codesage_graph::export_context_for_symbol(&db, "Repository", &req).unwrap();

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

    let bundle = codesage_graph::export_context_for_symbol(&db, "Repository", &req).unwrap();
    let related_paths: Vec<String> = bundle.related.iter().map(|r| r.file_path.clone()).collect();

    assert!(
        !bundle.symbol_definitions.is_empty(),
        "should have found the definition"
    );
    assert!(
        related_paths.iter().any(|p| p.ends_with("Controller.php")),
        "include_callers should surface Controller.php, got {related_paths:?}"
    );
    assert!(
        related_paths.iter().any(|p| p.ends_with("Service.php")),
        "include_callers should surface Service.php, got {related_paths:?}"
    );
}

#[test]
fn export_context_for_qualified_symbol_uses_exact_callers() {
    let (_dir, db) = setup_qualified_rust_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("Database::open".to_string()),
        limit: 10,
        include_callers: true,
        include_callees: false,
    };

    let bundle = codesage_graph::export_context_for_symbol(&db, "Database::open", &req).unwrap();
    let related_paths: Vec<String> = bundle.related.iter().map(|r| r.file_path.clone()).collect();

    assert!(
        related_paths.iter().any(|p| p.ends_with("db_user.rs")),
        "Database::open caller should be included, got {related_paths:?}"
    );
    assert!(
        !related_paths.iter().any(|p| p.ends_with("conn_user.rs")),
        "Connection::open caller must not be included for Database::open: {related_paths:?}"
    );
}

#[test]
fn impact_by_ambiguous_bare_name_requires_disambiguation() {
    let (_dir, db) = setup_qualified_rust_project();

    let req = ImpactRequest {
        target: ImpactTarget::Symbol {
            name: "open".to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let err = impact_analysis(&db, &req).unwrap_err();
    assert!(
        err.to_string().contains("ambiguous symbol"),
        "expected disambiguation error, got: {err:#}"
    );
}

fn setup_ambiguous_helper_rust_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("helpers_a.rs"), b"pub fn helper() -> i32 { 1 }\n").unwrap();
    std::fs::write(root.join("helpers_b.rs"), b"pub fn helper() -> i32 { 2 }\n").unwrap();
    std::fs::write(
        root.join("service.rs"),
        b"use crate::helpers_a::helper;\npub fn run() { let _ = helper(); }\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    insert_chunk(
        &db,
        "helpers_a.rs",
        "rust",
        "pub fn helper() -> i32 { 1 }\n",
        1,
    );
    insert_chunk(
        &db,
        "helpers_b.rs",
        "rust",
        "pub fn helper() -> i32 { 2 }\n",
        1,
    );
    insert_chunk(
        &db,
        "service.rs",
        "rust",
        "use crate::helpers_a::helper;\npub fn run() { let _ = helper(); }\n",
        3,
    );
    (dir, db)
}

#[test]
fn export_context_callees_resolve_imported_helper_not_homonym() {
    let (_dir, db) = setup_ambiguous_helper_rust_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("run".to_string()),
        limit: 10,
        include_callers: false,
        include_callees: true,
    };

    let bundle = codesage_graph::export_context_for_symbol(&db, "run", &req).unwrap();
    let related_paths: Vec<String> = bundle.related.iter().map(|r| r.file_path.clone()).collect();

    assert!(
        related_paths.iter().any(|p| p.ends_with("helpers_a.rs")),
        "imported helper should be included, got {related_paths:?}"
    );
    assert!(
        !related_paths.iter().any(|p| p.ends_with("helpers_b.rs")),
        "unimported homonym helper must not appear: {related_paths:?}"
    );
}

#[test]
fn reverse_impact_attributes_caller_to_imported_helper_only() {
    // Reverse counterpart of the forward test above: `service.rs` imports
    // `helpers_a::helper`, so it must inflate `helpers_a`'s reverse blast radius
    // but NOT `helpers_b`'s homonym. Before import-aware reverse resolution,
    // `find_references` tail-matched "helper" and attributed the caller to both.
    let (_dir, db) = setup_ambiguous_helper_rust_project();

    let reverse_files = |path: &str| -> Vec<String> {
        impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File {
                    path: path.to_string(),
                },
                depth: 2,
                source_only: false,
            },
        )
        .unwrap()
        .iter()
        .map(|e| e.file_path.clone())
        .collect()
    };

    let files_a = reverse_files("helpers_a.rs");
    assert!(
        files_a.iter().any(|p| p.ends_with("service.rs")),
        "caller importing helpers_a must appear in its reverse impact: {files_a:?}"
    );

    let files_b = reverse_files("helpers_b.rs");
    assert!(
        !files_b.iter().any(|p| p.ends_with("service.rs")),
        "caller importing helpers_a must NOT inflate helpers_b's reverse impact: {files_b:?}"
    );
}

#[test]
fn export_context_for_symbol_includes_callees() {
    let (_dir, db) = setup_callee_rust_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("run".to_string()),
        limit: 10,
        include_callers: false,
        include_callees: true,
    };

    let bundle = codesage_graph::export_context_for_symbol(&db, "run", &req).unwrap();
    let related_paths: Vec<String> = bundle.related.iter().map(|r| r.file_path.clone()).collect();

    assert!(
        related_paths.iter().any(|p| p.ends_with("helpers.rs")),
        "helper callee should be included, got {related_paths:?}"
    );
}

#[test]
fn export_context_for_symbol_respects_limit_for_definitions_and_primary() {
    let (_dir, db) = setup_qualified_rust_project();

    let req = ExportRequest {
        query: None,
        symbol: Some("open".to_string()),
        limit: 1,
        include_callers: false,
        include_callees: false,
    };

    let bundle = codesage_graph::export_context_for_symbol(&db, "open", &req).unwrap();

    assert_eq!(bundle.symbol_definitions.len(), 1);
    assert_eq!(bundle.primary.len(), 1);
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

    let bundle = codesage_graph::export_context_for_symbol(&db, "NoSuchSymbol", &req).unwrap();
    assert!(bundle.primary.is_empty());
    assert!(bundle.symbol_definitions.is_empty());
    assert!(bundle.target_description.contains("not found"));
}

/// A same-namespace subclass writes `extends Base` with no `use` statement, so
/// the reference row records the short name. Keying the reverse lookup on the
/// symbol's qualified name matched `to_name` exactly and found none of them,
/// so a widely-inherited base class reported zero dependents.
#[test]
fn same_namespace_inheritance_is_not_lost_to_the_qualified_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("BaseHandler.php"),
        b"<?php\nnamespace App\\Handler;\nabstract class BaseHandler {\n  abstract public function handle();\n}\n",
    )
    .unwrap();
    // Same namespace as the base, so PHP needs no `use` and the ref is short.
    for (file, cls) in [
        ("AlphaHandler.php", "AlphaHandler"),
        ("BetaHandler.php", "BetaHandler"),
    ] {
        std::fs::write(
            root.join(file),
            format!(
                "<?php\nnamespace App\\Handler;\nclass {cls} extends BaseHandler {{\n  public function handle() {{ return 1; }}\n}}\n"
            ),
        )
        .unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

    let entries = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "BaseHandler".to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();

    let files: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.file_path.as_str()).collect();
    assert!(
        files.contains("AlphaHandler.php") && files.contains("BetaHandler.php"),
        "both same-namespace subclasses must appear as dependents, got {files:?}"
    );
}

/// A trait used by a class in its own namespace has the same shape as the
/// inheritance case: `use SomeTrait;` inside the class body records the short
/// name, and the qualified lookup missed it.
#[test]
fn same_namespace_trait_use_is_not_lost_to_the_qualified_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("LoggingTrait.php"),
        b"<?php\nnamespace App\\Support;\ntrait LoggingTrait {\n  public function logIt($m) { return $m; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Reporter.php"),
        b"<?php\nnamespace App\\Support;\nclass Reporter {\n  use LoggingTrait;\n  public function run() { return $this->logIt('x'); }\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

    let entries = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "LoggingTrait".to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert!(
        entries.iter().any(|e| e.file_path == "Reporter.php"),
        "the trait's user must appear as a dependent, got {:?}",
        entries.iter().map(|e| &e.file_path).collect::<Vec<_>>()
    );
}
