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
fn incremental_after_new_commit_updates_existing_state_hermetically() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "first"]);

    let db = Database::open_in_memory().unwrap();
    let full = git_history_index_with_options(&db, root, &[], IndexMode::Full).unwrap();
    assert_eq!(full.commits_scanned, 1);
    assert_eq!(full.files_tracked, 1);
    let (first_sha, _) = db.get_git_index_state().unwrap().expect("state after full");

    std::fs::write(root.join("a.rs"), "fn a() { let _ = 1; }\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "second"]);

    let incr = git_history_index_with_options(&db, root, &[], IndexMode::Incremental).unwrap();
    assert_eq!(
        incr.commits_scanned, 1,
        "incremental should scan only the commit after the recorded SHA"
    );
    assert_eq!(incr.files_tracked, 2);
    let a = db.git_file("a.rs").unwrap().expect("a.rs row after incr");
    let b = db.git_file("b.rs").unwrap().expect("b.rs row after incr");
    assert_eq!(a.total_commits, 2);
    assert_eq!(b.total_commits, 1);

    let (second_sha, _) = db
        .get_git_index_state()
        .unwrap()
        .expect("state after incremental");
    assert_ne!(second_sha, first_sha);
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
fn incremental_falls_back_to_full_when_history_rewritten() {
    // Documented contract: when the stored SHA is no longer an ancestor of
    // HEAD (rebase, reset+recommit, force-update), incremental must detect
    // non-ancestry and fall back to a full rescan instead of additively
    // updating counters against an abandoned line of history.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "first"]);
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "second adds b"]);

    let db = Database::open_in_memory().unwrap();
    let full = git_history_index_with_options(&db, root, &[], IndexMode::Full).unwrap();
    assert_eq!(full.commits_scanned, 2);
    assert!(db.git_file("b.rs").unwrap().is_some());
    let (stale_sha, _) = db.get_git_index_state().unwrap().expect("state after full");

    // Rewrite history: drop the second commit and commit different content,
    // so the stored SHA still exists as an object but is not an ancestor of
    // the new HEAD.
    run_git(root, &["reset", "--hard", "HEAD~1"]);
    std::fs::write(root.join("a.rs"), "fn a() { let _ = 1; }\n").unwrap();
    std::fs::write(root.join("c.rs"), "fn c() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "rewritten second adds c"]);

    let incr = git_history_index_with_options(&db, root, &[], IndexMode::Incremental).unwrap();

    // (a) Non-ancestry detected → full rescan. The whole rewritten history is
    // scanned (2 commits), not just the delta reachable from HEAD but not the
    // stale SHA (which would be 1). State must move off the stale SHA.
    assert_eq!(
        incr.commits_scanned, 2,
        "fallback must rescan the entire rewritten history"
    );
    let (new_sha, _) = db
        .get_git_index_state()
        .unwrap()
        .expect("state after fallback");
    assert_ne!(
        new_sha, stale_sha,
        "state must be restamped to the new HEAD"
    );

    // The full rescan drops rows from the abandoned line; a buggy additive
    // incremental would have kept b.rs.
    assert!(
        db.git_file("b.rs").unwrap().is_none(),
        "b.rs only exists on the abandoned history line"
    );

    // (b) Counters equal a pristine full scan of the rewritten history.
    let db_fresh = Database::open_in_memory().unwrap();
    let fresh = git_history_index_with_options(&db_fresh, root, &[], IndexMode::Full).unwrap();
    assert_eq!(incr.commits_scanned, fresh.commits_scanned);
    assert_eq!(incr.files_tracked, fresh.files_tracked);
    assert_eq!(incr.co_change_pairs, fresh.co_change_pairs);
    for path in ["a.rs", "c.rs"] {
        let got = db
            .git_file(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} missing after fallback"));
        let want = db_fresh
            .git_file(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} missing from pristine full scan"));
        assert_eq!(got.total_commits, want.total_commits, "{path}");
        assert_eq!(got.fix_count, want.fix_count, "{path}");
        // Churn decays against wall-clock "now" at scan time; the two scans run
        // moments apart so the scores agree to well under a permille.
        assert!(
            (got.churn_score - want.churn_score).abs() < 1e-3,
            "{path}: churn {} vs pristine {}",
            got.churn_score,
            want.churn_score
        );
    }
}

