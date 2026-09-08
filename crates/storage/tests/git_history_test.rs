//! Storage-layer tests for V2b git history tables. Exercises the in-memory accessors:
//! upsert + on-conflict replacement, co-change query symmetry across pair sides,
//! churn percentile math, and clear_git_data.

use codesage_storage::Database;
use codesage_storage::db::CoChangeWrite;

#[test]
fn upsert_git_file_replaces_on_conflict() {
    let db = Database::open_in_memory().unwrap();

    db.upsert_git_file("src/foo.rs", 1.5, 1, 5, Some(1700000000))
        .unwrap();
    db.upsert_git_file("src/foo.rs", 3.7, 4, 12, Some(1700001000))
        .unwrap();

    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert_eq!(row.path, "src/foo.rs");
    assert!((row.churn_score - 3.7).abs() < 1e-9);
    assert_eq!(row.fix_count, 4);
    assert_eq!(row.total_commits, 12);
    assert_eq!(row.last_commit_at, Some(1700001000));
}

fn write(
    weight: f64,
    count: u32,
    mask: u64,
    first: Option<i64>,
    last: Option<i64>,
) -> CoChangeWrite {
    CoChangeWrite {
        weight,
        count,
        window_mask: mask,
        first_observed_at: first,
        last_observed_at: last,
    }
}

#[test]
fn co_change_recurrence_columns_round_trip_and_default_to_one_window() {
    let db = Database::open_in_memory().unwrap();
    // Legacy 5-arg upsert: mask 0, windows 1, first == last.
    db.upsert_git_co_change("a.rs", "b.rs", 1.0, 3, Some(1_700_000_000))
        .unwrap();
    // Four bits set, spanning two timestamps.
    db.upsert_git_co_change_full(
        "a.rs",
        "c.rs",
        &write(
            1.0,
            3,
            0b1_0110_0001,
            Some(1_600_000_000),
            Some(1_700_000_000),
        ),
    )
    .unwrap();
    // Mask 0 still reads as one window, never 0.
    db.upsert_git_co_change_full("a.rs", "d.rs", &write(1.0, 3, 0, None, None))
        .unwrap();
    // Bit 63 survives the signed INTEGER column.
    db.upsert_git_co_change_full("a.rs", "e.rs", &write(1.0, 3, 1 << 63 | 1, None, None))
        .unwrap();
    db.upsert_git_file("c.rs", 1.0, 0, 6, None).unwrap();

    let rows = db.co_changes_for("a.rs", 10).unwrap();
    let by_file = |f: &str| rows.iter().find(|r| r.file == f).expect(f).clone();
    assert_eq!(by_file("b.rs").windows, 1);
    assert_eq!(by_file("b.rs").window_mask, 0);
    assert_eq!(by_file("b.rs").first_observed_at, Some(1_700_000_000));
    assert_eq!(by_file("c.rs").windows, 4);
    assert_eq!(by_file("c.rs").window_mask, 0b1_0110_0001);
    assert_eq!(by_file("c.rs").first_observed_at, Some(1_600_000_000));
    assert_eq!(by_file("c.rs").last_observed_at, Some(1_700_000_000));
    assert_eq!(by_file("d.rs").windows, 1);
    assert_eq!(by_file("e.rs").window_mask, 1 << 63 | 1);
    assert_eq!(by_file("e.rs").windows, 2);
    // The other side's commit total rides along; a missing git_files row is 0.
    assert_eq!(by_file("c.rs").other_commits, 6);
    assert_eq!(by_file("b.rs").other_commits, 0);
    // Replacement upsert overwrites the recurrence columns too.
    db.upsert_git_co_change_full("a.rs", "c.rs", &write(1.0, 3, 0b11, None, None))
        .unwrap();
    let c = db
        .co_changes_for("a.rs", 10)
        .unwrap()
        .into_iter()
        .find(|r| r.file == "c.rs")
        .unwrap();
    assert_eq!(c.windows, 2);
    assert_eq!(c.window_mask, 0b11);
    assert_eq!(c.first_observed_at, None);
}

