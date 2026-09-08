//! `find_coupling` report tests over seeded git tables: asymmetric confidence,
//! the recurrence rank multiplier, span-based recurrence, and the one-off
//! note variants. The indexer is bypassed so the row values are controlled;
//! recurrence derivation itself is covered by the indexer unit tests and
//! `git_history_integration_test`.

use codesage_graph::{assess_risk, find_coupling, find_coupling_ranked, recommend_tests};
use codesage_storage::Database;
use codesage_storage::db::CoChangeWrite;

const DAY: i64 = 86_400;
/// Reference "now" for seeded timestamps.
const T0: i64 = 1_750_000_000;

fn pair(weight: f64, count: u32, window_mask: u64, first: i64, last: i64) -> CoChangeWrite {
    CoChangeWrite {
        weight,
        count,
        window_mask,
        first_observed_at: Some(first),
        last_observed_at: Some(last),
    }
}

/// A recurring pair: two window bits, spanning 200 days.
fn recurring(weight: f64, count: u32) -> CoChangeWrite {
    pair(weight, count, 0b11, T0 - 200 * DAY, T0)
}

/// A one-off pair: one window bit, all commits within `span` days.
fn one_off(weight: f64, count: u32, span: i64) -> CoChangeWrite {
    pair(weight, count, 0b1, T0 - span * DAY, T0)
}

#[test]
fn confidence_is_directional() {
    let db = Database::open_in_memory().unwrap();
    // A changed in 10 commits, B in 4, and they moved together 4 times: every
    // change to B came with A, but only 40% of A's changes touched B.
    db.upsert_git_file("A.php", 1.0, 0, 10, Some(T0)).unwrap();
    db.upsert_git_file("B.php", 1.0, 0, 4, Some(T0)).unwrap();
    db.upsert_git_co_change_full("A.php", "B.php", &recurring(2.0, 4))
        .unwrap();

    let from_a = find_coupling(&db, "A.php", 10).unwrap();
    assert_eq!(from_a.file_commits, 10);
    assert_eq!(from_a.coupled.len(), 1);
    let row = &from_a.coupled[0];
    assert_eq!(row.file, "B.php");
    assert!((row.confidence - 0.4).abs() < 1e-6, "P(B|A) = 4/10");
    assert!((row.reverse_confidence - 1.0).abs() < 1e-6, "P(A|B) = 4/4");
    assert_eq!(row.recurrence, 2);
    assert_eq!(row.span_days, 200);
    assert!(row.recurring);
    assert!(from_a.note.is_none(), "a recurring page carries no note");

    let from_b = find_coupling(&db, "B.php", 10).unwrap();
    let row = &from_b.coupled[0];
    assert_eq!(row.file, "A.php");
    assert!((row.confidence - 1.0).abs() < 1e-6, "P(A|B) = 4/4");
    assert!((row.reverse_confidence - 0.4).abs() < 1e-6, "P(B|A) = 4/10");
}

#[test]
fn confidence_is_zero_when_a_commit_total_is_unknown() {
    let db = Database::open_in_memory().unwrap();
    // Pair row without git_files rows on either side (a partially written
    // index): probabilities must read 0.0, not divide by zero or exceed 1.
    db.upsert_git_co_change("A.php", "B.php", 2.0, 4, None)
        .unwrap();
    let report = find_coupling(&db, "A.php", 10).unwrap();
    assert!(!report.file_indexed);
    assert_eq!(report.coupled[0].confidence, 0.0);
    assert_eq!(report.coupled[0].reverse_confidence, 0.0);
    assert_eq!(report.coupled[0].span_days, 0);
    // A stale total below the pair count clamps to 1.0.
    db.upsert_git_file("A.php", 1.0, 0, 2, None).unwrap();
    let report = find_coupling(&db, "A.php", 10).unwrap();
    assert_eq!(report.coupled[0].confidence, 1.0);
}