/// Mirrors `DECAY_HALFLIFE_DAYS * SECONDS_PER_DAY` in `git_history::indexer`.
/// The decay-materiality bounds below are tight enough that a drift in the
/// production constant fails this test — deliberate: the composition being
/// asserted is exp(-Δt/τ) with exactly this τ.
const DECAY_TAU_SECS: f64 = 180.0 * 86_400.0;

#[test]
fn full_then_incremental_matches_pristine_full_scan() {
    // Equivalence contract of the incremental decay math: Full over the first
    // commits followed by Incremental over the rest must produce the same
    // per-file stats and co-change weights as one Full pass over everything.
    // The incremental path scales stored weights by exp(-Δt/τ) before adding
    // new-commit deltas, which composes exactly with the full pass's
    // exp(-age/τ) — any drift here is a real decay bug, not test noise.
    //
    // Equivalence alone cannot prove decay ran: the phases execute moments
    // apart, so a regression nulling `decay_git_history_to_now` also passes
    // it (factor ≈ 1 either way). The decay-materiality block below closes
    // that hole: a forced ≥3s stamped gap between the phases plus a file
    // untouched by phase 2 pins the applied factor between two bounds a
    // no-op decay violates.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    let commit_pair = |subject: &str, body_a: &str, body_b: &str| {
        std::fs::write(root.join("a.rs"), body_a).unwrap();
        std::fs::write(root.join("b.rs"), body_b).unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-qm", subject]);
    };

    // Three co-change commits: the (a.rs, b.rs) pair clears the min-count
    // filter (3) within the full part alone.
    commit_pair("feat: one", "fn a() {}\n", "fn b() {}\n");
    commit_pair(
        "feat: two",
        "fn a() { let _ = 1; }\n",
        "fn b() { let _ = 1; }\n",
    );
    commit_pair(
        "feat: three",
        "fn a() { let _ = 2; }\n",
        "fn b() { let _ = 2; }\n",
    );
    // Phase-1-only file: untouched by phase 2, so after the incremental its
    // churn row differs from the full-pass value by exactly the decay factor.
    std::fs::write(root.join("c.rs"), "fn c() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "feat: c only"]);

    let db_incr = Database::open_in_memory().unwrap();
    let full_part = git_history_index_with_options(&db_incr, root, &[], IndexMode::Full).unwrap();
    assert_eq!(full_part.commits_scanned, 4);
    assert_eq!(full_part.co_change_pairs, 1);

    let (_, full_stamp) = db_incr
        .get_git_index_state()
        .unwrap()
        .expect("state after full");
    let c_churn_full = db_incr
        .git_file("c.rs")
        .unwrap()
        .expect("c.rs after full")
        .churn_score;

    // Force a stamped gap of at least 3 seconds between the phases so the
    // incremental's decay factor is bounded away from 1 by more than the
    // ±2s timestamp-truncation skew between unix_now() and unixepoch().
    std::thread::sleep(std::time::Duration::from_millis(3200));

    // One more co-change commit — its delta count (1) is below the min-count
    // filter, so only the pair-exists accumulate branch can surface it — plus
    // a fix commit touching a single file to vary fix_count/total_commits.
    commit_pair(
        "fix: four",
        "fn a() { let _ = 3; }\n",
        "fn b() { let _ = 3; }\n",
    );
    std::fs::write(root.join("a.rs"), "fn a() { let _ = 4; }\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "feat: five"]);

    let incr = git_history_index_with_options(&db_incr, root, &[], IndexMode::Incremental).unwrap();
    assert_eq!(
        incr.commits_scanned, 2,
        "incremental must scan only the two commits after the recorded SHA"
    );

    // Decay materiality: c.rs took no phase-2 deltas, so its stored churn
    // moved only by the decay scale. The applied factor is exp(-Δ'/τ) where
    // Δ' is the real seconds between the two passes' unix_now() calls; the
    // observed stamped gap Δ agrees with Δ' to within ±2s (each endpoint is
    // an independent second-truncation). A nulled decay leaves the row
    // bit-identical (w1 == w0), which the upper bound rejects.
    let (_, incr_stamp) = db_incr
        .get_git_index_state()
        .unwrap()
        .expect("state after incremental");
    let c_churn_incr = db_incr
        .git_file("c.rs")
        .unwrap()
        .expect("c.rs after incremental")
        .churn_score;
    let stamped_gap = (incr_stamp - full_stamp) as f64;
    assert!(
        stamped_gap >= 3.0,
        "the sleep must guarantee a >=3s stamped gap, got {stamped_gap}"
    );
    let upper = c_churn_full * (-(stamped_gap - 2.0) / DECAY_TAU_SECS).exp();
    let lower = c_churn_full * (-(stamped_gap + 2.0) / DECAY_TAU_SECS).exp();
    // Discriminating margin: the no-op outcome (w1 == w0) sits at least
    // w0·(1 - exp(-1s/τ)) ≈ 6.4e-8·w0 above `upper` — eight orders of
    // magnitude beyond f64 rounding on the single multiply decay performs,
    // so the comparison below is numerically meaningful.
    assert!(
        c_churn_full - upper > c_churn_full * 1e-8,
        "bound must be distinguishable from the undecayed value: w0={c_churn_full} upper={upper}"
    );
    assert!(
        c_churn_incr <= upper,
        "incremental must decay stored weights across the phase gap: \
         w0={c_churn_full} w1={c_churn_incr} upper={upper} gap={stamped_gap}s"
    );
    assert!(
        c_churn_incr >= lower,
        "decay overshoot: w1={c_churn_incr} lower={lower} gap={stamped_gap}s"
    );

    let db_full = Database::open_in_memory().unwrap();
    git_history_index_with_options(&db_full, root, &[], IndexMode::Full).unwrap();

    for path in ["a.rs", "b.rs", "c.rs"] {
        let got = db_incr
            .git_file(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} missing after incremental"));
        let want = db_full
            .git_file(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} missing from pristine full scan"));
        assert_eq!(
            got.total_commits, want.total_commits,
            "{path}: total_commits"
        );
        assert_eq!(got.fix_count, want.fix_count, "{path}: fix_count");
        // Both scans decay against wall-clock "now" moments apart; agreement
        // to 1e-3 proves the scale-then-add composition is exact.
        assert!(
            (got.churn_score - want.churn_score).abs() < 1e-3,
            "{path}: churn {} vs pristine {}",
            got.churn_score,
            want.churn_score
        );
    }

    let got_pairs = db_incr.co_changes_for("a.rs", 10).unwrap();
    let want_pairs = db_full.co_changes_for("a.rs", 10).unwrap();
    assert_eq!(
        got_pairs.len(),
        1,
        "one co-change pair expected: {got_pairs:?}"
    );
    assert_eq!(want_pairs.len(), 1);
    assert_eq!(got_pairs[0].file, "b.rs");
    // Count 4 proves the sub-threshold incremental delta accumulated onto the
    // existing pair rather than being dropped by the min-count filter.
    assert_eq!(
        got_pairs[0].count, 4,
        "pair-exists accumulate branch must fire"
    );
    assert_eq!(got_pairs[0].count, want_pairs[0].count);
    assert!(
        (got_pairs[0].weight - want_pairs[0].weight).abs() < 1e-3,
        "co-change weight {} vs pristine {}",
        got_pairs[0].weight,
        want_pairs[0].weight
    );
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
