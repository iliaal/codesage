use std::path::Path;
use std::process::Command;

use codesage_graph::{IndexMode, assess_risk, assess_risk_batch, git_history_index_with_options};
use codesage_storage::Database;

const DAY: i64 = 86_400;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(root: &Path, email: &str, time: i64, body: &str) {
    std::fs::write(root.join("feature.rs"), body).unwrap();
    git(root, &["add", "feature.rs"]);
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "feat: improve feature",
        ])
        .env("GIT_AUTHOR_NAME", "Contributor")
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", "Integrator")
        .env("GIT_COMMITTER_EMAIL", "integrator@example.com")
        .env("GIT_AUTHOR_DATE", format!("@{time} +0000"))
        .env("GIT_COMMITTER_DATE", format!("@{time} +0000"))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn real_git_full_and_incremental_preserve_same_authors_and_risk_disclosure() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    commit(
        root.path(),
        "ALICE@example.com",
        now - 360 * DAY,
        "fn one() {}\n",
    );
    let incremental = Database::open_in_memory().unwrap();
    git_history_index_with_options(&incremental, root.path(), &[], IndexMode::Full).unwrap();
    commit(
        root.path(),
        "alice@example.com",
        now - 180 * DAY,
        "fn two() {}\n",
    );
    commit(root.path(), "bob@example.com", now, "fn three() {}\n");
    git_history_index_with_options(&incremental, root.path(), &[], IndexMode::Incremental).unwrap();
    let full = Database::open_in_memory().unwrap();
    git_history_index_with_options(&full, root.path(), &[], IndexMode::Full).unwrap();
    assert_eq!(
        incremental.git_author_events("feature.rs").unwrap(),
        full.git_author_events("feature.rs").unwrap()
    );
    let risk = assess_risk(&incremental, "feature.rs").unwrap();
    let concentration = risk.author_concentration.as_ref().unwrap();
    assert_eq!(concentration.author_count, 2);
    assert!((concentration.dominant_share - 4.0 / 7.0).abs() < 1e-10);
    assert_eq!(concentration.half_life_days, 180);
    assert_eq!(concentration.history_days, 730);
    assert!(
        risk.notes
            .iter()
            .any(|note| note.contains("informational only"))
    );
    let mut concise = risk;
    concise.verbose = false;
    let wire = serde_json::to_value(concise).unwrap();
    assert_eq!(wire["author_concentration"]["author_count"], 2);
    let batch =
        assess_risk_batch(&incremental, &["feature.rs".into(), "feature.rs".into()]).unwrap();
    assert_eq!(
        batch.files[0].author_concentration.as_ref().unwrap().as_of,
        batch.files[1].author_concentration.as_ref().unwrap().as_of
    );
    let before = incremental.git_author_events("feature.rs").unwrap();
    git_history_index_with_options(&incremental, root.path(), &[], IndexMode::Incremental).unwrap();
    assert_eq!(before, incremental.git_author_events("feature.rs").unwrap());
}

#[test]
fn legacy_and_empty_author_history_are_disclosed_separately() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("feature.rs", 1.0, 0, 1, None).unwrap();
    let legacy = assess_risk(&db, "feature.rs").unwrap();
    assert!(legacy.author_concentration.is_none());
    assert!(
        legacy
            .notes
            .iter()
            .any(|note| note.contains("git-index --full"))
    );
    db.reset_git_authors().unwrap();
    let empty = assess_risk(&db, "feature.rs").unwrap();
    assert!(empty.author_concentration.is_none());
    assert!(
        empty
            .notes
            .iter()
            .any(|note| note.contains("no indexed author history for this path"))
    );
    assert_eq!(legacy.score, empty.score);
    db.upsert_git_author_event("feature.rs", "missing-identity", "", i64::MAX)
        .unwrap();
    let missing = assess_risk(&db, "feature.rs").unwrap();
    assert!(missing.author_concentration.is_none());
    assert!(
        missing
            .notes
            .iter()
            .any(|note| note.contains("author identities are missing"))
    );
}

