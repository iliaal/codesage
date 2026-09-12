//! Legacy databases need to_name_tail added before its index, with existing refs backfilled.

use codesage_storage::schema::{BREAKING_MIGRATION_PREFIX, init_db, name_tail};
use rusqlite::Connection;

fn create_old_schema(conn: &Connection) {
    // Schema as it existed before the to_name_tail column was added.
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            line_start INTEGER NOT NULL,
            line_end INTEGER NOT NULL,
            col_start INTEGER NOT NULL,
            col_end INTEGER NOT NULL
        );
        CREATE TABLE refs (
            id INTEGER PRIMARY KEY,
            from_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            from_symbol TEXT,
            to_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            line INTEGER NOT NULL,
            col INTEGER NOT NULL
        );
        CREATE INDEX idx_refs_to_name ON refs(to_name);
        CREATE INDEX idx_refs_from_file ON refs(from_file_id);
        "#,
    )
    .unwrap();
}

#[test]
fn migrates_legacy_schema_to_current() {
    let conn = Connection::open_in_memory().unwrap();
    create_old_schema(&conn);

    conn.execute(
        "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'a.rs', 'rust', 'h')",
        [],
    )
    .unwrap();
    let cases = [
        (1, "App\\Http\\Controllers\\Foo"), // PHP-style
        (2, "mod::sub::bar"),               // Rust-style
        (3, "a/b/c"),                       // path-style
        (4, "PlainName"),                   // no separator
    ];
    for (id, to_name) in &cases {
        conn.execute(
            "INSERT INTO refs (id, from_file_id, from_symbol, to_name, kind, line, col)
             VALUES (?1, 1, NULL, ?2, 'use', 1, 1)",
            rusqlite::params![id, to_name],
        )
        .unwrap();
    }

    let has_col_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_col_before, 0, "test setup must use legacy schema");

    init_db(&conn).expect("init_db must succeed on legacy schema");

    let has_col_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        has_col_after, 1,
        "to_name_tail column must exist after init_db"
    );

    let has_idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_refs_to_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_idx, 1, "idx_refs_to_name_tail must exist after init_db");

    for (id, to_name) in &cases {
        let tail: String = conn
            .query_row(
                "SELECT to_name_tail FROM refs WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tail,
            name_tail(to_name),
            "backfill mismatch for id={id} to_name={to_name}"
        );
    }
}

#[test]
fn init_db_is_idempotent_on_current_schema() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    init_db(&conn).expect("second init_db must be a no-op");
    init_db(&conn).expect("third init_db must still be a no-op");
}

#[test]
fn init_db_adds_v2b_git_tables_to_legacy_db() {
    let conn = Connection::open_in_memory().unwrap();
    create_old_schema(&conn);

    // Legacy DB has files/symbols/refs but no git_files/git_co_changes.
    init_db(&conn).expect("init_db must succeed on legacy schema");

    for table in &[
        "git_files",
        "git_co_changes",
        "semantic_files",
        "semantic_models",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "table {table} must be created by init_db");
    }
    for index in &[
        "idx_git_files_churn",
        "idx_git_co_changes_file_a",
        "idx_git_co_changes_file_b",
        "idx_semantic_models_model",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                rusqlite::params![index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "index {index} must be created by init_db");
    }
}

#[test]
fn fresh_db_records_migrations_exactly_once() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("init_db on fresh DB");

    let has_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_table, 1);

    let expected_migrations = [
        "0001_refs_name_tail",
        "0002_structural_index_state",
        "0003_semantic_files",
        "0004_semantic_files_chunk_table",
        "0005_semantic_models",
        "0006_refs_name_tail_dot",
        "0007_symbols_rationale",
        "0008_file_trust_boundaries",
        "0009_feature_tables",
        "0010_files_boundaries_derived_at",
        "0011_features_test_command",
        "0012_symbol_fingerprints",
        "0013_structural_unique_keys",
        "0014_git_files_churn_path",
        "0015_semantic_models_fingerprint",
        "0016_semantic_models_artifact_stat_key",
        "0017_git_co_changes_recurrence",
        "0018_git_author_events",
        "0019_file_hash_cache",
        "0020_file_interpretation",
        "0021_git_history_anchor",
    ];
    for migration in expected_migrations {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![migration],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{migration} recorded on fresh DB");
    }

    init_db(&conn).expect("second init_db");
    let count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count_after,
        expected_migrations.len() as i64,
        "second init_db must not re-apply migrations"
    );
}

