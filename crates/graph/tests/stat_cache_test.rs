#![cfg(unix)]

use codesage_graph::{full_index, incremental_index};
use codesage_parser::discover::{content_hash, discover_files_report_with_cache};
use codesage_storage::Database;
use std::{fs, time::Duration};

#[test]
fn persisted_cache_survives_reopen_and_full_index_bypasses_it() {
    let root = tempfile::tempdir().unwrap();
    let database = tempfile::tempdir().unwrap();
    let db_path = database.path().join("index.db");
    let body = "fn original() {}\n";
    fs::write(root.path().join("a.rs"), body).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let db = Database::open(&db_path).unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    drop(db);
    let db = Database::open(&db_path).unwrap();
    let cache = db.file_hash_cache().unwrap();
    let report = discover_files_report_with_cache(root.path(), &[], &cache).unwrap();
    assert_eq!(report.hashes_reused, 1);
    assert_eq!(
        incremental_index(root.path(), &db, &[], false)
            .unwrap()
            .files_skipped,
        1
    );
    let mut stale_cache = cache;
    stale_cache.get_mut("a.rs").unwrap().content_hash = "stale-cache-marker".into();
    db.replace_file_hash_cache(&stale_cache).unwrap();
    assert_eq!(
        discover_files_report_with_cache(root.path(), &[], &stale_cache)
            .unwrap()
            .files[0]
            .content_hash,
        "stale-cache-marker"
    );
    assert_eq!(
        full_index(root.path(), &db, &[], false)
            .unwrap()
            .files_indexed,
        1
    );
    assert_eq!(
        db.get_file_hash("a.rs").unwrap().as_deref(),
        Some(content_hash(body.as_bytes()).as_str())
    );
    assert_eq!(
        db.file_hash_cache().unwrap()["a.rs"].content_hash,
        content_hash(body.as_bytes())
    );
    fs::remove_file(root.path().join("a.rs")).unwrap();
    assert_eq!(
        incremental_index(root.path(), &db, &[], false)
            .unwrap()
            .files_removed,
        1
    );
    assert!(db.file_hash_cache().unwrap().is_empty());
}

#[test]
fn cached_unreadable_file_reports_failure_and_preserves_symbols() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a.rs");
    fs::write(&path, "fn durable() {}\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(
        discover_files_report_with_cache(root.path(), &[], &db.file_hash_cache().unwrap())
            .unwrap()
            .hashes_reused,
        1
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    let read_result = fs::read(&path);
    if read_result.is_ok() {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        eprintln!("permission regression requires an unprivileged user");
        return;
    }
    let stats = incremental_index(root.path(), &db, &[], false).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(stats.files_failed, 1);
    assert_eq!(stats.files_removed, 0);
    assert_eq!(db.symbols_for_file("a.rs").unwrap()[0].name, "durable");
    assert!(db.file_hash_cache().unwrap().is_empty());
}

#[test]
#[ignore = "synthetic no-op incremental indexing throughput measurement"]
fn measure_noop_incremental_index() {
    let root = tempfile::tempdir().unwrap();
    let database = tempfile::tempdir().unwrap();
    let db = Database::open(&database.path().join("index.db")).unwrap();
    let body = format!("/*{}*/\nfn stable() {{}}\n", "x".repeat(1_000_000));
    for index in 0..128 {
        fs::write(root.path().join(format!("file{index}.rs")), &body).unwrap();
    }
    std::thread::sleep(Duration::from_millis(1100));
    full_index(root.path(), &db, &[], false).unwrap();
    db.replace_file_hash_cache(&Default::default()).unwrap();
    let start = std::time::Instant::now();
    let uncached = incremental_index(root.path(), &db, &[], false).unwrap();
    let uncached_time = start.elapsed();
    let start = std::time::Instant::now();
    let cached = incremental_index(root.path(), &db, &[], false).unwrap();
    let cached_time = start.elapsed();
    assert_eq!(uncached.files_indexed, 0);
    assert_eq!(uncached.files_skipped, 128);
    assert_eq!(cached.files_indexed, 0);
    assert_eq!(cached.files_skipped, 128);
    eprintln!(
        "synthetic 128-file incremental index: source_bytes={}, uncached={uncached_time:?}, cached={cached_time:?}",
        body.len() * 128
    );
}