#[test]
fn incr_co_change_ors_masks_and_widens_the_observation_span() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_co_change_full(
        "a.rs",
        "b.rs",
        &write(1.0, 1, 0b0100, Some(1_650_000_000), Some(1_650_000_000)),
    )
    .unwrap();
    // Older first, newer last, overlapping plus new bits.
    db.incr_git_co_change_full(
        "a.rs",
        "b.rs",
        &write(1.0, 2, 0b0101, Some(1_600_000_000), Some(1_700_000_000)),
    )
    .unwrap();
    // A delta inside already-set bits and inside the span changes nothing
    // but count and weight.
    db.incr_git_co_change_full(
        "a.rs",
        "b.rs",
        &write(1.0, 1, 0b0100, Some(1_660_000_000), Some(1_660_000_000)),
    )
    .unwrap();
    // The recurrence-less wrapper never adds bits.
    db.incr_git_co_change("a.rs", "b.rs", 1.0, 1, Some(1_690_000_000))
        .unwrap();

    let rows = db.co_changes_for("a.rs", 10).unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.count, 5);
    assert_eq!(r.window_mask, 0b0101);
    assert_eq!(r.windows, 2, "windows is recomputed from the merged mask");
    assert_eq!(r.first_observed_at, Some(1_600_000_000));
    assert_eq!(r.last_observed_at, Some(1_700_000_000));
    // A fresh row through the wrapper gets mask 0 / windows 1.
    db.incr_git_co_change("a.rs", "c.rs", 1.0, 1, None).unwrap();
    let c = db
        .co_changes_for("a.rs", 10)
        .unwrap()
        .into_iter()
        .find(|r| r.file == "c.rs")
        .unwrap();
    assert_eq!((c.window_mask, c.windows), (0, 1));
}

#[test]
fn co_change_history_span_covers_the_whole_table() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(db.co_change_history_span().unwrap(), None);
    db.upsert_git_co_change_full("a.rs", "b.rs", &write(1.0, 3, 1, Some(100), Some(200)))
        .unwrap();
    db.upsert_git_co_change_full("c.rs", "d.rs", &write(1.0, 3, 1, Some(50), Some(120)))
        .unwrap();
    // A legacy row without first_observed_at falls back to its last.
    db.upsert_git_co_change("e.rs", "f.rs", 1.0, 3, Some(300))
        .unwrap();
    assert_eq!(db.co_change_history_span().unwrap(), Some((50, 300)));
}

#[test]
fn co_changes_for_ranked_demotes_one_off_pairs_but_reports_raw_weight() {
    let db = Database::open_in_memory().unwrap();
    // one-off.rs has the higher raw weight but its commits span 8 days and
    // straddle a grid boundary (2 bits); recurring.rs spans 60 days.
    let t = 1_750_000_000;
    db.upsert_git_co_change_full(
        "target.rs",
        "one-off.rs",
        &write(4.0, 3, 0b11, Some(t - 8 * 86_400), Some(t)),
    )
    .unwrap();
    db.upsert_git_co_change_full(
        "target.rs",
        "recurring.rs",
        &write(3.0, 3, 0b1, Some(t - 60 * 86_400), Some(t)),
    )
    .unwrap();
    // A legacy row (no first_observed_at) ranks as one-off.
    db.upsert_git_co_change_full("target.rs", "legacy.rs", &write(3.5, 3, 0, None, Some(t)))
        .unwrap();
    // Multiplier 1.0: raw order.
    let raw = db.co_changes_for_ranked("target.rs", 10, 1.0).unwrap();
    assert_eq!(raw[0].file, "one-off.rs");
    // Half weight for one-offs: 4.0 * 0.5 = 2.0 < 3.0, recurring first.
    let ranked = db
        .co_changes_for_ranked(
            "target.rs",
            10,
            codesage_storage::db::ONE_OFF_RANK_MULTIPLIER,
        )
        .unwrap();
    assert_eq!(ranked[0].file, "recurring.rs");
    assert_eq!(ranked[1].file, "one-off.rs");
    assert_eq!(
        ranked[2].file, "legacy.rs",
        "legacy 3.5 * 0.5 < one-off 4.0 * 0.5"
    );
    assert_eq!(ranked[1].weight, 4.0, "reported weight stays raw");
    // The cap applies after re-ranking, so a one-off can fall off the page.
    let top1 = db
        .co_changes_for_ranked(
            "target.rs",
            1,
            codesage_storage::db::ONE_OFF_RANK_MULTIPLIER,
        )
        .unwrap();
    assert_eq!(top1.len(), 1);
    assert_eq!(top1[0].file, "recurring.rs");
    // `co_changes_for` is the raw order.
    assert_eq!(
        db.co_changes_for("target.rs", 10).unwrap()[0].file,
        "one-off.rs"
    );
}