#[test]
fn legacy_interpretation_is_unknown_without_rewriting_raw_hashes() {
    let conn = Connection::open_in_memory().unwrap();
    create_old_schema(&conn);
    conn.execute(
        "INSERT INTO files (path, language, content_hash) VALUES ('old.py', 'python', 'raw-hash')",
        [],
    )
    .unwrap();
    init_db(&conn).unwrap();
    init_db(&conn).unwrap();
    let row: (String, Option<String>) = conn
        .query_row(
            "SELECT content_hash, interpretation FROM files WHERE path = 'old.py'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("raw-hash".to_string(), None));
}

#[test]
fn legacy_db_records_migration_after_upgrade() {
    let conn = Connection::open_in_memory().unwrap();
    create_old_schema(&conn);
    let has_table_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_table_before, 0);

    init_db(&conn).expect("init_db on legacy DB");

    for migration in [
        "0001_refs_name_tail",
        "0002_structural_index_state",
        "0003_semantic_files",
        "0004_semantic_files_chunk_table",
        "0005_semantic_models",
        "0006_refs_name_tail_dot",
        "0007_symbols_rationale",
        "0008_file_trust_boundaries",
        "0009_feature_tables",
        "0010_files_boundaries_derived_at",
        "0011_features_test_command",
        "0012_symbol_fingerprints",
        "0013_structural_unique_keys",
        "0014_git_files_churn_path",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![migration],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{migration} recorded after legacy upgrade");
    }
}

#[test]
fn intermediate_db_records_0008_through_0012_after_upgrade() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("initial current schema");

    conn.execute_batch(
        "DROP TABLE file_trust_boundaries;
         DROP TABLE features;
         DROP TABLE feature_files;
         DROP TABLE feature_trust_boundaries;
         DROP TABLE symbol_fingerprints;
         DELETE FROM schema_migrations
         WHERE name IN (
             '0008_file_trust_boundaries',
             '0009_feature_tables',
             '0010_files_boundaries_derived_at',
             '0011_features_test_command',
             '0012_symbol_fingerprints'
         );",
    )
    .unwrap();

    init_db(&conn).expect("init_db applies post-0007 migrations");

    for table in &[
        "file_trust_boundaries",
        "features",
        "feature_files",
        "feature_trust_boundaries",
        "symbol_fingerprints",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "table {table} must exist after upgrade");
    }

    for column in [
        ("files", "boundaries_derived_at"),
        ("features", "test_command"),
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                rusqlite::params![column.0, column.1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "column {}.{} must exist after upgrade",
            column.0, column.1
        );
    }

    let symfp_name_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_symfp_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(symfp_name_index, 1);

    for migration in [
        "0008_file_trust_boundaries",
        "0009_feature_tables",
        "0010_files_boundaries_derived_at",
        "0011_features_test_command",
        "0012_symbol_fingerprints",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![migration],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{migration} recorded after intermediate upgrade");
    }
}

#[test]
fn migrates_path_only_semantic_files_to_chunk_table_scoped_shape() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("initial current schema");

    conn.execute_batch(
        "DELETE FROM schema_migrations WHERE name = '0004_semantic_files_chunk_table';
         DROP TABLE semantic_files;
         CREATE TABLE semantic_files (
             path TEXT PRIMARY KEY,
             content_hash TEXT NOT NULL,
             indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         INSERT INTO semantic_files (path, content_hash) VALUES ('a.rs', 'old');",
    )
    .unwrap();

    init_db(&conn).expect("init_db migrates path-only semantic_files");

    let has_chunk_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('semantic_files') WHERE name = 'chunk_table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_chunk_table, 1);

    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM semantic_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "path-only freshness rows must be discarded");

    let index_table: String = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_semantic_files_path'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index_table, "semantic_files");

    let migration_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0004_semantic_files_chunk_table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migration_recorded, 1);
}

/// Minimal subscriber that records the debug-rendered fields of every WARN
/// event. Just enough to prove `init_db` warns loudly on forward-compat
/// detection without pulling in tracing-subscriber.
struct WarnCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, "{}={:?} ", field.name(), value);
            }
        }
        if *event.metadata().level() == tracing::Level::WARN {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn unknown_additive_migration_from_newer_binary_warns_but_opens() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES ('9999_from_the_future')",
        [],
    )
    .unwrap();

    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tracing::subscriber::with_default(WarnCapture(warnings.clone()), || {
        init_db(&conn).expect("unknown additive migration must not block an old binary");
    });

    let warnings = warnings.lock().unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("9999_from_the_future")),
        "init_db must warn naming the unknown migration, got: {warnings:?}"
    );
}

