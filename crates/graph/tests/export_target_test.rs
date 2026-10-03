use codesage_graph::{TargetError, export_context, export_context_for_target, full_index};
use codesage_protocol::{DEFAULT_EMBEDDING_DIM, ExportRequest};
use codesage_storage::Database;

fn project() -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    for (path, source) in [
        (
            "src/a.rs",
            "pub fn search() {}\npub fn unique_anchor() {}\n",
        ),
        ("src/b.rs", "pub fn search() {}\n"),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    (root, db)
}

fn request(target: &str) -> ExportRequest {
    ExportRequest::from_target(target.to_string(), false, 5, true, true)
}

#[test]
fn an_unflagged_exact_symbol_uses_structural_resolution_without_a_query_embedding() {
    let (_root, db) = project();
    let bundle = export_context(&db, &[], None, &request("unique_anchor")).unwrap();
    assert!(bundle.found);
    assert_eq!(bundle.target_description, "symbol: unique_anchor");
    assert_eq!(bundle.symbol_definitions.len(), 1);
    assert_eq!(bundle.symbol_definitions[0].name, "unique_anchor");
}

#[test]
fn an_unflagged_ambiguous_symbol_returns_candidate_handles() {
    let (_root, db) = project();
    let error = export_context(&db, &[], None, &request("search")).unwrap_err();
    let target = error.downcast_ref::<TargetError>().unwrap();
    assert_eq!(target.code(), "E_AMBIGUOUS");
    assert_eq!(
        target.handles(),
        ["sym:src/a.rs#search", "sym:src/b.rs#search"]
    );
}

#[test]
fn exact_file_directory_and_chunk_targets_keep_definitions_without_semantic_chunks() {
    let (_root, db) = project();
    for (target, names) in [
        ("src/a.rs", vec!["search", "unique_anchor"]),
        ("file:src/a.rs", vec!["search", "unique_anchor"]),
        ("dir:src", vec!["search", "unique_anchor", "search"]),
        ("chunk:src/a.rs:2-2", vec!["unique_anchor"]),
        ("src/a.rs:2", vec!["unique_anchor"]),
    ] {
        let bundle = export_context(&db, &[], None, &request(target)).unwrap();
        assert!(bundle.found, "{target}");
        assert!(bundle.primary.is_empty(), "{target}");
        let definitions: Vec<_> = bundle
            .symbol_definitions
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(definitions, names, "{target}");
    }
}

#[test]
fn directory_bundles_preserve_literal_case_patterns_and_component_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let sources = [
        ("module/a.rs", "pub fn inside_lower() {}\n"),
        ("module/nested/b.rs", "pub fn inside_nested() {}\n"),
        ("MODULE/c.rs", "pub fn outside_upper() {}\n"),
        ("module_extra/d.rs", "pub fn outside_component() {}\n"),
        ("mod_le/e.rs", "pub fn inside_underscore() {}\n"),
        ("modXle/f.rs", "pub fn outside_underscore() {}\n"),
        ("mod%le/g.rs", "pub fn inside_percent() {}\n"),
        ("modZZle/h.rs", "pub fn outside_percent() {}\n"),
    ];
    for (path, source) in sources {
        std::fs::create_dir_all(root.path().join(path).parent().unwrap()).unwrap();
        std::fs::write(root.path().join(path), source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    let embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];
    for (path, source) in sources {
        db.insert_chunks(path, "rust", &[(source, 1, 1, &embedding)])
            .unwrap();
    }
    for (target, expected) in [
        ("dir:module", vec!["module/a.rs", "module/nested/b.rs"]),
        ("dir:MODULE", vec!["MODULE/c.rs"]),
        ("dir:mod_le", vec!["mod_le/e.rs"]),
        ("dir:mod%25le", vec!["mod%le/g.rs"]),
    ] {
        let bundle = export_context_for_target(&db, &request(target))
            .unwrap()
            .unwrap();
        let chunks: Vec<_> = bundle
            .primary
            .iter()
            .map(|row| row.file_path.as_str())
            .collect();
        let definitions: Vec<_> = bundle
            .symbol_definitions
            .iter()
            .map(|row| row.file_path.as_str())
            .collect();
        assert_eq!(chunks, expected, "{target}");
        assert_eq!(definitions, expected, "{target}");
    }
}

#[test]
fn entity_misses_are_typed_while_unresolved_text_requests_inference() {
    let (_root, db) = project();
    for target in [
        "Search",
        "sym:src/a.rs#gone",
        "file:gone.rs",
        "dir:gone",
        "chunk:gone.rs:1-2",
        "route:GET /gone",
        "cmd:gone",
    ] {
        let error = export_context_for_target(&db, &request(target)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<TargetError>().unwrap().code(),
            "E_NOT_FOUND",
            "{target}"
        );
    }
    assert!(
        export_context_for_target(&db, &request("explain how the anchor behaves"))
            .unwrap()
            .is_none()
    );
    let forced = ExportRequest::from_target("unindexed_symbol".to_string(), true, 5, false, false);
    assert!(
        !export_context_for_target(&db, &forced)
            .unwrap()
            .unwrap()
            .found
    );
}

#[test]
fn path_bundles_order_and_limit_chunks_and_preserve_chunk_range_selection() {
    let (_root, db) = project();
    let embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];
    db.insert_chunks(
        "src/a.rs",
        "rust",
        &[
            ("pub fn unique_anchor() {}", 2, 2, embedding.as_slice()),
            ("pub fn search() {}", 1, 1, embedding.as_slice()),
        ],
    )
    .unwrap();
    db.insert_chunks(
        "src/b.rs",
        "rust",
        &[("pub fn search() {}", 1, 1, embedding.as_slice())],
    )
    .unwrap();
    for (target, expected) in [
        ("file:src/a.rs", vec![("src/a.rs", 1), ("src/a.rs", 2)]),
        (
            "dir:src",
            vec![("src/a.rs", 1), ("src/a.rs", 2), ("src/b.rs", 1)],
        ),
        ("chunk:src/a.rs:2-2", vec![("src/a.rs", 2)]),
    ] {
        let bundle = export_context(&db, &[], None, &request(target)).unwrap();
        let actual: Vec<_> = bundle
            .primary
            .iter()
            .map(|row| (row.file_path.as_str(), row.start_line))
            .collect();
        assert_eq!(actual, expected, "{target}");
        let mut limited = request(target);
        limited.limit = 1;
        let bundle = export_context(&db, &[], None, &limited).unwrap();
        assert_eq!(bundle.primary.len(), 1, "{target}");
        assert_eq!(bundle.symbol_definitions.len(), 1, "{target}");
        assert!(bundle.related.len() <= 1, "{target}");
        limited.limit = 0;
        assert!(
            !export_context(&db, &[], None, &limited)
                .unwrap()
                .primary
                .is_empty()
        );
    }
}

#[test]
fn free_text_still_runs_semantic_retrieval() {
    let (_root, db) = project();
    let mut embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];
    embedding[0] = 1.0;
    db.insert_chunks(
        "src/a.rs",
        "rust",
        &[("pub fn unique_anchor() {}", 2, 2, embedding.as_slice())],
    )
    .unwrap();
    let bundle = export_context(
        &db,
        &embedding,
        None,
        &request("explain how the anchor behaves"),
    )
    .unwrap();
    assert_eq!(
        bundle.target_description,
        "query: explain how the anchor behaves"
    );
    assert_eq!(bundle.primary.len(), 1);
    assert_eq!(bundle.primary[0].file_path, "src/a.rs");
    assert_eq!(bundle.symbol_definitions[0].name, "unique_anchor");
}
