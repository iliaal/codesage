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

    let import_edge_index: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_refs_import_edges'",
            [],
            |r| r.get(0),
        )
        .expect("idx_refs_import_edges must exist after init_db");
    assert!(
        import_edge_index.contains(
            "WHERE kind IN ('import', 'import_binding', 'include', 'inheritance', 'trait_use')"
        ),
        "{import_edge_index}"
    );

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
        "0022_refs_lazy",
        "0023_symbols_visibility",
        "0024_is_test",
        "0025_git_exclusion_fingerprint",
        "0026_python_bindings",
        "0027_symbol_signatures",
        "0029_feature_entrypoints",
        "0030_git_history_build",
        "0031_git_history_build_v2",
        "0032_refs_import_edges",
        "0033_js_aliases",
        "0034_php_nocase_lookups",
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

    let lookup_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_symbols_file_qualified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lookup_index, 1, "base schema carries the lookup index");

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
fn legacy_rows_read_as_product_code_until_reparsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let conn = Connection::open(&path).unwrap();
    create_old_schema(&conn);
    conn.execute_batch(
        "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'tests/old.rs', 'rust', 'h');
         INSERT INTO symbols (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
         VALUES (1, 'helper', 'tests::helper', 'function', 1, 2, 0, 1);
         INSERT INTO refs (from_file_id, from_symbol, to_name, kind, line, col)
         VALUES (1, 'tests::helper', 'target', 'call', 2, 4);",
    )
    .unwrap();
    init_db(&conn).unwrap();
    let flags: (i64, i64) = conn
        .query_row(
            "SELECT f.is_test, s.is_test FROM files f JOIN symbols s ON s.file_id = f.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        flags,
        (0, 0),
        "0024 must default legacy rows to product code"
    );
    let lookup_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_symbols_file_qualified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        lookup_index, 1,
        "0024 adds the (file_id, qualified_name) index the reference is_test lookup rides"
    );
    drop(conn);

    let db = codesage_storage::Database::open(&path).unwrap();
    let symbols = db.find_symbols("helper", None).unwrap();
    assert_eq!(symbols.len(), 1);
    assert!(!symbols[0].is_test);
    let refs = db.find_references("target", None).unwrap();
    assert_eq!(refs.len(), 1);
    assert!(!refs[0].is_test);
    let json = serde_json::to_value(&refs[0]).unwrap();
    assert!(json.get("is_test").is_none(), "false is omitted: {json}");
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

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .unwrap()
        .query_map([table], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn row_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

/// The staging statements of the 562c496 binary, which knows migration 0030
/// only: `clear_git_history_build`, `start_git_history_build`,
/// `stage_git_history_slice`, `set_git_history_slice_len`, and the reads of
/// `git_history_build` and `git_history_build_slices`.
fn run_562c496_staging(conn: &Connection) {
    conn.execute_batch(
        "DELETE FROM git_history_build_slices;
         DELETE FROM git_history_build_progress;
         DELETE FROM git_history_build;
         INSERT INTO git_history_build (id, target_sha, policy, cutoff, commits)
             VALUES (1, 'old-head', 'policy', 7, 'a');
         INSERT INTO git_history_build_progress (id, slice_len) VALUES (1, 2)
             ON CONFLICT(id) DO UPDATE SET slice_len = excluded.slice_len;
         INSERT INTO git_history_build_slices (start, len, output) VALUES (0, 1, 'out');",
    )
    .unwrap();
    conn.query_row(
        "SELECT target_sha, policy, cutoff, commits FROM git_history_build WHERE id = 1",
        [],
        |_| Ok(()),
    )
    .unwrap();
    conn.query_row(
        "SELECT slice_len FROM git_history_build_progress WHERE id = 1",
        [],
        |_| Ok(()),
    )
    .unwrap();
    conn.query_row(
        "SELECT COALESCE(SUM(len), 0) FROM git_history_build_slices",
        [],
        |_| Ok(()),
    )
    .unwrap();
    conn.prepare("SELECT start, len, output FROM git_history_build_slices ORDER BY start")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(|row| row.unwrap());
    conn.execute_batch(
        "DELETE FROM git_history_build_slices;
         DELETE FROM git_history_build_progress;
         DELETE FROM git_history_build;",
    )
    .unwrap();
}

/// A fresh index, one migrated by the 562c496 binary (0030 applied, with a
/// rebuild it saved), and a 0.49.0 index (neither migration recorded) all end
/// with 0031's tables, 0030's tables intact and emptied, and staging that
/// works for this binary and, after a rollback, for the 562c496 one.
#[test]
fn every_starting_state_reaches_0031_and_keeps_0030_usable() {
    use codesage_storage::Database;
    use codesage_storage::db::GitHistoryProgress;

    for state in ["fresh", "562c496", "0.49.0"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            if state != "fresh" {
                init_db(&conn).unwrap();
                conn.execute_batch(
                    "DROP TABLE git_history_build_v2_commits;
                     DROP TABLE git_history_build_v2_keys;
                     DROP TABLE git_history_build_v2_progress;
                     DROP TABLE git_history_build_v2;
                     DELETE FROM schema_migrations WHERE name = '0031_git_history_build_v2';",
                )
                .unwrap();
            }
            match state {
                "562c496" => conn
                    .execute_batch(
                        "INSERT INTO git_history_build VALUES (1, 'head', 'policy', 0, 'a');
                         INSERT INTO git_history_build_progress VALUES (1, 4);
                         INSERT INTO git_history_build_slices VALUES (0, 1, 'out');",
                    )
                    .unwrap(),
                "0.49.0" => conn
                    .execute_batch(
                        "DROP TABLE git_history_build_slices;
                         DROP TABLE git_history_build_progress;
                         DROP TABLE git_history_build;
                         DELETE FROM schema_migrations WHERE name = '0030_git_history_build';",
                    )
                    .unwrap(),
                _ => {}
            }
            init_db(&conn).unwrap();
            assert_eq!(
                table_columns(&conn, "git_history_build_v2"),
                [
                    "id",
                    "target_sha",
                    "policy",
                    "cutoff",
                    "commit_count",
                    "commits"
                ],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build_v2_progress"),
                [
                    "id",
                    "slice_len",
                    "cause",
                    "stored_sha",
                    "outcome",
                    "stalled",
                    "run_saved",
                    "updated_at",
                    "note",
                    "discarded_at",
                    "discarding_since",
                    "discard_cause",
                    "restarts"
                ],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build_v2_keys"),
                ["id", "env_key", "used_at"],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build_v2_commits"),
                ["sha", "key_id", "output"],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build"),
                ["id", "target_sha", "policy", "cutoff", "commits"],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build_progress"),
                ["id", "slice_len"],
                "{state}"
            );
            assert_eq!(
                table_columns(&conn, "git_history_build_slices"),
                ["start", "len", "output"],
                "{state}"
            );
            for table in [
                "git_history_build",
                "git_history_build_progress",
                "git_history_build_slices",
            ] {
                assert_eq!(
                    row_count(&conn, table),
                    0,
                    "{state}: {table} kept a stale rebuild"
                );
            }
            for name in ["0030_git_history_build", "0031_git_history_build_v2"] {
                let recorded: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                        [name],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(recorded, 1, "{state}: {name}");
            }
        }

        let db = Database::open(&path).unwrap();
        assert!(db.git_history_build().unwrap().is_none(), "{state}");
        let commits = vec!["a".repeat(40), "b".repeat(40)];
        let progress = GitHistoryProgress {
            slice_len: 1,
            cause: "first_build",
            stored_sha: None,
            outcome: "progressing",
            stalled: None,
            run_saved: 1,
        };
        db.execute_batch(|db| {
            db.start_git_history_build("head", "policy", 0, &commits, &progress)?;
            db.stage_git_history_commits(&[(&commits[0], "out")], &progress)
        })
        .unwrap();
        drop(db);

        // Roll back to the 562c496 binary: its staging still works, and the
        // two binaries never see each other's rows.
        let conn = Connection::open(&path).unwrap();
        run_562c496_staging(&conn);
        conn.execute_batch(
            "INSERT INTO git_history_build VALUES (1, 'old-head', 'policy', 7, 'c');",
        )
        .unwrap();
        drop(conn);
        let db = Database::open(&path).unwrap();
        let status = db.git_history_build_status().unwrap().unwrap();
        assert_eq!(
            (status.target_sha.as_str(), status.staged, status.total),
            ("head", 1, 2),
            "{state}"
        );
        db.clear_git_history_build().unwrap();
        drop(db);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            row_count(&conn, "git_history_build"),
            1,
            "{state}: this binary cleared the older binary's staging"
        );
    }
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