/// An index stamped by the development build that named migration 0017
/// `0017_git_co_changes_windows` (and only added the `windows` column) must
/// open silently: the superseded row is dropped, the renamed migration runs
/// and adds the missing columns, and no "newer codesage" warning fires.
#[test]
fn superseded_0017_name_is_forgotten_and_the_renamed_migration_completes_it() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    // Rewind to the round-1 shape: only `windows` exists, stamped under the
    // old name.
    conn.execute_batch(
        "ALTER TABLE git_co_changes DROP COLUMN first_observed_at;
         ALTER TABLE git_co_changes DROP COLUMN window_mask;
         DELETE FROM schema_migrations WHERE name = '0017_git_co_changes_recurrence';
         INSERT INTO schema_migrations (name) VALUES ('0017_git_co_changes_windows');",
    )
    .unwrap();

    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tracing::subscriber::with_default(WarnCapture(warnings.clone()), || {
        init_db(&conn).expect("second init_db");
    });
    assert!(
        warnings.lock().unwrap().is_empty(),
        "superseded name must not warn, got: {:?}",
        warnings.lock().unwrap()
    );

    let names: Vec<String> = conn
        .prepare("SELECT name FROM schema_migrations WHERE name LIKE '0017%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(names, vec!["0017_git_co_changes_recurrence".to_string()]);
    for column in ["first_observed_at", "window_mask", "windows"] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('git_co_changes') WHERE name = ?1",
                rusqlite::params![column],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            present, 1,
            "{column} must exist after the renamed 0017 runs"
        );
    }
    init_db(&conn).expect("third init_db");
}

#[test]
fn breaking_migration_marker_from_newer_binary_refuses_open() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    let marker = format!("{BREAKING_MIGRATION_PREFIX}9999_drop_everything");
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES (?1)",
        rusqlite::params![marker],
    )
    .unwrap();

    let err = init_db(&conn).expect_err("breaking marker must refuse open");
    let msg = err.to_string();
    assert!(
        msg.contains(&marker),
        "error must name the breaking migration: {msg}"
    );
    assert!(
        msg.contains("upgrade codesage"),
        "error must tell the operator how to recover: {msg}"
    );
}

#[test]
fn unique_key_migration_dedupes_existing_duplicates() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("initial current schema");

    // Roll back to the pre-0013 state so duplicates can be seeded.
    conn.execute_batch(
        "DROP INDEX uq_symbols_identity;
         DROP INDEX uq_refs_identity;
         DROP INDEX uq_symfp_identity;
         DELETE FROM schema_migrations WHERE name = '0013_structural_unique_keys';",
    )
    .unwrap();

    conn.execute_batch(
        "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'a.rs', 'rust', 'h');
         INSERT INTO symbols (id, file_id, name, qualified_name, kind,
                              line_start, line_end, col_start, col_end)
         VALUES (10, 1, 'foo', 'a::foo', 'function', 1, 3, 0, 10),
                (11, 1, 'foo', 'a::foo', 'function', 1, 3, 0, 10),
                (12, 1, 'bar', 'a::bar', 'function', 5, 8, 0, 10);
         INSERT INTO refs (id, from_file_id, from_symbol, to_name, to_name_tail, kind, line, col)
         VALUES (20, 1, NULL, 'baz', 'baz', 'call', 2, 4),
                (21, 1, NULL, 'baz', 'baz', 'call', 2, 4),
                (22, 1, NULL, 'qux', 'qux', 'call', 3, 4);
         INSERT INTO symbol_fingerprints (file_id, name, kind, line_start, line_end, leaf_count, fp)
         VALUES (1, 'foo', 'function', 1, 3, 7, zeroblob(512)),
                (1, 'foo', 'function', 1, 3, 7, zeroblob(512)),
                (1, 'bar', 'function', 5, 8, 9, zeroblob(512));",
    )
    .unwrap();

    init_db(&conn).expect("0013 must dedupe then create the unique indexes");

    // Lowest id per natural key survives.
    let symbol_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM symbols ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        symbol_ids,
        vec![10, 12],
        "duplicate symbol row must be gone"
    );

    let ref_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM refs ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(ref_ids, vec![20, 22], "duplicate ref row must be gone");

    let fp_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbol_fingerprints", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fp_count, 2, "duplicate fingerprint row must be gone");

    for index in [
        "uq_symbols_identity",
        "uq_refs_identity",
        "uq_symfp_identity",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                rusqlite::params![index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "index {index} must exist after migration");
    }
}

fn sample_file_info() -> codesage_protocol::FileInfo {
    codesage_protocol::FileInfo {
        path: "src/sample.rs".to_string(),
        language: codesage_protocol::Language::Rust,
        content_hash: "h1".to_string(),
    }
}

fn sample_symbol() -> codesage_protocol::Symbol {
    codesage_protocol::Symbol {
        name: "foo".to_string(),
        qualified_name: "sample::foo".to_string(),
        kind: codesage_protocol::SymbolKind::Function,
        file_path: "src/sample.rs".to_string(),
        line_start: 1,
        line_end: 3,
        col_start: 0,
        col_end: 10,
        rationale: Vec::new(),
    }
}