#[test]
fn recurring_pairs_outrank_one_offs_of_higher_raw_weight() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 20, None).unwrap();
    // Span-recurring: one window bit, 66 days of sustained coupling.
    db.upsert_git_co_change_full("target.rs", "span.rs", &one_off(6.0, 11, 66))
        .unwrap();
    // Boundary straddle: two window bits but only 8 days apart. Under the
    // window rule this ranked as recurring; under the span rule it is a
    // one-off and halves to 2.0.
    db.upsert_git_co_change_full(
        "target.rs",
        "straddle.rs",
        &pair(4.0, 4, 0b11, T0 - 8 * DAY, T0),
    )
    .unwrap();
    // A lighter genuinely recurring competitor: raw 2.5 < straddle's 4.0, but
    // it keeps its weight while the straddle halves, so it must come out
    // ahead of the straddle.
    db.upsert_git_co_change_full("target.rs", "light.rs", &recurring(2.5, 3))
        .unwrap();
    // A single mass commit, raw 3.0: halves to 1.5, last.
    db.upsert_git_co_change_full("target.rs", "mass.rs", &one_off(3.0, 3, 0))
        .unwrap();

    let ranked = find_coupling_ranked(&db, "target.rs", 10, true).unwrap();
    let files: Vec<&str> = ranked.coupled.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(
        files,
        vec!["span.rs", "light.rs", "straddle.rs", "mass.rs"],
        "ranking: 6.0, 2.5, 4.0*0.5, 3.0*0.5"
    );
    let by_file = |f: &str| ranked.coupled.iter().find(|e| e.file == f).expect(f);
    assert!(by_file("span.rs").recurring);
    assert_eq!(by_file("span.rs").recurrence, 1);
    assert!(
        !by_file("straddle.rs").recurring,
        "a straddle is not recurrence"
    );
    assert_eq!(by_file("straddle.rs").recurrence, 2);
    assert!(by_file("light.rs").recurring);
    assert!(!by_file("mass.rs").recurring);
    // Raw weights are reported, not the ranking values.
    assert_eq!(by_file("straddle.rs").weight, 4.0);
    assert_eq!(by_file("mass.rs").weight, 3.0);

    let raw = find_coupling_ranked(&db, "target.rs", 10, false).unwrap();
    let files: Vec<&str> = raw.coupled.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(files, vec!["span.rs", "straddle.rs", "mass.rs", "light.rs"]);
}

#[test]
fn a_month_of_co_changes_inside_one_window_is_recurring() {
    // mcp/mod.rs <-> params.rs on the real repo: 11 co-changes over 66 days,
    // all inside one calendar window. One window bit, but a 66-day span is
    // sustained coupling, not a mass commit.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("mod.rs", 1.0, 0, 30, None).unwrap();
    db.upsert_git_co_change_full("mod.rs", "params.rs", &one_off(6.0, 11, 66))
        .unwrap();
    // 29 days is still a burst.
    db.upsert_git_co_change_full("mod.rs", "burst.rs", &one_off(1.0, 3, 29))
        .unwrap();
    // Exactly 30 days crosses the line.
    db.upsert_git_co_change_full("mod.rs", "month.rs", &one_off(1.0, 3, 30))
        .unwrap();

    let report = find_coupling(&db, "mod.rs", 10).unwrap();
    let by_file = |f: &str| report.coupled.iter().find(|e| e.file == f).expect(f);
    assert_eq!(by_file("params.rs").recurrence, 1);
    assert_eq!(by_file("params.rs").span_days, 66);
    assert!(by_file("params.rs").recurring);
    assert_eq!(by_file("burst.rs").span_days, 29);
    assert!(!by_file("burst.rs").recurring);
    assert_eq!(by_file("month.rs").span_days, 30);
    assert!(by_file("month.rs").recurring);
    assert!(
        report.note.is_none(),
        "a page with recurring rows has no note"
    );
    // Span-recurring rows rank at raw weight, like window-recurring ones.
    assert_eq!(report.coupled[0].file, "params.rs");
}