/// A newer binary may have reshaped tables SCHEMA indexes; the refusal must
/// come before SCHEMA or any repair touches them.
#[test]
fn breaking_marker_refuses_before_schema_touches_a_reshaped_table() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    let marker = format!("{BREAKING_MIGRATION_PREFIX}9999_semantic_files_rev");
    conn.execute_batch(
        "DROP TABLE semantic_files;
         CREATE TABLE semantic_files(chunk_table TEXT NOT NULL, rev TEXT NOT NULL,
             content_hash TEXT NOT NULL, PRIMARY KEY (chunk_table, rev));
         INSERT INTO semantic_files(chunk_table, rev, content_hash) VALUES ('t', 'r', 'h');",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES (?1)",
        rusqlite::params![marker],
    )
    .unwrap();

    let msg = init_db(&conn)
        .expect_err("breaking marker must refuse open")
        .to_string();
    assert!(msg.contains(&marker), "refusal, not a raw SQL error: {msg}");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM semantic_files WHERE rev = 'r'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the newer binary's table must be left alone");
}

/// Refusing a newer index must not first apply this binary's own pending
/// migrations to it.
#[test]
fn breaking_marker_refuses_before_known_migrations_run() {
    let conn = Connection::open_in_memory().unwrap();
    init_db(&conn).expect("first init_db");
    let marker = format!("{BREAKING_MIGRATION_PREFIX}9999_drop_everything");
    conn.execute_batch("DELETE FROM schema_migrations WHERE name = '0024_is_test'")
        .unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES (?1)",
        rusqlite::params![marker],
    )
    .unwrap();

    init_db(&conn).expect_err("breaking marker must refuse open");
    let stamped: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = '0024_is_test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stamped, 0, "no known migration may run before the refusal");
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
        is_test: false,
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
        visibility: None,
        is_test: false,
        signature: None,
        overloaded: false,
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
        lazy: false,
        to: None,
        from_line: None,
        is_test: false,
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

/// Commands open the index through the migrating path, and searches through
/// the model path on top of it. On a current index both must only read: a
/// statement that takes the write lock, even one that changes nothing, waits
/// out the busy timeout behind an indexing transaction and then fails the
/// command.
#[test]
fn opening_a_current_index_succeeds_while_another_connection_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    drop(codesage_storage::Database::open(&path).unwrap());
    drop(codesage_storage::Database::open_for_model(&path, "probe-model", 8).unwrap());

    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE schema_migrations SET applied_at = applied_at;",
        )
        .unwrap();

    codesage_storage::Database::open(&path)
        .expect("a current index opens while another connection holds the write lock");
    codesage_storage::Database::open_for_model_existing(&path, "probe-model", 8)
        .expect("a recorded model opens while another connection holds the write lock");
    writer.execute_batch("ROLLBACK").unwrap();
}
