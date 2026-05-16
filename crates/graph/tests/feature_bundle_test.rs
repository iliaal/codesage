//! `feature_bundle` end-to-end: map features against a tiny fixture repo,
//! seed chunks for the relevant files (semantic indexer requires a real
//! embedder; we shortcut by inserting chunks directly), call
//! `feature_bundle` on the resulting feature_id, and verify the bundle
//! tracks the curated file list rather than semantic search results.

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
    full_index(root, &db, &[]).unwrap();
    map_features(root, &db).unwrap();

    // Seed chunks directly (avoids spinning up a real embedder in tests).
    seed_chunk(&db, "src/main.rs", "rust", main_src);
    seed_chunk(&db, "tests/integration.rs", "rust", test_src);

    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features
        .iter()
        .find(|f| f.entry_path == "src/main.rs")
        .expect("main feature mapped");

    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 5).unwrap();
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
    // The nearby-test discovery attaches tests/integration.rs to Rust
    // binaries by convention; the bundle's related[] should carry it.
    assert!(
        related_paths.contains(&"tests/integration.rs"),
        "related should include the nearby test, got {:?}",
        related_paths
    );
    // The bundle must include the entry symbol's definition (Rust `main`)
    // because the feature has entry_symbol = "main".
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
    // Regression: real codesage binary main.rs has 447 lines of `use`
    // statements before `fn main()`. The bundle's entry chunk must
    // overlap the symbol, not just be the file's first chunk — otherwise
    // an agent reviewing the feature gets imports instead of the body.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    // Synthesize a large entry file: 100 import lines, then main() at L101.
    let mut content = String::new();
    for _ in 0..100 {
        content.push_str("use std::path::Path;\n");
    }
    content.push_str("fn main() { println!(\"hi\"); }\n");
    write(root, "src/main.rs", &content);
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[]).unwrap();
    map_features(root, &db).unwrap();
    // Seed two chunks: imports L1-50, body L51-101. Without the fix, the
    // bundle would pick L1-50; with the fix it picks the L51-101 chunk
    // because that's where `fn main` sits.
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
fn missing_chunks_yield_empty_primary_but_keep_metadata() {
    // Feature mapped but never semantically indexed: the bundle returns
    // metadata + the entry symbol definition (loaded from the symbol
    // table, which structural index populated), but `primary` / `related`
    // come up empty because no chunks exist.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
    );
    write(root, "src/main.rs", "fn main() {}\n");
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[]).unwrap();
    map_features(root, &db).unwrap();
    let features = db.features_for_file("src/main.rs").unwrap();
    let main_feature = features.first().expect("at least one feature");
    let bundle = feature_bundle(&db, &main_feature.feature_id, false, false, 5).unwrap();
    assert!(bundle.primary.is_empty());
    assert!(bundle.related.is_empty());
    // Entry symbol still resolvable from symbols table.
    assert!(
        bundle.symbol_definitions.iter().any(|s| s.name == "main"),
        "entry symbol should still come back even without chunks"
    );
}
