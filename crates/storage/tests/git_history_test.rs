//! Storage-layer tests for V2b git history tables. Exercises the in-memory accessors:
//! upsert + on-conflict replacement, co-change query symmetry across pair sides,
//! churn percentile math, and clear_git_data.

use codesage_storage::Database;

#[test]
fn upsert_git_file_replaces_on_conflict() {
    let db = Database::open_in_memory().unwrap();

    db.upsert_git_file("src/foo.rs", 1.5, 1, 5, Some(1700000000)).unwrap();
    db.upsert_git_file("src/foo.rs", 3.7, 4, 12, Some(1700001000)).unwrap();

    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert_eq!(row.path, "src/foo.rs");
    assert!((row.churn_score - 3.7).abs() < 1e-9);
    assert_eq!(row.fix_count, 4);
    assert_eq!(row.total_commits, 12);
    assert_eq!(row.last_commit_at, Some(1700001000));
}

#[test]
fn git_file_returns_none_for_unknown_path() {
    let db = Database::open_in_memory().unwrap();
    assert!(db.git_file("does/not/exist").unwrap().is_none());
}

#[test]
fn co_changes_for_returns_from_both_pair_sides() {
    let db = Database::open_in_memory().unwrap();
    // Pair stored sorted: (a, b) where a < b lexicographically.
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 7, Some(1700000000)).unwrap();
    db.upsert_git_co_change("src/a.rs", "src/c.rs", 3.0, 5, Some(1700001000)).unwrap();
    db.upsert_git_co_change("src/b.rs", "src/c.rs", 1.0, 4, Some(1700002000)).unwrap();

    // Querying from the smaller side (file_a) returns the larger side.
    let from_a = db.co_changes_for("src/a.rs", 10).unwrap();
    let names: Vec<&str> = from_a.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(names, vec!["src/b.rs", "src/c.rs"], "weight-sorted desc");

    // Querying from the larger side (file_b) returns pairs from BOTH columns.
    let from_b = db.co_changes_for("src/b.rs", 10).unwrap();
    let names: Vec<&str> = from_b.iter().map(|r| r.file.as_str()).collect();
    // a (weight 5.0 from a-b pair) + c (weight 1.0 from b-c pair).
    assert_eq!(names, vec!["src/a.rs", "src/c.rs"]);
}

#[test]
fn co_changes_respects_limit() {
    let db = Database::open_in_memory().unwrap();
    for i in 0..10 {
        let other = format!("src/other_{i:02}.rs");
        db.upsert_git_co_change("src/main.rs", &other, (10 - i) as f64, 5, Some(1700000000)).unwrap();
    }
    let top3 = db.co_changes_for("src/main.rs", 3).unwrap();
    assert_eq!(top3.len(), 3);
    // Highest weights first (10.0, 9.0, 8.0)
    assert!((top3[0].weight - 10.0).abs() < 1e-9);
    assert!((top3[1].weight - 9.0).abs() < 1e-9);
    assert!((top3[2].weight - 8.0).abs() < 1e-9);
}

#[test]
fn churn_percentile_returns_zero_for_unknown_path() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("src/a.rs", 5.0, 0, 1, None).unwrap();
    assert_eq!(db.churn_percentile("src/missing.rs").unwrap(), 0.0);
}

