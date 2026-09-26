//! User `log.*` configuration must not change what `git-index` reads.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use codesage_graph::{IndexMode, find_coupling, git_history_index_with_options};
use codesage_protocol::GitIndexStats;
use codesage_storage::Database;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository whose commits carry real SSH signatures that verify, so
/// `log.showSignature` prints a "Good signature" line per commit.
fn signed_repo(root: &Path, keys: &Path) {
    let key = keys.join("signing_key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "fixture", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen is required to build the signed-commit fixture");
    assert!(status.success(), "ssh-keygen failed");
    let public = std::fs::read_to_string(keys.join("signing_key.pub")).unwrap();
    let allowed = keys.join("allowed_signers");
    std::fs::write(&allowed, format!("sig@example.invalid {public}")).unwrap();

    git(root, &["init", "-q"]);
    std::fs::create_dir_all(root.join(".git/disabled-hooks")).unwrap();
    for (name, value) in [
        ("user.email", "sig@example.invalid"),
        ("user.name", "Signed"),
        ("core.hooksPath", ".git/disabled-hooks"),
        ("gpg.format", "ssh"),
        ("user.signingkey", key.to_str().unwrap()),
        ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
        ("commit.gpgsign", "true"),
    ] {
        git(root, &["config", name, value]);
    }
}

fn commit_pair(root: &Path, tag: u32) {
    for file in ["a.rs", "b.rs"] {
        std::fs::write(root.join(file), format!("fn f() {{ let _ = {tag}; }}\n")).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", &format!("feat: {tag}")]);
}

fn show_signature(root: &Path, on: bool) {
    git(
        root,
        &[
            "config",
            "log.showSignature",
            if on { "true" } else { "false" },
        ],
    );
}

fn index(root: &Path, db: &Database, mode: IndexMode) -> GitIndexStats {
    git_history_index_with_options(db, root, &[], mode).unwrap()
}

fn stats_tuple(stats: &GitIndexStats) -> (usize, usize, usize) {
    (
        stats.commits_scanned,
        stats.files_tracked,
        stats.co_change_pairs,
    )
}

fn assert_same_rows(got: &Database, want: &Database) {
    for path in ["a.rs", "b.rs"] {
        let g = got
            .git_file(path)
            .unwrap()
            .expect("row under showSignature");
        let w = want.git_file(path).unwrap().expect("control row");
        assert_eq!(g.total_commits, w.total_commits, "{path}");
        assert_eq!(g.fix_count, w.fix_count, "{path}");
        assert_eq!(g.last_commit_at, w.last_commit_at, "{path}");
        assert!((g.churn_score - w.churn_score).abs() < 1e-9, "{path}");
    }
    let g = got.co_changes_for("a.rs", 10).unwrap();
    let w = want.co_changes_for("a.rs", 10).unwrap();
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0].file, w[0].file);
    assert_eq!(g[0].count, w[0].count);
    assert_eq!(g[0].window_mask, w[0].window_mask);
    assert!((g[0].weight - w[0].weight).abs() < 1e-9);
}

#[test]
fn show_signature_config_does_not_change_full_or_incremental_history() {
    let dir = tempfile::tempdir().unwrap();
    let keys = tempfile::tempdir().unwrap();
    let root = dir.path();
    signed_repo(root, keys.path());
    for tag in 1..=4 {
        commit_pair(root, tag);
    }

    let control = Database::open_in_memory().unwrap();
    let want = index(root, &control, IndexMode::Full);
    assert_eq!(stats_tuple(&want), (4, 2, 1));

    show_signature(root, true);
    // The fixture must actually make plain `git log` print signature text.
    let raw = git(root, &["log", "-1", "--format=%ct"]);
    assert!(
        raw.contains("Good \"git\" signature"),
        "fixture does not force signature output: {raw:?}"
    );

    let signed = Database::open_in_memory().unwrap();
    let got = index(root, &signed, IndexMode::Full);
    assert_eq!(stats_tuple(&got), stats_tuple(&want), "--full");
    assert_same_rows(&signed, &control);
    let anchor = signed.git_history_anchor().unwrap().expect("anchor");
    assert_eq!(anchor.0, "HEAD", "HEAD's date must be read, not guessed");

    commit_pair(root, 5);
    let incr = index(root, &signed, IndexMode::Incremental);
    assert_eq!(stats_tuple(&incr), (1, 2, 1), "--incremental");

    show_signature(root, false);
    let pristine = Database::open_in_memory().unwrap();
    index(root, &pristine, IndexMode::Full);
    assert_same_rows(&signed, &pristine);
    assert_eq!(signed.git_file("a.rs").unwrap().unwrap().total_commits, 5);
    let report = find_coupling(&signed, "b.rs", 10).unwrap();
    assert_eq!(report.coupled.len(), 1);
    assert_eq!(report.coupled[0].count, 5);
}

#[test]
fn show_root_false_still_counts_the_root_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::create_dir_all(root.join(".git/disabled-hooks")).unwrap();
    for (name, value) in [
        ("user.email", "root@example.invalid"),
        ("user.name", "Root"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ".git/disabled-hooks"),
        ("log.showRoot", "false"),
    ] {
        git(root, &["config", name, value]);
    }
    commit_pair(root, 1);
    commit_pair(root, 2);
    // The fixture's config must actually hide the root commit's numstat.
    let raw = git(root, &["log", "--numstat", "--format=%s"]);
    assert_eq!(raw.matches("a.rs").count(), 1, "{raw:?}");

    let db = Database::open_in_memory().unwrap();
    let stats = index(root, &db, IndexMode::Full);
    assert_eq!(stats_tuple(&stats), (2, 2, 0));
    assert_eq!(db.git_file("a.rs").unwrap().unwrap().total_commits, 2);
}