#[test]
fn filtered_author_history_names_the_actual_anchor_without_changing_risk() {
    let db = Database::open_in_memory().unwrap();
    let anchor = 1_700_000_000;
    db.upsert_git_file("feature.rs", 1.0, 0, 1, Some(anchor))
        .unwrap();
    db.reset_git_authors().unwrap();
    let empty = assess_risk(&db, "feature.rs").unwrap();
    db.upsert_git_author_event(
        "feature.rs",
        "old",
        "email:a@example.com",
        anchor - 731 * DAY,
    )
    .unwrap();
    let filtered = assess_risk(&db, "feature.rs").unwrap();
    assert!(filtered.author_concentration.is_none());
    assert!(
        filtered
            .notes
            .iter()
            .any(|note| note.contains("730-day window")
                && note.contains("newest indexed commit for this file")
                && note.contains("1700000000")),
        "{:?}",
        filtered.notes
    );
    assert_eq!(filtered.score, empty.score);
    db.upsert_file(&codesage_protocol::FileInfo {
        path: "untracked.rs".into(),
        language: codesage_protocol::Language::Rust,
        content_hash: "fixture".into(),
    })
    .unwrap();
    let absent = assess_risk(&db, "untracked.rs").unwrap();
    assert!(
        absent
            .notes
            .iter()
            .any(|note| note.contains("no indexed author history for this path")),
        "{:?}",
        absent.notes
    );
    db.upsert_git_author_event("untracked.rs", "old", "email:a@example.com", 1)
        .unwrap();
    let filtered = assess_risk(&db, "untracked.rs").unwrap();
    assert!(
        filtered.notes.iter().any(|note| note.contains("730d@now")),
        "{:?}",
        filtered.notes
    );
    assert_eq!(absent.score, filtered.score);
}

#[test]
fn legacy_history_window_remains_unknown_until_full_rebuild() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let old = 1_500_000_000;
    commit(root.path(), "a@example.com", old, "fn first() {}\n");
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(sha.status.success());
    let db = Database::open_in_memory().unwrap();
    db.set_git_index_state(String::from_utf8(sha.stdout).unwrap().trim())
        .unwrap();
    db.upsert_git_file("feature.rs", 1.0, 0, 1, Some(old))
        .unwrap();
    let unknown = || {
        let note = codesage_graph::find_coupling(&db, "feature.rs", 5)
            .unwrap()
            .note
            .unwrap();
        assert!(note.contains("window provenance is unknown"), "{note}");
        assert!(note.contains("git-index --full"), "{note}");
        assert!(!note.contains("730d@HEAD"), "{note}");
    };
    unknown();
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Incremental).unwrap();
    unknown();
    commit(root.path(), "b@example.com", old + DAY, "fn second() {}\n");
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Incremental).unwrap();
    unknown();
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Full).unwrap();
    let note = codesage_graph::find_coupling(&db, "feature.rs", 5)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("730d@HEAD"), "{note}");
    assert!(note.contains("730 days before HEAD"), "{note}");
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Incremental).unwrap();
    let note = codesage_graph::find_coupling(&db, "feature.rs", 5)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("730d@HEAD"), "{note}");
    db.execute_raw_for_tests(
        "UPDATE git_index_state SET last_sha = last_sha, last_indexed_at = last_indexed_at",
    )
    .unwrap();
    unknown();
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Incremental).unwrap();
    unknown();
}

#[test]
fn legacy_incremental_requires_full_rebuild_before_claiming_complete_history() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    commit(
        root.path(),
        "alice@example.com",
        now - DAY,
        "fn first() {}\n",
    );
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(sha.status.success());
    let db = Database::open_in_memory().unwrap();
    db.set_git_index_state(String::from_utf8(sha.stdout).unwrap().trim())
        .unwrap();
    db.upsert_git_file("feature.rs", 1.0, 0, 1, Some(now - DAY))
        .unwrap();
    commit(root.path(), "bob@example.com", now, "fn second() {}\n");
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Incremental).unwrap();
    assert_eq!(db.git_author_events("feature.rs").unwrap().len(), 1);
    assert!(!db.git_authors_complete().unwrap());
    assert!(
        assess_risk(&db, "feature.rs")
            .unwrap()
            .author_concentration
            .is_none()
    );
    git_history_index_with_options(&db, root.path(), &[], IndexMode::Full).unwrap();
    assert_eq!(
        assess_risk(&db, "feature.rs")
            .unwrap()
            .author_concentration
            .unwrap()
            .author_count,
        2
    );
}