#[test]
fn note_says_evidence_is_too_short_when_the_index_spans_under_thirty_days() {
    // A 20-day-old repository: no pair can reach the 30-day recurrence span
    // yet, and the note must say that rather than call the coupling one-off
    // or suggest a reindex.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 10))
        .unwrap();
    db.upsert_git_co_change_full("other.rs", "y.rs", &one_off(1.0, 3, 20))
        .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert_eq!(report.coupled.len(), 1);
    assert!(!report.coupled[0].recurring);
    let note = report.note.as_deref().expect("short-evidence note");
    assert!(note.contains("evidence spans only 20 days"), "{note}");
    assert!(note.contains("within 10 days"), "{note}");
    assert!(!note.contains("--full"), "{note}");
}

#[test]
fn note_is_short_burst_when_a_recurring_pair_exists_inside_ninety_days() {
    // 5-day pair on the queried file, 40-day recurring pair elsewhere: the
    // index is 40 days old and already shows recurrence, so the note must not
    // claim recurrence "cannot be observed yet".
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 5))
        .unwrap();
    db.upsert_git_co_change_full("other.rs", "y.rs", &one_off(1.0, 3, 40))
        .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert!(!report.coupled[0].recurring);
    let note = report.note.as_deref().expect("short-burst note");
    assert!(note.contains("short-burst evidence"), "{note}");
    assert!(note.contains("within 5 days"), "{note}");
    assert!(!note.contains("cannot be observed"), "{note}");
    assert!(!note.contains("--full"), "{note}");
    // The recurring pair itself reads as such.
    let other = find_coupling(&db, "other.rs", 10).unwrap();
    assert!(other.coupled[0].recurring);
    assert!(other.note.is_none());
}

#[test]
fn note_says_short_burst_throughout_when_a_rebuilt_index_has_no_recurring_pair() {
    // 400 days of history, every pair fully populated (no NULL
    // first_observed_at), none spanning 30 days: a freshly rebuilt index whose
    // project really does couple in bursts. The reindex hint would be false.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 5))
        .unwrap();
    db.upsert_git_co_change_full(
        "old.rs",
        "z.rs",
        &pair(1.0, 3, 0b1, T0 - 400 * DAY, T0 - 395 * DAY),
    )
    .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    let note = report.note.as_deref().expect("throughout note");
    assert!(note.contains("short-burst throughout"), "{note}");
    assert!(note.contains("within 5 days"), "{note}");
    assert!(!note.contains("--full"), "{note}");

    // Once any pair anywhere recurs, the note describes the page as
    // short-burst evidence instead.
    db.upsert_git_co_change_full("p.rs", "q.rs", &recurring(1.0, 3))
        .unwrap();
    let report = find_coupling(&db, "target.rs", 10).unwrap();
    let note = report.note.as_deref().expect("short-burst note");
    assert!(note.contains("short-burst evidence"), "{note}");
    assert!(!note.contains("throughout"), "{note}");
    assert!(!note.contains("--full"), "{note}");

    // A recurring pair on the page clears the note.
    db.upsert_git_co_change_full("target.rs", "w.rs", &recurring(0.5, 3))
        .unwrap();
    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert!(report.note.is_none());
}

#[test]
fn note_suggests_full_reindex_only_when_legacy_rows_explain_the_absence() {
    // Same long history, nothing recurring, but one pair still has no
    // first_observed_at (written before migration 0017): the missing data can
    // explain the absence, so the note points at `--full`.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 0))
        .unwrap();
    db.upsert_git_co_change_full(
        "old.rs",
        "z.rs",
        &CoChangeWrite {
            weight: 1.0,
            count: 3,
            window_mask: 0,
            first_observed_at: None,
            last_observed_at: Some(T0 - 400 * DAY),
        },
    )
    .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    let note = report.note.as_deref().expect("reindex note");
    assert!(note.contains("codesage git-index --full"), "{note}");
    assert!(note.contains("within a day"), "{note}");
    assert!(!note.contains("throughout"), "{note}");
}