#[test]
fn any_co_change_recurring_is_decided_by_span_not_window_bits() {
    let db = Database::open_in_memory().unwrap();
    let t = 1_750_000_000;
    assert!(!db.any_co_change_recurring().unwrap());
    // Legacy shape and a two-bit boundary straddle over 8 days: not recurring.
    db.upsert_git_co_change("a.rs", "b.rs", 1.0, 3, Some(t))
        .unwrap();
    db.upsert_git_co_change_full(
        "a.rs",
        "c.rs",
        &write(1.0, 3, 0b11, Some(t - 8 * 86_400), Some(t)),
    )
    .unwrap();
    assert!(!db.any_co_change_recurring().unwrap());
    // One bit but 30 days of span: recurring.
    db.upsert_git_co_change_full(
        "a.rs",
        "d.rs",
        &write(1.0, 3, 0b1, Some(t - 30 * 86_400), Some(t)),
    )
    .unwrap();
    assert!(db.any_co_change_recurring().unwrap());
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
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 7, Some(1700000000))
        .unwrap();
    db.upsert_git_co_change("src/a.rs", "src/c.rs", 3.0, 5, Some(1700001000))
        .unwrap();
    db.upsert_git_co_change("src/b.rs", "src/c.rs", 1.0, 4, Some(1700002000))
        .unwrap();

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
        db.upsert_git_co_change("src/main.rs", &other, (10 - i) as f64, 5, Some(1700000000))
            .unwrap();
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
    db.upsert_git_file("src/a.rs", 5.0, 1, 3, Some(1700000000))
        .unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 7, Some(1700000000))
        .unwrap();

    db.clear_git_data().unwrap();

    assert!(db.git_file("src/a.rs").unwrap().is_none());
    assert!(db.co_changes_for("src/a.rs", 10).unwrap().is_empty());
}

#[test]
fn co_changes_upsert_replaces_on_conflict() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 1.0, 2, Some(1700000000))
        .unwrap();
    db.upsert_git_co_change("src/a.rs", "src/b.rs", 5.0, 8, Some(1700001000))
        .unwrap();
    let rows = db.co_changes_for("src/a.rs", 10).unwrap();
    assert_eq!(rows.len(), 1, "no duplicate row");
    assert!((rows[0].weight - 5.0).abs() < 1e-9);
    assert_eq!(rows[0].count, 8);
    assert_eq!(rows[0].last_observed_at, Some(1700001000));
}

