//! End-to-end smoke test for git_history_index. Points at the CodeSage repo itself
//! (parent of CARGO_MANIFEST_DIR) rather than building a synthetic fixture. This trades
//! tight behavioral assertions (specific weights, specific top hotspots) for a cheaper
//! integration probe that catches:
//!   - subprocess pipeline breakage (git not in PATH, format string wrong)
//!   - parse-shape regressions (numstat / rename / binary handling)
//!   - storage/accessor wiring (populated rows come back through co_changes_for etc.)
//!
//! Values drift on every commit, so assertions stay loose. For exact-value coverage,
//! see the unit tests in git_history::tests and the seeded-DB tests in risk_test.

use std::path::PathBuf;

use codesage_graph::{IndexMode, find_coupling, git_history_index, git_history_index_with_options};
use codesage_storage::Database;

fn codesage_repo_root() -> PathBuf {
    // crates/graph/Cargo.toml -> crates/graph -> crates -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn indexer_runs_against_codesage_repo_and_populates_tables() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        eprintln!("skipping: codesage repo has no .git/ (sandbox?); path={}", root.display());
        return;
    }

    let db = Database::open_in_memory().unwrap();
    let stats = git_history_index(&db, &root).expect("git-index on codesage repo must succeed");

    // Loose structural assertions. Exact numbers drift; these just prove the pipe is alive.
    assert!(stats.commits_scanned > 0, "expected > 0 commits, got {}", stats.commits_scanned);
    assert!(stats.files_tracked > 0, "expected > 0 files tracked, got {}", stats.files_tracked);
    // Even tiny histories should produce some qualifying pairs (min_count=3), but a brand-new
    // repo could have zero. Allow either.

    // Every git_files row should have plausible bounds.
    let cargo_toml = db.git_file("Cargo.toml").unwrap();
    if let Some(row) = cargo_toml {
        assert!(row.total_commits >= 1);
        assert!(row.churn_score >= 0.0);
        assert!(row.fix_count <= row.total_commits);
    }

    // Pick a file that's known to exist in this repo and confirm coupling lookups work
    // without panicking. Result can be empty if the file is too new or isolated.
    let _ = find_coupling(&db, "crates/storage/src/schema.rs", 5)
        .expect("find_coupling must return Ok even when empty");
}

#[test]
fn own_repo_indexer_is_idempotent() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        eprintln!("skipping: codesage repo has no .git/");
        return;
    }
    let db = Database::open_in_memory().unwrap();
    let first = git_history_index(&db, &root).unwrap();
    let second = git_history_index(&db, &root).unwrap();
    // Same input -> same output (commits_scanned is driven by decay-time, which uses
    // unix_now; it can shift by a microsecond on re-run but the counts are commit-level
    // and unchanged).
    assert_eq!(first.commits_scanned, second.commits_scanned);
    assert_eq!(first.files_tracked, second.files_tracked);
    assert_eq!(first.co_change_pairs, second.co_change_pairs);
}

#[test]
fn incremental_after_full_is_noop_when_head_unchanged() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        return;
    }
    let db = Database::open_in_memory().unwrap();
    // Full pass: stamps state with HEAD.
    let full = git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    assert!(full.files_tracked > 0);

    // Incremental with HEAD unchanged: short-circuits and reports zeros.
    let incr = git_history_index_with_options(&db, &root, &[], IndexMode::Incremental).unwrap();
    assert_eq!(incr.commits_scanned, 0);
    assert_eq!(incr.files_tracked, 0);
    assert_eq!(incr.co_change_pairs, 0);

    // State must still point at the HEAD SHA.
    let state = db.get_git_index_state().unwrap();
    assert!(state.is_some(), "state should still be present after no-op incremental");
}

#[test]
fn incremental_without_state_falls_back_to_full() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        return;
    }
    // Fresh DB, no state recorded. Asking for Incremental directly should still produce
    // a populated index (we fall back to Full instead of failing).
    let db = Database::open_in_memory().unwrap();
    let stats = git_history_index_with_options(&db, &root, &[], IndexMode::Incremental).unwrap();
    assert!(stats.commits_scanned > 0);
    assert!(stats.files_tracked > 0);
    // State must now be populated so subsequent calls are truly incremental.
    assert!(db.get_git_index_state().unwrap().is_some());
}

#[test]
fn auto_mode_matches_full_on_fresh_db() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        return;
    }
    let db_full = Database::open_in_memory().unwrap();
    let full = git_history_index_with_options(&db_full, &root, &[], IndexMode::Full).unwrap();

    let db_auto = Database::open_in_memory().unwrap();
    let auto = git_history_index_with_options(&db_auto, &root, &[], IndexMode::Auto).unwrap();

    // Auto with no state should behave like Full.
    assert_eq!(full.commits_scanned, auto.commits_scanned);
    assert_eq!(full.files_tracked, auto.files_tracked);
    assert_eq!(full.co_change_pairs, auto.co_change_pairs);
}
