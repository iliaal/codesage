#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use codesage_parser::discover::{content_hash, discover_files_report_with_cache};

#[test]
fn unchanged_files_reuse_hashes_but_full_discovery_reads_bytes() {
    let root = tempfile::tempdir().unwrap();
    let body = "fn stable() {}\n".repeat(100_000);
    fs::write(root.path().join("stable.rs"), &body).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let first = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    assert_eq!(first.bytes_hashed, body.len() as u64);
    let second = discover_files_report_with_cache(root.path(), &[], &first.hash_cache).unwrap();
    assert_eq!(second.hashes_reused, 1);
    assert_eq!(second.bytes_hashed, 0);
    assert_eq!(second.files[0].content_hash, content_hash(body.as_bytes()));
    let full = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    assert_eq!(full.hashes_reused, 0);
    assert_eq!(full.bytes_hashed, body.len() as u64);
}

#[test]
fn racy_mtime_and_ctime_and_clock_rollback_rehash() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    let first = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    for clock in [
        "mtime",
        "ctime",
        "mtime_within_second",
        "ctime_within_second",
        "future",
    ] {
        let mut cache = first.hash_cache.clone();
        let entry = cache.get_mut("a.rs").unwrap();
        entry.hashed_at_ns = match clock {
            "mtime" => entry.stat.mtime_ns,
            "ctime" => entry.stat.ctime_ns,
            "mtime_within_second" => entry.stat.mtime_ns + 1,
            "ctime_within_second" => entry.stat.ctime_ns + 1,
            _ => i64::MAX,
        };
        entry.content_hash = "forbidden-stale-hash".into();
        let actual = discover_files_report_with_cache(root.path(), &[], &cache).unwrap();
        assert_eq!(actual.hashes_reused, 0, "{clock}");
        assert_eq!(actual.files[0].content_hash, content_hash(b"fn a() {}\n"));
    }
}

#[test]
fn same_size_rewrite_with_restored_mtime_changes_hash() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a.rs");
    fs::write(&path, "fn a() {}\n").unwrap();
    let old_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let first = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    fs::write(&path, "fn b() {}\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_mtime))
        .unwrap();
    let second = discover_files_report_with_cache(root.path(), &[], &first.hash_cache).unwrap();
    assert_eq!(second.hashes_reused, 0);
    assert_eq!(second.files[0].content_hash, content_hash(b"fn b() {}\n"));
}

#[test]
fn cached_files_still_obey_exclusions_deletion_and_header_dialect() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.h"), "void a();\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let first = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    fs::write(root.path().join("dialect.cpp"), "void b() {}\n").unwrap();
    let second = discover_files_report_with_cache(root.path(), &[], &first.hash_cache).unwrap();
    assert_eq!(second.hashes_reused, 1);
    assert_eq!(
        second
            .files
            .iter()
            .find(|f| f.path == "a.h")
            .unwrap()
            .language,
        codesage_protocol::Language::Cpp
    );
    let excluded =
        discover_files_report_with_cache(root.path(), &["a.h".into()], &second.hash_cache).unwrap();
    assert!(!excluded.files.iter().any(|f| f.path == "a.h"));
    assert!(!excluded.hash_cache.contains_key("a.h"));
    fs::remove_file(root.path().join("a.h")).unwrap();
    let deleted = discover_files_report_with_cache(root.path(), &[], &second.hash_cache).unwrap();
    assert!(!deleted.files.iter().any(|f| f.path == "a.h"));
}

#[test]
#[ignore = "synthetic discovery throughput measurement"]
fn measure_cached_discovery() {
    let root = tempfile::tempdir().unwrap();
    let body = "fn stable() {}\n".repeat(70_000);
    for index in 0..128 {
        fs::write(root.path().join(format!("file{index}.rs")), &body).unwrap();
    }
    std::thread::sleep(Duration::from_millis(1100));
    let start = std::time::Instant::now();
    let full = discover_files_report_with_cache(root.path(), &[], &HashMap::new()).unwrap();
    let full_time = start.elapsed();
    let start = std::time::Instant::now();
    let cached = discover_files_report_with_cache(root.path(), &[], &full.hash_cache).unwrap();
    let cached_time = start.elapsed();
    assert_eq!(cached.hashes_reused, 128);
    assert_eq!(cached.bytes_hashed, 0);
    assert_eq!(full.bytes_hashed, body.len() as u64 * 128);
    assert_eq!(full.files, cached.files);
    eprintln!(
        "synthetic 128-file discovery: bytes_hashed={}, uncached={full_time:?}, cached={cached_time:?}, cached_bytes_hashed={}",
        full.bytes_hashed, cached.bytes_hashed
    );
}