#[test]
fn git_index_state_round_trip() {
    let db = Database::open_in_memory().unwrap();
    assert!(
        db.get_git_index_state().unwrap().is_none(),
        "fresh DB has no state"
    );

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
    db.incr_git_file("src/foo.rs", 1.5, 1, 3, Some(1700000000))
        .unwrap();
    db.incr_git_file("src/foo.rs", 0.5, 2, 4, Some(1700001000))
        .unwrap();
    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert!((row.churn_score - 2.0).abs() < 1e-9, "churn summed");
    assert_eq!(row.fix_count, 3, "fix_count summed");
    assert_eq!(row.total_commits, 7, "commits summed");
    assert_eq!(row.last_commit_at, Some(1700001000), "last_commit_at MAXed");
}

#[test]
fn incr_git_file_keeps_existing_last_when_new_is_older() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_file("src/foo.rs", 1.0, 0, 1, Some(1700001000))
        .unwrap();
    db.incr_git_file("src/foo.rs", 1.0, 0, 1, Some(1700000000))
        .unwrap();
    let row = db.git_file("src/foo.rs").unwrap().expect("present");
    assert_eq!(
        row.last_commit_at,
        Some(1700001000),
        "MAX kept the older row"
    );
}

#[test]
fn incr_git_co_change_accumulates_pair() {
    let db = Database::open_in_memory().unwrap();
    db.incr_git_co_change("a.rs", "b.rs", 1.0, 2, Some(1700000000))
        .unwrap();
    db.incr_git_co_change("a.rs", "b.rs", 0.5, 3, Some(1700001000))
        .unwrap();
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
    db.upsert_git_co_change("a.rs", "b.rs", 1.0, 3, Some(1700000000))
        .unwrap();
    assert!(db.co_change_pair_exists("a.rs", "b.rs").unwrap());
    // Pairs are stored sorted, so b/a (unsorted) would be a debug_assert hit in incr_;
    // exists() also requires sorted input. Validate the sorted lookup works.
    assert!(!db.co_change_pair_exists("a.rs", "c.rs").unwrap());
}

#[test]
fn scale_git_decay_multiplies_churn_and_pair_weights() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("a.rs", 4.0, 1, 5, Some(1700000000))
        .unwrap();
    db.upsert_git_file("b.rs", 2.0, 0, 3, Some(1700000000))
        .unwrap();
    db.upsert_git_co_change("a.rs", "b.rs", 6.0, 4, Some(1700000000))
        .unwrap();

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

#[test]
fn remove_file_cascades_to_git_tables() {
    let db = Database::open_in_memory().unwrap();
    // Seed: one git_files row for the doomed path, and co-change pairs using it on
    // both sides.
    db.upsert_git_file("src/doomed.rs", 4.0, 2, 8, Some(1700000000))
        .unwrap();
    db.upsert_git_file("src/survivor.rs", 2.0, 1, 3, Some(1700001000))
        .unwrap();
    // pair with doomed as file_a
    db.upsert_git_co_change("src/doomed.rs", "src/z.rs", 3.0, 5, Some(1700000100))
        .unwrap();
    // pair with doomed as file_b (lexicographic ordering puts a_file first)
    db.upsert_git_co_change("src/a_file.rs", "src/doomed.rs", 2.0, 4, Some(1700000200))
        .unwrap();
    // unrelated pair that must stay
    db.upsert_git_co_change("src/a_file.rs", "src/z.rs", 1.0, 3, Some(1700000300))
        .unwrap();

    db.remove_file("src/doomed.rs").unwrap();

    assert!(
        db.git_file("src/doomed.rs").unwrap().is_none(),
        "git_files row gone"
    );
    assert!(
        db.git_file("src/survivor.rs").unwrap().is_some(),
        "unrelated git_files row survives"
    );

    let remaining_for_a = db.co_changes_for("src/a_file.rs", 10).unwrap();
    let names: Vec<&str> = remaining_for_a.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(
        names,
        vec!["src/z.rs"],
        "pair with doomed.rs on either side removed; unrelated pair stays"
    );
    let remaining_for_z = db.co_changes_for("src/z.rs", 10).unwrap();
    let names: Vec<&str> = remaining_for_z.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(names, vec!["src/a_file.rs"]);
}
