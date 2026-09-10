use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};
use codesage_storage::Database;
use std::time::{Duration, Instant};

const EXPENSIVE_SQL: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n";

#[cfg(unix)]
#[test]
fn opened_connection_detects_replacement_even_when_path_returns_to_original_inode() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let saved = directory.path().join("original.db");
    let replacement = directory.path().join("replacement.db");
    for database_path in [&path, &replacement] {
        let db = Database::open(database_path).unwrap();
        db.execute_raw_for_tests("PRAGMA journal_mode=DELETE")
            .unwrap();
    }
    std::fs::rename(&path, &saved).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    let db = Database::open_read_only_strict(&path).unwrap();
    assert!(db.path_still_matches_open_file(&path).unwrap());
    std::fs::rename(&path, &replacement).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    assert!(!db.path_still_matches_open_file(&path).unwrap());
    assert!(
        Database::open_read_only_strict(&path)
            .unwrap()
            .path_still_matches_open_file(&path)
            .unwrap()
    );
}

#[test]
fn in_memory_connection_does_not_attest_a_file_identity() {
    assert!(
        !Database::open_in_memory()
            .unwrap()
            .path_still_matches_open_file(std::path::Path::new(":memory:"))
            .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn opened_connection_rejects_retargeted_ancestor_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original");
    let replacement = directory.path().join("replacement");
    for root in [&original, &replacement] {
        std::fs::create_dir(root).unwrap();
        let db = Database::open(&root.join("index.db")).unwrap();
        db.execute_raw_for_tests("PRAGMA journal_mode=DELETE")
            .unwrap();
    }
    let alias = directory.path().join("alias");
    let saved_alias = directory.path().join("saved-alias");
    std::os::unix::fs::symlink(&replacement, &alias).unwrap();
    let expected = alias.join("index.db");
    let db = Database::open_read_only_strict(&expected).unwrap();
    assert!(db.path_still_matches_open_file(&expected).unwrap());
    std::fs::rename(&alias, &saved_alias).unwrap();
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    assert!(!db.path_still_matches_open_file(&expected).unwrap());
    assert!(
        db.path_still_matches_open_file(&replacement.join("index.db"))
            .unwrap()
    );
}

#[test]
fn cancellation_between_statements_remains_effective() {
    let control = WorkControl::new(None);
    let _scope = control.enter();
    let db = Database::open_in_memory().unwrap();
    db.file_count().unwrap();
    control.cancel(StopReason::ClientCancelled);
    let started = Instant::now();
    let error = db.execute_raw_for_tests(EXPENSIVE_SQL).unwrap_err();
    assert!(error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted)));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn deadline_interrupts_active_sql() {
    let control = WorkControl::new(None);
    let _scope = control.enter();
    let db = Database::open_in_memory().unwrap();
    let cancel = control.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        cancel.cancel(StopReason::DeadlineExceeded);
    });
    let started = Instant::now();
    assert!(db.execute_raw_for_tests(EXPENSIVE_SQL).is_err());
    thread.join().unwrap();
    assert_eq!(control.reason(), Some(StopReason::DeadlineExceeded));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn progress_callback_discovers_deadline_without_timer_thread() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    drop(Database::open(&path).unwrap());
    let control = WorkControl::new(Some(Instant::now() + Duration::from_millis(250)));
    let _scope = control.enter();
    let db = Database::open_read_only_strict(&path).unwrap();
    let started = Instant::now();
    assert!(db.execute_raw_for_tests(EXPENSIVE_SQL).is_err());
    assert_eq!(control.reason(), Some(StopReason::DeadlineExceeded));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn cancellation_leaves_other_connection_usable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    drop(Database::open(&path).unwrap());
    let stopped = WorkControl::new(None);
    let first = {
        let _scope = stopped.enter();
        Database::open_read_only_strict(&path).unwrap()
    };
    let unaffected = WorkControl::new(None);
    let second = {
        let _scope = unaffected.enter();
        Database::open_read_only_strict(&path).unwrap()
    };
    stopped.cancel(StopReason::ClientCancelled);
    assert!(first.execute_raw_for_tests(EXPENSIVE_SQL).is_err());
    second.execute_raw_for_tests("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n").unwrap();
    assert_eq!(unaffected.reason(), None);
}

#[test]
fn busy_read_only_open_never_falls_back_to_immutable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let writer = Database::open(&path).unwrap();
    writer
        .execute_raw_for_tests("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    let control = WorkControl::new(None);
    let _scope = control.enter();
    for strict in [false, true] {
        let started = Instant::now();
        let result = if strict {
            Database::open_read_only_strict(&path)
        } else {
            Database::open_read_only(&path)
        };
        let error = result
            .err()
            .expect("locked database must not open as immutable");
        assert!(error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    writer.execute_raw_for_tests("ROLLBACK").unwrap();
}

#[test]
fn expired_control_refuses_constructor_before_sql() {
    let control = WorkControl::new(Some(Instant::now()));
    let _scope = control.enter();
    let error = Database::open_in_memory().err().unwrap();
    assert_eq!(
        error.downcast_ref::<WorkStopped>().unwrap().reason,
        StopReason::DeadlineExceeded
    );
}

#[test]
fn controlled_busy_wait_is_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let writer = Database::open(&path).unwrap();
    let control = WorkControl::new(None);
    let _scope = control.enter();
    let db = Database::open_existing_read(&path).unwrap();
    writer.execute_raw_for_tests("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let error = db.execute_raw_for_tests("BEGIN IMMEDIATE").unwrap_err();
    assert!(error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy)));
    assert!(started.elapsed() < Duration::from_secs(1));
    writer.execute_raw_for_tests("ROLLBACK").unwrap();
}

#[test]
fn read_snapshot_pins_rows_until_guard_drops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let writer = Database::open(&path).unwrap();
    let reader = Database::open_existing_read(&path).unwrap();
    let before = reader.data_version().unwrap();
    let snapshot = reader.read_snapshot().unwrap();
    writer
        .execute_raw_for_tests(
            "INSERT INTO files(path, language, content_hash) VALUES('a.rs','rust','a')",
        )
        .unwrap();
    assert_eq!(reader.file_count().unwrap(), 0);
    drop(snapshot);
    assert_eq!(reader.file_count().unwrap(), 1);
    assert_ne!(reader.data_version().unwrap(), before);
}
