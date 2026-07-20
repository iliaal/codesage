//! `remove_file` must purge every per-path store — files (+ FK cascades),
//! semantic freshness, git history, feature membership, and EVERY model's
//! chunk table plus FTS sidecar — even when the database was opened
//! structural-only (no active model), as `codesage index --no-semantic`
//! does.

use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, FileInfo,
    Language, Symbol, SymbolKind,
};
use codesage_storage::Database;
use codesage_storage::schema::{fts_table_name, model_table_name};

fn file_info(path: &str) -> FileInfo {
    FileInfo {
        path: path.to_string(),
        language: Language::Rust,
        content_hash: "hash".to_string(),
    }
}

fn symbol(name: &str, file_path: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        qualified_name: name.to_string(),
        kind: SymbolKind::Function,
        file_path: file_path.to_string(),
        line_start: 1,
        line_end: 3,
        col_start: 0,
        col_end: 0,
        rationale: vec![],
    }
}

fn count_for_path(conn: &rusqlite::Connection, table: &str, col: &str, path: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM \"{table}\" WHERE \"{col}\" = ?1"),
        rusqlite::params![path],
        |r| r.get(0),
    )
    .unwrap_or_else(|e| panic!("count {table}.{col}={path}: {e}"))
}

#[test]
fn remove_file_purges_all_chunk_tables_fts_and_feature_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("idx.db");

    let table_a = model_table_name("model-a", 2);
    let table_b = model_table_name("model-b", 3);

    // Model A: files, symbols, chunks for both a.rs and keep.rs, one feature.
    {
        let db = Database::open_for_model(&db_path, "model-a", 2).expect("open model-a");
        let file_id = db.upsert_file(&file_info("a.rs")).unwrap();
        db.insert_symbols(file_id, &[symbol("gone", "a.rs")])
            .unwrap();
        db.upsert_file(&file_info("keep.rs")).unwrap();
        db.insert_chunks("a.rs", "rust", &[("fn gone() {}", 1, 3, &[0.0, 0.0])])
            .unwrap();
        db.insert_chunks("keep.rs", "rust", &[("fn keep() {}", 1, 3, &[0.0, 0.0])])
            .unwrap();
        db.upsert_feature(&FeatureRecord {
            feature_id: "feat_remove".to_string(),
            title: "Removal target".to_string(),
            summary: String::new(),
            kind: FeatureKind::Library,
            source: "test".to_string(),
            confidence: FeatureConfidence::High,
            entry_path: "keep.rs".to_string(),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            test_command: None,
            language: Language::Rust,
            tags: Vec::new(),
            trust_boundaries: Vec::new(),
            files: vec![
                FeatureFileRef {
                    path: "keep.rs".to_string(),
                    role: FeatureFileRole::Entry,
                    reason: None,
                },
                FeatureFileRef {
                    path: "a.rs".to_string(),
                    role: FeatureFileRole::Owned,
                    reason: None,
                },
            ],
        })
        .unwrap();
    }

    // Model B: a second chunk table for the same file. remove_file must purge
    // it even though the removing connection never opens model B.
    {
        let db = Database::open_for_model(&db_path, "model-b", 3).expect("open model-b");
        db.insert_chunks("a.rs", "rust", &[("fn gone() {}", 1, 3, &[0.0, 0.0, 0.0])])
            .unwrap();
    }

    // Git history and semantic-freshness rows are path-keyed, seeded directly.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("INSERT INTO git_files (path) VALUES ('a.rs')", [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO semantic_files (chunk_table, path, content_hash)
                 VALUES ('{table_a}', 'a.rs', 'hash'), ('{table_a}', 'keep.rs', 'hash')"
            ),
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count)
             VALUES ('a.rs', 'keep.rs', 1.0, 3)",
            [],
        )
        .unwrap();
    }

    // Structural-only open: no active chunk table — the failure mode under test.
    {
        let db = Database::open(&db_path).expect("structural-only open");
        db.remove_file("a.rs").expect("remove_file");
    }

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    assert_eq!(count_for_path(&conn, "files", "path", "a.rs"), 0);
    assert_eq!(count_for_path(&conn, "files", "path", "keep.rs"), 1);
    let symbols: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap();
    assert_eq!(symbols, 0, "symbols must cascade with the files row");

    assert_eq!(count_for_path(&conn, "semantic_files", "path", "a.rs"), 0);
    assert_eq!(
        count_for_path(&conn, "semantic_files", "path", "keep.rs"),
        1
    );
    assert_eq!(count_for_path(&conn, "git_files", "path", "a.rs"), 0);
    assert_eq!(count_for_path(&conn, "git_co_changes", "file_a", "a.rs"), 0);
    assert_eq!(count_for_path(&conn, "feature_files", "path", "a.rs"), 0);
    assert_eq!(count_for_path(&conn, "feature_files", "path", "keep.rs"), 1);

    for table in [&table_a, &table_b] {
        assert_eq!(
            count_for_path(&conn, table, "file_path", "a.rs"),
            0,
            "chunk table {table} must not retain removed file"
        );
        let fts = fts_table_name(table);
        assert_eq!(
            count_for_path(&conn, &fts, "file_path", "a.rs"),
            0,
            "fts sidecar {fts} must not retain removed file"
        );
    }
    assert_eq!(
        count_for_path(&conn, &table_a, "file_path", "keep.rs"),
        1,
        "other files' chunks must survive"
    );
    assert_eq!(
        count_for_path(&conn, &fts_table_name(&table_a), "file_path", "keep.rs"),
        1,
        "other files' fts rows must survive"
    );
}