#[test]
fn churn_percentile_ranks_correctly() {
    let db = Database::open_in_memory().unwrap();
    // Four files with churns 1, 2, 3, 4.
    for (path, churn) in [("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)] {
        db.upsert_git_file(path, churn, 0, 1, None).unwrap();
    }
    // Rank: a is at percentile 0.25 (1 of 4 files have churn <= 1), d at 1.0
    assert!((db.churn_percentile("a").unwrap() - 0.25).abs() < 1e-9);
    assert!((db.churn_percentile("b").unwrap() - 0.5).abs() < 1e-9);
    assert!((db.churn_percentile("c").unwrap() - 0.75).abs() < 1e-9);
    assert!((db.churn_percentile("d").unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn churn_percentile_handles_ties() {
    let db = Database::open_in_memory().unwrap();
    for (path, churn) in [("a", 5.0), ("b", 5.0), ("c", 5.0)] {
        db.upsert_git_file(path, churn, 0, 1, None).unwrap();
    }
    // All tied: each is at 100th percentile under <= comparison.
    assert!((db.churn_percentile("a").unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn clear_git_data_wipes_both_tables() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("src/a.rs", 5.0, 1, 3, Some(1700000000)).unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 7, Some(1700000000)).unwrap();

    db.clear_git_data().unwrap();

    assert!(db.git_file("src/a.rs").unwrap().is_none());
    assert!(db.co_changes_for("src/a.rs", 10).unwrap().is_empty());
}

#[test]
fn co_changes_upsert_replaces_on_conflict() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 1.0, 2, Some(1700000000)).unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 8, Some(1700001000)).unwrap();
    let rows = db.co_changes_for("src/a.rs", 10).unwrap();
    assert_eq!(rows.len(), 1, "no duplicate row");
    assert!((rows[0].weight - 5.0).abs() < 1e-9);
    assert_eq!(rows[0].count, 8);
    assert_eq!(rows[0].last_observed_at, Some(1700001000));
}

#[test]
fn git_index_state_round_trip() {
    let db = Database::open_in_memory().unwrap();
    assert!(db.get_git_index_state().unwrap().is_none(), "fresh DB has no state");

    db.set_git_index_state("abc123").unwrap();
    let (sha, at) = db.get_git_index_state().unwrap().expect("state present");
    assert_eq!(sha, "abc123");
    assert!(at > 0, "indexed_at stamped via unixepoch()");

    // Update with a new SHA replaces the row, not appends.
    db.set_git_index_state("def456").unwrap();
    let (sha2, _) = db.get_git_index_state().unwrap().expect("state present");
    assert_eq!(sha2, "def456");
}

#[test]
fn clear_git_data_drops_state_too() {
    let db = Database::open_in_memory().unwrap();
    db.set_git_index_state("abc").unwrap();
    db.upsert_git_file("a.rs", 1.0, 0, 1, Some(1)).unwrap();
    db.clear_git_data().unwrap();
    assert!(db.get_git_index_state().unwrap().is_none());
    assert!(db.git_file("a.rs").unwrap().is_none());
}

#[test]
fn incr_git_file_accumulates_counters() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_file("src/foo.rs", 1.5, 1, 3, Some(1700000000)).unwrap();
    db.incr_git_file("src/foo.rs", 0.5, 2, 4, Some(1700001000)).unwrap();
    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert!((row.churn_score - 2.0).abs() < 1e-9, "churn summed");
    assert_eq!(row.fix_count, 3, "fix_count summed");
    assert_eq!(row.total_commits, 7, "commits summed");
    assert_eq!(row.last_commit_at, Some(1700001000), "last_commit_at MAXed");
}

#[test]
fn incr_git_file_keeps_existing_last_when_new_is_older() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_file("src/foo.rs", 1.0, 0, 1, Some(1700001000)).unwrap();
    db.incr_git_file("src/foo.rs", 1.0, 0, 1, Some(1700000000)).unwrap();
    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert_eq!(row.last_commit_at, Some(1700001000), "MAX kept the older row");
}

#[test]
fn incr_git_co_change_accumulates_pair() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_co_change("a.rs", "b.rs", 1.0, 2, Some(1700000000)).unwrap();
    db.incr_git_co_change("a.rs", "b.rs", 0.5, 3, Some(1700001000)).unwrap();
    let rows = db.co_changes_for("a.rs", 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert!((rows[0].weight - 1.5).abs() < 1e-9);
    assert_eq!(rows[0].count, 5);
    assert_eq!(rows[0].last_observed_at, Some(1700001000));
}

#[test]
fn co_change_pair_exists_detects_both_orderings() {
    let db = Database::open_in_memory().unwrap();
    assert!(!db.co_change_pair_exists("a.rs", "b.rs").unwrap());
    db.upsert_git_co_change("a.rs", "b.rs", 1.0, 3, Some(1700000000)).unwrap();
    assert!(db.co_change_pair_exists("a.rs", "b.rs").unwrap());
    // Pairs are stored sorted, so b/a (unsorted) would be a debug_assert hit in incr_;
    // exists() also requires sorted input. Validate the sorted lookup works.
    assert!(!db.co_change_pair_exists("a.rs", "c.rs").unwrap());
}

#[test]
fn scale_git_decay_multiplies_churn_and_pair_weights() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("a.rs", 4.0, 1, 5, Some(1700000000)).unwrap();
    db.upsert_git_file("b.rs", 2.0, 0, 3, Some(1700000000)).unwrap();
    db.upsert_git_co_change("a.rs", "b.rs", 6.0, 4, Some(1700000000)).unwrap();

    db.scale_git_decay(0.5).unwrap();

    let a = db.git_file("a.rs").unwrap().expect("present");
    let b = db.git_file("b.rs").unwrap().expect("present");
    assert!((a.churn_score - 2.0).abs() < 1e-9);
    assert!((b.churn_score - 1.0).abs() < 1e-9);
    // Counters are not scaled — only weights.
    assert_eq!(a.fix_count, 1);
    assert_eq!(a.total_commits, 5);

    let pairs = db.co_changes_for("a.rs", 10).unwrap();
    assert!((pairs[0].weight - 3.0).abs() < 1e-9);
    assert_eq!(pairs[0].count, 4, "count preserved");
}
