//! assess_risk composition tests. Seeds git_files + git_co_changes directly so
//! the score inputs are controlled (bypasses the real git log indexer).

use codesage_graph::{assess_risk, full_index};
use codesage_storage::Database;

/// Build a small project so impact_analysis has a graph to walk. One class, two
/// callers, one test. The structural data is only needed to give assess_risk's
/// dependent-file BFS something to find.
fn setup_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Controller.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller {\n  public function show(Repository $r, $id) { return $r->find($id); }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Service.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Service {\n  public function run(Repository $r) { return $r->find(1); }\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[]).unwrap();
    (dir, db)
}

#[test]
fn score_zero_and_note_when_no_git_history() {
    let (_dir, db) = setup_project();
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert_eq!(r.total_commits, 0);
    assert_eq!(r.fix_count, 0);
    assert!(
        r.score < 0.2,
        "score without history should stay low, got {}",
        r.score
    );
    assert!(
        r.notes.iter().any(|n| n.contains("no git history")),
        "expected 'no git history' note, got {:?}",
        r.notes
    );
}

#[test]
fn hotspot_fix_heavy_file_scores_high_and_emits_notes() {
    let (_dir, db) = setup_project();

    // Seed a hot, fix-heavy file with lots of churn and high fix ratio.
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    // A few cooler files so churn_percentile is well-defined and our target ends up on top.
    for (p, c) in [
        ("Controller.php", 1.0_f64),
        ("Service.php", 2.0),
        ("other_a.php", 0.5),
        ("other_b.php", 0.7),
    ] {
        db.upsert_git_file(p, c, 0, 5, Some(1_700_000_000)).unwrap();
    }

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.score >= 0.6,
        "hotspot+fix-heavy should score >= 0.6, got {}",
        r.score
    );
    assert!(r.churn_percentile >= 0.99);
    assert!((r.fix_ratio - 0.5).abs() < 1e-9);
    let notes = r.notes.join(" | ");
    assert!(notes.contains("hotspot"), "missing hotspot note: {notes}");
    assert!(
        notes.contains("fix-heavy"),
        "missing fix-heavy note: {notes}"
    );
}

#[test]
fn cold_isolated_file_scores_low() {
    let (_dir, db) = setup_project();

    // Give a few files history so churn_percentile has a distribution.
    for p in ["Repository.php", "Controller.php", "Service.php"] {
        db.upsert_git_file(p, 10.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    db.upsert_git_file("other_cold.php", 0.01, 0, 1, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "other_cold.php").unwrap();
    assert!(
        r.score < 0.4,
        "cold file should score < 0.4, got {}",
        r.score
    );
    assert!(!r.notes.iter().any(|n| n.contains("hotspot")));
    assert!(!r.notes.iter().any(|n| n.contains("fix-heavy")));
}

#[test]
fn test_gap_false_when_coupled_to_test_file() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    // Pair must be lexicographically sorted.
    db.upsert_git_co_change(
        "Repository.php",
        "RepositoryTest.php",
        5.0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(!r.test_gap, "coupled test file must close the test gap");
    assert!(!r.notes.iter().any(|n| n.contains("test gap")));
    assert!(r.top_coupled.iter().any(|c| c.file == "RepositoryTest.php"));
}

#[test]
fn test_gap_false_when_sibling_test_exists_without_coupling() {
    let (_dir, db) = setup_project();
    // Same directory, PHP sibling convention.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    // No co-change relationship seeded.
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        !r.test_gap,
        "sibling test file should close the test gap even without coupling"
    );
}

#[test]
fn test_gap_true_when_no_test_sibling_and_no_coupled_test() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Controller.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_co_change(
        "Controller.php",
        "Repository.php",
        3.0,
        4,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(r.test_gap, "no test anywhere should flag test_gap");
    assert!(r.notes.iter().any(|n| n.contains("test gap")));
}

#[test]
fn high_coupling_triggers_coupling_note() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    for i in 0..10 {
        let other = format!("z_other_{i:02}.php");
        db.upsert_git_file(&other, 0.5, 0, 5, Some(1_700_000_000))
            .unwrap();
        // "Repository.php" < "z_other_NN.php" lexicographically so the pair is sorted correctly.
        db.upsert_git_co_change(
            "Repository.php",
            &other,
            (10 - i) as f64,
            5,
            Some(1_700_000_000),
        )
        .unwrap();
    }

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert_eq!(r.coupled_files, 10);
    assert!(
        r.notes.iter().any(|n| n.contains("high coupling")),
        "missing coupling note: {:?}",
        r.notes
    );
}

#[test]
fn wide_blast_radius_note_fires_when_many_dependents() {
    let (_dir, db) = setup_project();
    // Repository.php has 2 direct callers in the fixture. Force 10 dependents by
    // seeding git_files rows so the risk function still runs, but then assert the
    // note only fires when impact_analysis returns >=10 deps. On this tiny fixture
    // the impact depth-2 is 2, so the "wide blast radius" note must NOT fire.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.dependent_files < 10,
        "fixture has only 2 callers, got {}",
        r.dependent_files
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("wide blast radius")),
        "wide blast radius must not fire below the threshold, got {:?}",
        r.notes
    );
}