#[test]
fn legacy_rows_win_over_the_too_short_wording_when_spans_collapse_to_zero() {
    // Every row is legacy-shaped except the queried pair, whose commits sit in
    // one day: the whole-table span reads 0 (< 30), which used to pick the
    // "cannot be observed yet" arm and hide the reindex hint.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 0))
        .unwrap();
    for other in ["p.rs", "q.rs"] {
        db.upsert_git_co_change_full(
            "legacy.rs",
            other,
            &CoChangeWrite {
                weight: 1.0,
                count: 3,
                window_mask: 0,
                first_observed_at: None,
                last_observed_at: Some(T0),
            },
        )
        .unwrap();
    }
    let report = find_coupling(&db, "target.rs", 10).unwrap();
    let note = report.note.as_deref().expect("reindex note");
    assert!(note.contains("codesage git-index --full"), "{note}");
    assert!(!note.contains("cannot be observed"), "{note}");
}

/// A legacy row: written before migration 0017, never rewritten by `--full`.
fn legacy(weight: f64, count: u32, last: i64) -> CoChangeWrite {
    CoChangeWrite {
        weight,
        count,
        window_mask: 0,
        first_observed_at: None,
        last_observed_at: Some(last),
    }
}

#[test]
fn span_unknown_note_wins_when_the_page_has_legacy_rows_even_if_another_pair_in_the_table_recurs() {
    // The queried file's pairs are all unbaselined while a baselined recurring
    // pair exists elsewhere: the page cannot be called short-burst, it is
    // simply unmeasured.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &legacy(2.0, 3, T0))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "y.rs", &legacy(1.0, 3, T0))
        .unwrap();
    db.upsert_git_co_change_full("other.rs", "z.rs", &recurring(1.0, 3))
        .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert_eq!(report.coupled.len(), 2);
    assert!(
        report
            .coupled
            .iter()
            .all(|e| !e.recurring && e.span_days == 0)
    );
    let note = report.note.as_deref().expect("span-unknown note");
    assert!(
        note.contains("span unknown for 2 of these 2 pairs"),
        "{note}"
    );
    assert!(note.contains("codesage git-index --full"), "{note}");
    assert!(!note.contains("short-burst"), "{note}");

    // A mixed page counts only the unbaselined rows.
    db.upsert_git_co_change_full("target.rs", "w.rs", &one_off(0.5, 3, 3))
        .unwrap();
    let report = find_coupling(&db, "target.rs", 10).unwrap();
    let note = report.note.as_deref().expect("span-unknown note");
    assert!(
        note.contains("span unknown for 2 of these 3 pairs"),
        "{note}"
    );
}

#[test]
fn span_unknown_note_appears_on_a_mixed_page_with_a_recurring_row() {
    // The normal post-upgrade state under the hooks: legacy rows next to one
    // 0017-aware recurring pair on the same page. The recurring row must not
    // silence the disclosure, and the rows themselves say which are unknown.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &legacy(2.0, 3, T0))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "y.rs", &legacy(1.0, 3, T0))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "fresh.rs", &recurring(1.5, 3))
        .unwrap();

    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert_eq!(report.coupled.len(), 3);
    let by_file = |f: &str| report.coupled.iter().find(|e| e.file == f).expect(f);
    assert!(by_file("fresh.rs").recurring && by_file("fresh.rs").span_known);
    assert!(!by_file("x.rs").span_known && !by_file("x.rs").recurring);
    assert!(!by_file("y.rs").span_known);
    let note = report
        .note
        .as_deref()
        .expect("span-unknown note on a mixed page");
    assert!(
        note.contains("span unknown for 2 of these 3 pairs"),
        "{note}"
    );
    assert!(note.contains("codesage git-index --full"), "{note}");

    // A fully baselined page with a recurring row carries no note.
    db.upsert_git_co_change_full("target.rs", "x.rs", &one_off(2.0, 3, 2))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "y.rs", &one_off(1.0, 3, 2))
        .unwrap();
    let report = find_coupling(&db, "target.rs", 10).unwrap();
    assert!(report.coupled.iter().all(|e| e.span_known));
    assert!(report.note.is_none());
}

