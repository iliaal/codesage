//! Feature bundles use mapped ownership and seeded chunks, without an embedder.

use codesage_features::map_features;
use codesage_graph::{feature_bundle, full_index};
use codesage_protocol::DEFAULT_EMBEDDING_DIM;
use codesage_storage::Database;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

fn seed_chunk(db: &Database, file_path: &str, language: &str, content: &str) {
    let embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];
    let end_line = content.lines().count().max(1) as u32;
    db.insert_chunks(
        file_path,
        language,
        &[(content, 1, end_line, embedding.as_slice())],
    )
    .unwrap();
}

#[test]
fn returns_empty_bundle_with_marker_when_feature_missing() {
    let db = Database::open_in_memory().unwrap();
    let bundle = feature_bundle(&db, "feat_does_not_exist", false, false, 5).unwrap();
    assert!(!bundle.found);
    assert!(bundle.target_description.contains("not found"));
    assert!(bundle.primary.is_empty());
    assert!(bundle.related.is_empty());
    assert!(bundle.symbol_definitions.is_empty());
}

#[test]
fn returns_bundle_with_curated_files_after_map() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    let main_src = "fn main() { println!(\"hi\"); }\n";
    let test_src = "#[test]\nfn it_works() { assert!(true); }\n";
    write(root, "src/main.rs", main_src);
    write(root, "tests/integration.rs", test_src);
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();

    seed_chunk(&db, "src/main.rs", "rust", main_src);
    seed_chunk(&db, "tests/integration.rs", "rust", test_src);

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 5).unwrap();
    assert!(bundle.found);
    assert!(
        bundle.target_description.contains(&main_feature.feature_id),
        "target_description should name the feature id, got {:?}",
        bundle.target_description
    );
    let primary_paths: Vec<&str> = bundle
        .primary
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(
        primary_paths.contains(&"src/main.rs"),
        "primary should include the entry file, got {:?}",
        primary_paths
    );
    let related_paths: Vec<&str> = bundle
        .related
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(
        related_paths.contains(&"tests/integration.rs"),
        "related should include the nearby test, got {:?}",
        related_paths
    );
    assert!(
        bundle
            .symbol_definitions
            .iter()
            .any(|s| s.name == "main" && s.file_path == "src/main.rs"),
        "symbol_definitions should include the entry symbol, got {:?}",
        bundle
            .symbol_definitions
            .iter()
            .map(|s| (s.name.clone(), s.file_path.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn entry_chunk_overlaps_entry_symbol_not_first_chunk() {
    // Leading imports must not displace the entry symbol's body from the bundle.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    let mut content = String::new();
    for _ in 0..100 {
        content.push_str("use std::path::Path;\n");
    }
    content.push_str("fn main() { println!(\"hi\"); }\n");
    write(root, "src/main.rs", &content);
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    // Only the second chunk overlaps `main`.
    let imports = content.lines().take(50).collect::<Vec<_>>().join("\n");
    let body = content.lines().skip(50).collect::<Vec<_>>().join("\n");
    db.insert_chunks(
        "src/main.rs",
        "rust",
        &[
            (
                imports.as_str(),
                1,
                50,
                vec![0.0; DEFAULT_EMBEDDING_DIM].as_slice(),
            ),
            (
                body.as_str(),
                51,
                101,
                vec![0.0; DEFAULT_EMBEDDING_DIM].as_slice(),
            ),
        ],
    )
    .unwrap();
    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature");
    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 5).unwrap();
    let entry_chunk = bundle
        .primary
        .iter()
        .find(|r| r.file_path == "src/main.rs")
        .expect("entry chunk present");
    assert!(
        entry_chunk.start_line <= 101 && entry_chunk.end_line >= 101,
        "entry chunk must cover `fn main` at line 101, got {}..{}",
        entry_chunk.start_line,
        entry_chunk.end_line
    );
    assert!(
        entry_chunk.content.contains("fn main"),
        "entry chunk must contain the function body, got first 80 chars: {:?}",
        &entry_chunk.content[..entry_chunk.content.len().min(80)]
    );
}

#[test]
fn caller_expansion_reserves_related_capacity_when_tests_saturate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "src/helpers.rs", "pub fn helper() {}\n");
    write(root, "src/lib.rs", "pub mod helpers;\n");
    write(
        root,
        "src/main.rs",
        "use acme::helpers::helper;\nfn main() { helper(); }\n",
    );
    for index in 0..5 {
        write(
            root,
            &format!("tests/integration_{index}.rs"),
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    seed_chunk(&db, "src/helpers.rs", "rust", "pub fn helper() {}\n");
    seed_chunk(
        &db,
        "src/main.rs",
        "rust",
        "use acme::helpers::helper;\nfn main() { helper(); }\n",
    );
    for index in 0..5 {
        seed_chunk(
            &db,
            &format!("tests/integration_{index}.rs"),
            "rust",
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, true, true, 5).unwrap();
    assert!(
        bundle
            .primary
            .iter()
            .all(|c| c.file_path != "src/helpers.rs")
    );
    let related_paths: Vec<&str> = bundle
        .related
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(
        related_paths.contains(&"src/helpers.rs"),
        "callee helper file should appear in related[], got {related_paths:?}"
    );
    assert_eq!(related_paths.len(), 5, "related[] must respect limit");
    assert!(
        related_paths.iter().any(|path| path.starts_with("tests/")),
        "tests should backfill the capacity left by graph expansion"
    );
}

#[test]
fn caller_expansion_reserves_exactly_two_slots_with_three_callees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "src/alpha.rs", "pub fn alpha_helper() {}\n");
    write(root, "src/beta.rs", "pub fn beta_helper() {}\n");
    write(root, "src/gamma.rs", "pub fn gamma_helper() {}\n");
    write(
        root,
        "src/lib.rs",
        "pub mod alpha;\npub mod beta;\npub mod gamma;\n",
    );
    let main_src = "use acme::alpha::alpha_helper;\nuse acme::beta::beta_helper;\nuse acme::gamma::gamma_helper;\nfn main() { alpha_helper(); beta_helper(); gamma_helper(); }\n";
    write(root, "src/main.rs", main_src);
    for index in 0..5 {
        write(
            root,
            &format!("tests/integration_{index}.rs"),
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    seed_chunk(&db, "src/alpha.rs", "rust", "pub fn alpha_helper() {}\n");
    seed_chunk(&db, "src/beta.rs", "rust", "pub fn beta_helper() {}\n");
    seed_chunk(&db, "src/gamma.rs", "rust", "pub fn gamma_helper() {}\n");
    seed_chunk(&db, "src/main.rs", "rust", main_src);
    for index in 0..5 {
        seed_chunk(
            &db,
            &format!("tests/integration_{index}.rs"),
            "rust",
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, true, true, 5).unwrap();
    assert!(
        bundle
            .primary
            .iter()
            .all(|c| !["src/alpha.rs", "src/beta.rs", "src/gamma.rs"]
                .contains(&c.file_path.as_str()))
    );
    let related_paths: Vec<&str> = bundle
        .related
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    let callee_count = related_paths
        .iter()
        .filter(|path| path.starts_with("src/"))
        .count();
    let test_count = related_paths
        .iter()
        .filter(|path| path.starts_with("tests/"))
        .count();
    assert_eq!(
        callee_count, 2,
        "graph expansion must reserve exactly two related slots, got {related_paths:?}"
    );
    assert_eq!(test_count, 3, "tests must backfill the remaining slots");
    assert_eq!(related_paths.len(), 5, "related[] must respect limit");
}

#[test]
fn caller_expansion_with_limit_below_reservation_does_not_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "src/helpers.rs", "pub fn helper() {}\n");
    write(root, "src/lib.rs", "pub mod helpers;\n");
    write(
        root,
        "src/main.rs",
        "use acme::helpers::helper;\nfn main() { helper(); }\n",
    );
    write(
        root,
        "tests/integration.rs",
        "#[test]\nfn ok() { assert!(true); }\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    seed_chunk(&db, "src/helpers.rs", "rust", "pub fn helper() {}\n");
    seed_chunk(
        &db,
        "src/main.rs",
        "rust",
        "use acme::helpers::helper;\nfn main() { helper(); }\n",
    );
    seed_chunk(
        &db,
        "tests/integration.rs",
        "rust",
        "#[test]\nfn ok() { assert!(true); }\n",
    );

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, true, true, 1).unwrap();
    assert_eq!(bundle.related.len(), 1);
    assert_eq!(bundle.related[0].file_path, "src/helpers.rs");
    assert!(
        bundle.related.len() <= 1,
        "related[] must respect a limit below the two-slot reservation, got {:?}",
        bundle
            .related
            .iter()
            .map(|c| c.file_path.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn expansion_with_no_resolvable_callees_leaves_tests_full_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    let main_src = "fn main() { println!(\"no calls to owned code\"); }\n";
    write(root, "src/main.rs", main_src);
    for index in 0..5 {
        write(
            root,
            &format!("tests/integration_{index}.rs"),
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    seed_chunk(&db, "src/main.rs", "rust", main_src);
    for index in 0..5 {
        seed_chunk(
            &db,
            &format!("tests/integration_{index}.rs"),
            "rust",
            &format!("#[test]\nfn integration_{index}() {{ assert!(true); }}\n"),
        );
    }

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, true, true, 5).unwrap();
    let related_paths: Vec<&str> = bundle
        .related
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert_eq!(
        related_paths.len(),
        5,
        "tests keep the full related budget when expansion resolves nothing, got {related_paths:?}"
    );
    assert!(
        related_paths.iter().all(|path| path.starts_with("tests/")),
        "every related slot should be a test chunk, got {related_paths:?}"
    );
}

#[test]
fn entry_chunk_present_when_owned_lib_sorts_before_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    write(root, "src/lib.rs", "pub fn lib_fn() {}\n");
    write(root, "src/main.rs", "fn main() {}\n");
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    seed_chunk(&db, "src/lib.rs", "rust", "pub fn lib_fn() {}\n");
    seed_chunk(&db, "src/main.rs", "rust", "use std::io;\nfn main() {}\n");

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 1).unwrap();
    let primary_paths: Vec<&str> = bundle
        .primary
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(
        primary_paths.contains(&"src/main.rs"),
        "entry file must stay in primary[] even with a low limit and owned lib.rs, got {primary_paths:?}"
    );
}

#[test]
fn missing_chunks_yield_empty_primary_but_keep_metadata() {
    // Structural metadata remains available without semantic chunks.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    write(root, "src/main.rs", "fn main() {}\n");
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    map_features(root, &db, &[]).unwrap();
    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features.first().expect("at least one feature");
    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 5).unwrap();
    assert!(bundle.primary.is_empty());
    assert!(bundle.related.is_empty());
    assert!(
        bundle.symbol_definitions.iter().any(|s| s.name == "main"),
        "entry symbol should still come back even without chunks"
    );
}
