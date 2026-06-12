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
use std::process::Command;

use codesage_graph::{
    IndexMode, changed_files_since, find_coupling, git_history_index,
    git_history_index_with_options,
};
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

fn init_hermetic_repo(root: &std::path::Path) {
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "review@example.invalid"]);
    run_git(root, &["config", "user.name", "Review"]);
    // Ignore ambient signing/hooks config so temp-repo tests don't fail on
    // machines with global commit.gpgsign or custom core.hooksPath.
    run_git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join(".git/disabled-hooks")).unwrap();
    run_git(root, &["config", "core.hooksPath", ".git/disabled-hooks"]);
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git command starts");
    assert!(status.success(), "git {:?} failed", args);
}

#[test]
#[cfg(unix)]
fn init_hermetic_repo_ignores_hooks_left_in_git_hooks_dir() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    let hook = root.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 42\n").unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(&hook, perms).unwrap();

    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "commit ignores disabled hook"]);
}

#[test]
fn indexer_runs_against_codesage_repo_and_populates_tables() {
    let root = codesage_repo_root();
    if !root.join(".git").exists() {
        eprintln!(
            "skipping: codesage repo has no .git/ (sandbox?); path={}",
            root.display()
        );
        return;
    }

    let db = Database::open_in_memory().unwrap();
    let stats = git_history_index(&db, &root).expect("git-index on codesage repo must succeed");

    // Loose structural assertions. Exact numbers drift; these just prove the pipe is alive.
    assert!(
        stats.commits_scanned > 0,
        "expected > 0 commits, got {}",
        stats.commits_scanned
    );
    assert!(
        stats.files_tracked > 0,
        "expected > 0 files tracked, got {}",
        stats.files_tracked
    );
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
    assert!(
        state.is_some(),
        "state should still be present after no-op incremental"
    );
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

#[test]
fn extra_excludes_skip_git_history_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    std::fs::create_dir_all(root.join("keep")).unwrap();
    std::fs::create_dir_all(root.join("skip")).unwrap();
    std::fs::write(root.join("keep/a.rs"), "fn keep() {}\n").unwrap();
    std::fs::write(root.join("skip/b.rs"), "fn skip() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "initial"]);

    let db = Database::open_in_memory().unwrap();
    let excludes = vec!["skip/**".to_string()];
    let stats = git_history_index_with_options(&db, root, &excludes, IndexMode::Full).unwrap();

    assert_eq!(stats.files_tracked, 1);
    assert!(db.git_file("keep/a.rs").unwrap().is_some());
    assert!(db.git_file("skip/b.rs").unwrap().is_none());
}

#[test]
fn changed_files_since_returns_only_files_touched_after_ref() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    // Commit 1: two files, both untouched after this point except `changed.rs`.
    std::fs::write(root.join("stable.rs"), "fn stable() {}\n").unwrap();
    std::fs::write(root.join("changed.rs"), "fn before() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "first"]);

    // Commit 2: modify one existing file, add a new one. `stable.rs` is left alone.
    std::fs::write(root.join("changed.rs"), "fn after() {}\n").unwrap();
    std::fs::write(root.join("added.rs"), "fn added() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "second"]);

    let changed = changed_files_since(root, "HEAD~1").unwrap();
    assert!(
        changed.contains("changed.rs"),
        "modified file missing: {changed:?}"
    );
    assert!(
        changed.contains("added.rs"),
        "added file missing: {changed:?}"
    );
    assert!(
        !changed.contains("stable.rs"),
        "untouched file should not appear: {changed:?}"
    );
    assert_eq!(changed.len(), 2, "exactly two changed files: {changed:?}");
}

#[test]
fn changed_files_since_errors_on_unknown_ref() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "only"]);

    // An unresolvable ref must surface as an error, not a silent empty set —
    // otherwise `--since typo` would look like "nothing changed".
    assert!(changed_files_since(root, "no-such-ref").is_err());
}