#[test]
fn recommend_tests_notes_coupled_tests_with_unknown_span() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 20, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "tests/old_test.rs", &legacy(4.0, 4, T0))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "tests/new_test.rs", &recurring(3.0, 3))
        .unwrap();

    let recs = recommend_tests(&db, &["target.rs".to_string()]).unwrap();
    let files: Vec<&str> = recs.coupled.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(
        files,
        vec!["tests/new_test.rs", "tests/old_test.rs"],
        "unknown span ranks at half weight: 4.0 * 0.5 < 3.0"
    );
    let old = &recs.coupled[1];
    assert!(!old.span_known && !old.recurring && old.span_days == 0);
    assert!(recs.coupled[0].span_known);
    assert!(
        recs.notes.iter().any(
            |n| n.contains("span unknown for 1 of the 2 coupled test(s)")
                && n.contains("codesage git-index --full")
        ),
        "{:?}",
        recs.notes
    );
}

#[test]
fn assess_risk_reports_span_unknown_rows_in_top_coupled() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 8, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "x.rs", &legacy(2.0, 3, T0))
        .unwrap();
    db.upsert_git_co_change_full("target.rs", "y.rs", &recurring(1.0, 3))
        .unwrap();
    let risk = assess_risk(&db, "target.rs").unwrap();
    assert_eq!(risk.top_coupled.len(), 2);
    assert!(
        risk.notes.iter().any(|n| n
            .contains("span unknown for 1 of this file's top 2 co-change pairs")
            && n.contains("codesage git-index --full")),
        "{:?}",
        risk.notes
    );
}

#[test]
fn assess_risk_counts_a_test_promoted_into_the_ranked_list() {
    // Ten one-off sources at 10.0 push a recurring test at 9.0 out of the raw
    // top ten; the ranked page halves the sources to 5.0 and the test leads.
    // A raw-only coupled-test check would report a test gap here.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 40, None).unwrap();
    for i in 0..10 {
        db.upsert_git_co_change_full("target.rs", &format!("src/m{i}.rs"), &one_off(10.0, 4, 2))
            .unwrap();
    }
    db.upsert_git_co_change_full("target.rs", "tests/promoted_test.rs", &recurring(9.0, 5))
        .unwrap();

    let raw = db.co_changes_for("target.rs", 10).unwrap();
    assert!(raw.iter().all(|r| r.file != "tests/promoted_test.rs"));
    let risk = assess_risk(&db, "target.rs").unwrap();
    assert_eq!(risk.top_coupled[0].file, "tests/promoted_test.rs");
    assert!(!risk.test_gap, "the ranked page holds a coupled test");
    assert!(!risk.notes.iter().any(|n| n.starts_with("test gap")));
    assert!(!risk.notes.iter().any(|n| n.contains("below the ranked")));
}

#[test]
fn recommend_tests_scopes_the_absence_advice_when_the_fetch_was_cut() {
    // Twenty-one non-test partners and no test anywhere: the absence claim is
    // limited to what was consulted and the cut note follows it.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 60, None).unwrap();
    for i in 0..21 {
        db.upsert_git_co_change_full(
            "target.rs",
            &format!("src/mod_{i:02}.rs"),
            &recurring(5.0, 5),
        )
        .unwrap();
    }
    let recs = recommend_tests(&db, &["target.rs".to_string()]).unwrap();
    assert!(recs.primary.is_empty());
    assert!(recs.coupled.is_empty());
    assert!(
        recs.notes
            .iter()
            .any(|n| n.contains("among the top 20 co-change partners")),
        "{:?}",
        recs.notes
    );
    assert!(
        recs.notes.iter().any(|n| n.contains("beyond the top 20")),
        "{:?}",
        recs.notes
    );
    assert!(
        !recs
            .notes
            .iter()
            .any(|n| n.contains("via sibling conventions or co-change history;")),
        "unscoped absence claim must not appear: {:?}",
        recs.notes
    );
}