fn sample_reference(kind: codesage_protocol::ReferenceKind) -> codesage_protocol::Reference {
    codesage_protocol::Reference {
        from_file: "src/sample.rs".to_string(),
        from_symbol: None,
        to_name: "bar".to_string(),
        kind,
        line: 2,
        col: 4,
    }
}

#[test]
fn unique_backstop_rejects_double_insert_without_delete() {
    use codesage_protocol::ReferenceKind;
    use codesage_storage::Database;
    use codesage_storage::db::FingerprintInput;

    let db = Database::open_in_memory().unwrap();
    let file_id = db.upsert_file(&sample_file_info()).unwrap();

    let symbols = vec![sample_symbol()];
    db.insert_symbols(file_id, &symbols).unwrap();
    assert!(
        db.insert_symbols(file_id, &symbols).is_err(),
        "re-inserting the same symbol without the per-file delete must hit \
         the unique backstop"
    );

    let refs = vec![sample_reference(ReferenceKind::Call)];
    db.insert_references(file_id, &refs).unwrap();
    assert!(
        db.insert_references(file_id, &refs).is_err(),
        "re-inserting the same ref without the per-file delete must hit \
         the unique backstop"
    );

    let fp: Vec<u64> = vec![0; 64];
    let fps = vec![FingerprintInput {
        name: "foo",
        kind: "function",
        line_start: 1,
        line_end: 3,
        leaf_count: 7,
        fp: &fp,
    }];
    db.insert_fingerprints(file_id, &fps).unwrap();
    assert!(
        db.insert_fingerprints(file_id, &fps).is_err(),
        "re-inserting the same fingerprint without the per-file delete must \
         hit the unique backstop"
    );
}

#[test]
fn route_handler_refs_stay_exempt_from_unique_backstop() {
    use codesage_protocol::ReferenceKind;
    use codesage_storage::Database;

    // Mapper-owned route edges use col = 0; same-line registrations may share a handler.
    let db = Database::open_in_memory().unwrap();
    let file_id = db.upsert_file(&sample_file_info()).unwrap();
    let refs = vec![sample_reference(ReferenceKind::RouteHandler)];
    db.insert_references(file_id, &refs).unwrap();
    db.insert_references(file_id, &refs)
        .expect("duplicate route_handler edges must not trip the backstop");
}

#[test]
fn reindex_delete_then_insert_still_works_with_backstop() {
    use codesage_protocol::ReferenceKind;
    use codesage_storage::Database;
    use codesage_storage::db::FingerprintInput;

    let db = Database::open_in_memory().unwrap();
    let fp: Vec<u64> = vec![0; 64];
    for _pass in 0..2 {
        let file_id = db.upsert_file(&sample_file_info()).unwrap();
        db.insert_symbols(file_id, &[sample_symbol()]).unwrap();
        db.insert_references(file_id, &[sample_reference(ReferenceKind::Call)])
            .unwrap();
        db.insert_fingerprints(
            file_id,
            &[FingerprintInput {
                name: "foo",
                kind: "function",
                line_start: 1,
                line_end: 3,
                leaf_count: 7,
                fp: &fp,
            }],
        )
        .unwrap();
    }
    let n: usize = db.symbol_count().unwrap();
    assert_eq!(n, 1, "reindex cycle must leave exactly one symbol row");
}

#[test]
fn name_tail_handles_separators() {
    assert_eq!(name_tail("App\\Http\\Controllers\\Foo"), "Foo");
    assert_eq!(name_tail("mod::sub::bar"), "bar");
    assert_eq!(name_tail("fmt.Println"), "Println");
    assert_eq!(name_tail("UserService.findAll"), "findAll");
    assert_eq!(name_tail("a/b/c"), "c");
    assert_eq!(name_tail("PlainName"), "PlainName");
    assert_eq!(name_tail(""), "");
    // Mixed separators: rightmost wins
    assert_eq!(name_tail("a/b::c"), "c");
    assert_eq!(name_tail("a::b/c"), "c");
    assert_eq!(name_tail("a.b::c"), "c");
}

#[test]
fn dot_tail_migration_backfills_existing_refs() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("initial current schema");

    conn.execute_batch(
        "DELETE FROM schema_migrations WHERE name = '0006_refs_name_tail_dot';
         INSERT INTO files (id, path, language, content_hash)
         VALUES (1, 'sample.go', 'go', 'h');
         INSERT INTO refs (id, from_file_id, from_symbol, to_name, to_name_tail, kind, line, col)
         VALUES (1, 1, NULL, 'fmt.Println', 'fmt.Println', 'call', 49, 1);",
    )
    .unwrap();

    init_db(&conn).expect("init_db reruns dot-tail migration");

    let tail: String = conn
        .query_row("SELECT to_name_tail FROM refs WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(tail, "Println");

    let migration_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0006_refs_name_tail_dot'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migration_recorded, 1);
}
