//! What `build_edit_brief` will and will not say. Seeds git_files and
//! git_co_changes directly so the inputs are controlled.
//!
//! These pin decisions that were made by reading real payloads across three
//! repos, and each one is a thing that regresses silently: a wrong `tests:`
//! line still looks like a plausible answer.

use codesage_graph::{build_edit_brief, full_index};
use codesage_storage::Database;

/// One source file with a sibling test, one unrelated test that merely
/// co-changes with it, and enough churn rows for percentiles to be meaningful.
fn project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("tests/Unit")).unwrap();
    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/Unit/RepositoryTest.php"),
        b"<?php\nnamespace Tests;\nclass RepositoryTest {\n  public function testFind() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/Unit/BillingTest.php"),
        b"<?php\nnamespace Tests;\nclass BillingTest {\n  public function testCharge() {}\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

/// Fill out the churn distribution so a seeded file's percentile is not an
/// artifact of being the only row.
fn seed_background_churn(db: &Database) {
    for i in 0..20 {
        db.upsert_git_file(&format!("filler{i}.php"), f64::from(i) * 0.1, 0, 1, None)
            .unwrap();
    }
}

#[test]
fn a_co_changed_test_is_not_reported_as_a_test_of_the_file() {
    let (_dir, db) = project();
    seed_background_churn(&db);
    db.upsert_git_file("Repository.php", 99.0, 6, 12, None)
        .unwrap();
    // BillingTest tests something else entirely; it just moves at the same time.
    db.upsert_git_co_change("Repository.php", "tests/Unit/BillingTest.php", 9.0, 9, None)
        .unwrap();

    let brief = build_edit_brief(&db, "Repository.php", None).unwrap();

    assert!(
        brief.tests.iter().any(|t| t.contains("RepositoryTest")),
        "the sibling test is the one thing `tests` promises: {:?}",
        brief.tests
    );
    assert!(
        !brief.tests.iter().any(|t| t.contains("BillingTest")),
        "a co-changed test is a correlation, not a test OF this file: {:?}",
        brief.tests
    );
    // Not lost, just labelled honestly.
    assert!(
        brief.coupled.iter().any(|c| c.contains("BillingTest")),
        "it should still surface as a co-changer: {:?}",
        brief.coupled
    );
}

#[test]
fn churn_rank_alone_does_not_make_a_hotspot() {
    let (_dir, db) = project();
    seed_background_churn(&db);
    // Top of the repo by churn, but on two commits. Percentile is a within-repo
    // rank, so a young file rides to the top of a quiet corpus.
    db.upsert_git_file("Repository.php", 99.0, 1, 2, None)
        .unwrap();

    let brief = build_edit_brief(&db, "Repository.php", None).unwrap();
    assert!(
        brief.churn_percentile.unwrap() >= 0.75,
        "fixture should rank high: {:?}",
        brief.churn_percentile
    );
    assert!(
        !brief.hotspot,
        "two commits cannot support a fix ratio, whatever the rank"
    );
}

#[test]
fn enough_commits_behind_a_high_rank_does_make_a_hotspot() {
    let (_dir, db) = project();
    seed_background_churn(&db);
    db.upsert_git_file("Repository.php", 99.0, 6, 12, None)
        .unwrap();

    let brief = build_edit_brief(&db, "Repository.php", None).unwrap();
    assert!(brief.hotspot);
    assert!(!brief.empty);
}

#[test]
fn a_file_with_nothing_to_say_is_empty() {
    let (_dir, db) = project();
    seed_background_churn(&db);
    // No git row, no sibling test, no co-changers.
    let brief = build_edit_brief(&db, "tests/Unit/BillingTest.php", None).unwrap();
    assert!(brief.empty, "unexpected payload: {brief:?}");
    assert!(!brief.hotspot);
    assert!(brief.tests.is_empty());
    assert!(brief.coupled.is_empty());
}

#[test]
fn an_unknown_path_is_empty_rather_than_an_error() {
    let (_dir, db) = project();
    let brief = build_edit_brief(&db, "does/not/exist.php", None).unwrap();
    assert!(brief.empty);
}