#[test]
fn assess_risk_names_a_coupled_test_demoted_out_of_the_ranked_list() {
    // A one-off test at raw rank 5 keeps `test_gap` false but is demoted below
    // ten recurring sources in the ranked `top_coupled`; the note must name it
    // so the two fields do not contradict.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 40, None).unwrap();
    for (i, w) in [13.0, 12.0, 11.0, 10.0, 5.9, 5.8, 5.7, 5.6, 5.5, 5.4]
        .iter()
        .enumerate()
    {
        db.upsert_git_co_change_full("target.rs", &format!("src/m{i}.rs"), &recurring(*w, 5))
            .unwrap();
    }
    db.upsert_git_co_change_full("target.rs", "tests/hidden_test.rs", &one_off(6.0, 4, 0))
        .unwrap();

    // Sanity on the two pages.
    let raw = db.co_changes_for("target.rs", 10).unwrap();
    assert_eq!(raw[4].file, "tests/hidden_test.rs", "raw rank 5");
    let ranked = find_coupling(&db, "target.rs", 10).unwrap();
    assert!(
        ranked
            .coupled
            .iter()
            .all(|e| e.file != "tests/hidden_test.rs"),
        "6.0 * 0.5 = 3.0 sits below every recurring source"
    );

    let risk = assess_risk(&db, "target.rs").unwrap();
    assert!(!risk.test_gap, "the raw page holds a coupled test");
    assert!(
        risk.top_coupled
            .iter()
            .all(|e| e.file != "tests/hidden_test.rs")
    );
    assert!(
        risk.notes
            .iter()
            .any(|n| n.contains("below the ranked `top_coupled` list")
                && n.contains("tests/hidden_test.rs")),
        "{:?}",
        risk.notes
    );
    assert!(!risk.notes.iter().any(|n| n.starts_with("test gap")));
    assert!(
        risk.notes
            .iter()
            .any(|n| n.contains("one-off coupled test"))
    );

    db.upsert_git_co_change_full(
        "target.rs",
        "tests/hidden_test.rs",
        &CoChangeWrite {
            weight: 6.0,
            count: 4,
            window_mask: 0,
            first_observed_at: None,
            last_observed_at: Some(T0),
        },
    )
    .unwrap();
    let risk = assess_risk(&db, "target.rs").unwrap();
    assert!(!risk.test_gap);
    assert!(
        risk.top_coupled
            .iter()
            .all(|e| e.file != "tests/hidden_test.rs")
    );
    assert!(
        risk.notes.iter().any(|n| n.contains("span unknown")
            && n.contains("tests/hidden_test.rs")
            && n.contains("git-index --full")),
        "{:?}",
        risk.notes
    );
    assert!(
        !risk
            .notes
            .iter()
            .any(|n| n.contains("one-off coupled test"))
    );
}

/// Seed `target.rs` with test partners of known shape and return the file
/// order `find_coupling` reports for the tests among them.
fn seed_test_partners(db: &Database) -> Vec<String> {
    db.upsert_git_file("target.rs", 1.0, 0, 30, None).unwrap();
    // one-off test, strongest raw weight (6.0 -> 3.0 demoted).
    db.upsert_git_co_change_full("target.rs", "tests/burst_test.rs", &one_off(6.0, 4, 3))
        .unwrap();
    // recurring test, lighter (4.0, keeps 4.0).
    db.upsert_git_co_change_full("target.rs", "tests/steady_test.rs", &recurring(4.0, 5))
        .unwrap();
    // recurring test, lightest (2.0).
    db.upsert_git_co_change_full("target.rs", "tests/slow_test.rs", &recurring(2.0, 3))
        .unwrap();
    // a source partner between them, ignored by the coupled bucket.
    db.upsert_git_co_change_full("target.rs", "helper.rs", &recurring(5.0, 5))
        .unwrap();
    find_coupling(db, "target.rs", 20)
        .unwrap()
        .coupled
        .into_iter()
        .map(|e| e.file)
        .filter(|f| f.starts_with("tests/"))
        .collect()
}

