use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use codesage_graph::{IndexMode, find_coupling, git_history_index_with_options};
use codesage_storage::Database;

fn git(root: &Path, args: &[&str], timestamp: u64) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_AUTHOR_NAME", "Coupling test")
        .env("GIT_AUTHOR_EMAIL", "coupling@example.invalid")
        .env("GIT_COMMITTER_NAME", "Coupling test")
        .env("GIT_COMMITTER_EMAIL", "coupling@example.invalid")
        .env("GIT_AUTHOR_DATE", format!("{timestamp} +0000"))
        .env("GIT_COMMITTER_DATE", format!("{timestamp} +0000"))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}: {:?}", args, output);
}

#[test]
fn single_commit_passes_retain_evidence_until_visible_and_match_full_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    git(root, &["init", "-q"], now);
    let incremental = Database::open_in_memory().unwrap();
    for count in 1..=5 {
        let timestamp = now - (6 - count) * 40 * 86_400;
        for file in ["a.rs", "b.rs"] {
            std::fs::write(root.join(file), format!("fn f() {{ let n = {count}; }}\n")).unwrap();
        }
        git(root, &["add", "a.rs", "b.rs"], timestamp);
        git(root, &["commit", "-qm", "change both files"], timestamp);
        let stats =
            git_history_index_with_options(&incremental, root, &[], IndexMode::Auto).unwrap();
        assert_eq!(stats.commits_scanned, 1);
        assert_eq!(stats.co_change_pairs, usize::from(count >= 3));
        let report = find_coupling(&incremental, "a.rs", 10).unwrap();
        if count < 3 {
            assert!(report.coupled.is_empty());
        } else {
            assert_eq!(report.coupled.len(), 1);
            assert_eq!(report.coupled[0].count, count as u32);
            assert_eq!(report.coupled[0].confidence, 1.0);
        }
    }
    let full = Database::open_in_memory().unwrap();
    git_history_index_with_options(&full, root, &[], IndexMode::Full).unwrap();
    let actual = incremental.co_changes_for("a.rs", 10).unwrap();
    let expected = full.co_changes_for("a.rs", 10).unwrap();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].count, expected[0].count);
    assert_eq!(actual[0].window_mask, expected[0].window_mask);
    assert_eq!(actual[0].first_observed_at, expected[0].first_observed_at);
    assert_eq!(actual[0].last_observed_at, expected[0].last_observed_at);
    assert!((actual[0].weight - expected[0].weight).abs() < 1e-6);
}

#[test]
fn empty_coupling_does_not_infer_isolation() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_git_file("a.rs", 1.0, 0, 5, None).unwrap();
    let report = find_coupling(&db, "a.rs", 10).unwrap();
    let note = report.note.unwrap();
    assert!(!note.contains("typically changes in isolation"));
    assert!(note.contains("insufficient"));
}
