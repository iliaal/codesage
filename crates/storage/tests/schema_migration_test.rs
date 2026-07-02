//! Regression test for the to_name_tail migration ordering bug fixed in 6498ec2.
//!
//! Before that commit, init_db ran the SCHEMA batch (including
//! `CREATE INDEX ... ON refs(to_name_tail)`) before the migration that adds the column.
//! On a database that predated the column, init_db errored before the migration
//! could run. This test creates such a stale database, runs init_db, and asserts
//! that the column, index, and backfill all land correctly.

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

    // Seed some refs so we can verify the backfill happens.
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

    // Pre-condition: column does not exist yet.
    let has_col_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_col_before, 0, "test setup must use legacy schema");

    // Run init_db (should ALTER + backfill + create index).
    init_db(&conn).expect("init_db must succeed on legacy schema");

    // Column added.
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

    // Index created.
    let has_idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_refs_to_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_idx, 1, "idx_refs_to_name_tail must exist after init_db");

    // Backfill: each row's to_name_tail must equal name_tail(to_name).
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

    // schema_migrations table must exist.
    let has_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_table, 1);

    // Each migration name must be present exactly once after first init.
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
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![migration],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{migration} recorded on fresh DB");
    }

    // Running init_db again must be a no-op: count stays at 12.
    init_db(&conn).expect("second init_db");
    let count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count_after, 12,
        "second init_db must not re-apply migrations"
    );
}

#[test]
fn legacy_db_records_migration_after_upgrade() {
    let conn = Connection::open_in_memory().unwrap();
    create_old_schema(&conn);
    // Pre-condition: no schema_migrations yet.
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
