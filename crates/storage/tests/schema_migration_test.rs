//! Regression test for the to_name_tail migration ordering bug fixed in 6498ec2.
//!
//! Before that commit, init_db ran the SCHEMA batch (including
//! `CREATE INDEX ... ON refs(to_name_tail)`) before the migration that adds the column.
//! On a database that predated the column, init_db errored before the migration
//! could run. This test creates such a stale database, runs init_db, and asserts
//! that the column, index, and backfill all land correctly.

use codesage_storage::schema::{init_db, name_tail};
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

    for table in &["git_files", "git_co_changes"] {
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
    let count_0001: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0001_refs_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_0001, 1, "0001 recorded on fresh DB");

    // Running init_db again must be a no-op: count stays at 1.
    init_db(&conn).expect("second init_db");
    let count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0001_refs_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_after, 1, "second init_db must not re-apply migrations");
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

    // schema_migrations exists with 0001 recorded.
    let count_0001: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0001_refs_name_tail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_0001, 1);
}

#[test]
fn name_tail_handles_separators() {
    assert_eq!(name_tail("App\\Http\\Controllers\\Foo"), "Foo");
    assert_eq!(name_tail("mod::sub::bar"), "bar");
    assert_eq!(name_tail("a/b/c"), "c");
    assert_eq!(name_tail("PlainName"), "PlainName");
    assert_eq!(name_tail(""), "");
    // Mixed separators: rightmost wins
    assert_eq!(name_tail("a/b::c"), "c");
    assert_eq!(name_tail("a::b/c"), "c");
}