#[test]
fn recommend_tests_coupled_bucket_orders_like_find_coupling() {
    let db = Database::open_in_memory().unwrap();
    let coupling_order = seed_test_partners(&db);
    assert_eq!(
        coupling_order,
        vec![
            "tests/steady_test.rs",
            "tests/burst_test.rs",
            "tests/slow_test.rs"
        ],
        "4.0 recurring, 6.0*0.5 one-off, 2.0 recurring"
    );

    let recs = recommend_tests(&db, &["target.rs".to_string()]).unwrap();
    let tests_for_order: Vec<&str> = recs.coupled.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(tests_for_order, coupling_order);
    let burst = recs
        .coupled
        .iter()
        .find(|e| e.file == "tests/burst_test.rs")
        .unwrap();
    assert_eq!(burst.weight, 6.0, "raw weight is reported");
    assert_eq!(burst.span_days, 3);
    assert!(!burst.recurring);
    assert!(recs.coupled[0].recurring);
    assert_eq!(recs.coupled[0].span_days, 200);
    // Well under the fetch cap: no cut note.
    assert!(
        !recs.notes.iter().any(|n| n.contains("beyond the top")),
        "{:?}",
        recs.notes
    );
}

#[test]
fn recommend_tests_discloses_when_the_co_change_fetch_cap_cut_candidates() {
    // Twenty-one recurring source partners outrank a strong one-off test once
    // it is demoted, pushing it past the 20 consulted; the bucket must say so
    // instead of silently omitting the test.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 60, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "tests/burst_test.rs", &one_off(9.0, 4, 2))
        .unwrap();
    for i in 0..21 {
        db.upsert_git_co_change_full(
            "target.rs",
            &format!("src/mod_{i:02}.rs"),
            &recurring(5.0, 5),
        )
        .unwrap();
    }
    // Sanity: find_coupling with a wide limit still lists the test, below the
    // recurring sources.
    let full = find_coupling(&db, "target.rs", 50).unwrap();
    assert_eq!(full.coupled.len(), 22);
    assert_eq!(full.coupled[21].file, "tests/burst_test.rs");

    let recs = recommend_tests(&db, &["target.rs".to_string()]).unwrap();
    assert!(recs.coupled.is_empty(), "{:?}", recs.coupled);
    let note = recs
        .notes
        .iter()
        .find(|n| n.contains("beyond the top 20"))
        .unwrap_or_else(|| panic!("cut must be disclosed, notes: {:?}", recs.notes));
    assert!(note.contains("target.rs"), "{note}");

    // With nineteen sources the test is the 20th row by rank, inside the
    // cap after the +1 probe; it appears and no cut is reported.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 60, None).unwrap();
    db.upsert_git_co_change_full("target.rs", "tests/burst_test.rs", &one_off(9.0, 4, 2))
        .unwrap();
    for i in 0..19 {
        db.upsert_git_co_change_full(
            "target.rs",
            &format!("src/mod_{i:02}.rs"),
            &recurring(5.0, 5),
        )
        .unwrap();
    }
    let recs = recommend_tests(&db, &["target.rs".to_string()]).unwrap();
    assert_eq!(recs.coupled.len(), 1);
    assert_eq!(recs.coupled[0].file, "tests/burst_test.rs");
    assert!(!recs.notes.iter().any(|n| n.contains("beyond the top")));
}

#[test]
fn legacy_rows_without_recurrence_columns_read_as_one_off() {
    // Rows written by the 5-argument upsert (or before migration 0017) carry
    // mask 0 and no span; they read as recurrence 1, span 0, not recurring.
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("a.rs", 1.0, 0, 6, None).unwrap();
    db.upsert_git_co_change("a.rs", "b.rs", 2.0, 3, Some(T0))
        .unwrap();
    let report = find_coupling(&db, "a.rs", 10).unwrap();
    assert_eq!(report.coupled[0].recurrence, 1);
    assert_eq!(report.coupled[0].span_days, 0);
    assert!(!report.coupled[0].recurring);
    assert!(report.note.is_some());
}
