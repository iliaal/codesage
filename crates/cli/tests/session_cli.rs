use std::{
    path::Path,
    process::{Command, Output},
};

#[test]
fn session_end_failure_prints_report_before_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_acyclic_php(root);

    run_ok(codesage(root).arg("init"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );
    run_ok(
        codesage(root)
            .arg("session-start")
            .arg("--session-id")
            .arg("flush-check"),
    );

    write_cyclic_php(root);
    let _ = std::fs::remove_file(root.join("C.php"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );

    let out = codesage(root)
        .arg("session-end")
        .arg("--session-id")
        .arg("flush-check")
        .output()
        .expect("run session-end");
    assert!(
        !out.status.success(),
        "session-end should fail when a new cycle is introduced"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Session flush-check: FAIL") && stdout.contains("NEW cycles"),
        "session-end failure report was not flushed to stdout: {stdout:?}"
    );
}
#[test]
fn overview_does_not_migrate_missing_semantic_schema() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_ok(codesage(root).arg("init"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );
    let db = codesage_storage::Database::open(&root.join(".codesage/index.db")).unwrap();
    db.execute_raw_for_tests("DROP TABLE semantic_files")
        .unwrap();
    drop(db);

    let out = codesage(root)
        .arg("overview")
        .arg("--json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("semantic_files"),
        "overview stderr: {stderr}"
    );
}

#[test]
fn overview_migrates_safe_structural_columns_in_a_real_pre_0024_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_ok(codesage(root).arg("init"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );

    let db_path = root.join(".codesage/index.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "DROP INDEX idx_symbols_file_qualified;
         ALTER TABLE files DROP COLUMN is_test;
         ALTER TABLE symbols DROP COLUMN is_test;
         DELETE FROM schema_migrations WHERE name = '0024_is_test';",
    )
    .unwrap();
    drop(db);

    let out = codesage(root)
        .arg("overview")
        .arg("--json")
        .output()
        .expect("run overview on pre-0024 index");
    assert!(
        out.status.success(),
        "overview must apply safe structural migrations: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let migrated = rusqlite::Connection::open(db_path).unwrap();
    for table in ["files", "symbols"] {
        let has_is_test: i64 = migrated
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = 'is_test'",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_is_test, 1, "overview did not migrate {table}.is_test");
    }
}

#[test]
fn overview_preserves_legacy_semantic_schema_instead_of_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_ok(codesage(root).arg("init"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );
    let db_path = root.join(".codesage/index.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "DELETE FROM schema_migrations WHERE name = '0004_semantic_files_chunk_table';
         ALTER TABLE semantic_files RENAME COLUMN path TO legacy_path;",
    )
    .unwrap();
    drop(db);

    let out = codesage(root)
        .arg("overview")
        .arg("--json")
        .output()
        .expect("run overview on legacy semantic schema");
    assert!(
        !out.status.success(),
        "legacy semantic schema must be reported"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("semantic_files"),
        "overview must name the damaged semantic schema: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let preserved = rusqlite::Connection::open(db_path).unwrap();
    let columns: i64 = preserved
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('semantic_files') WHERE name = 'legacy_path'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        columns, 1,
        "overview must not rewrite legacy semantic state"
    );
}

fn codesage(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codesage"));
    cmd.current_dir(root);
    cmd
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("run command");
    assert!(
        out.status.success(),
        "command failed: status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write_acyclic_php(root: &Path) {
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Mid;\nclass Top { public function x(Mid $m) { return $m->y(null); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Leaf;\nclass Mid { public function y(Leaf $l) { return $l->z(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Leaf { public function z() { return 1; } }\n",
    )
    .unwrap();
}

fn write_cyclic_php(root: &Path) {
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
}
