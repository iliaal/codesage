//! Drift reporting over real Git histories: the commit count and the indexed
//! files a reindex would actually change are different numbers.

use std::path::Path;
use std::process::Command;

use codesage_graph::drift::{DriftKind, check_drift};
use codesage_parser::discover::DEFAULT_EXCLUDE_PATTERNS;
use codesage_storage::Database;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Isolate the fixture from the caller's signing and hook configuration.
fn init_hermetic_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "drift@example.invalid"]);
    git(root, &["config", "user.name", "Drift"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join(".git/disabled-hooks")).unwrap();
    git(root, &["config", "core.hooksPath", ".git/disabled-hooks"]);
}

fn commit(root: &Path, files: &[(&str, &str)], message: &str) -> String {
    for (rel, body) in files {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, body).unwrap();
    }
    // The open index.db under `.codesage/` is never part of the fixture history.
    git(root, &["add", "-A", "--", ":(exclude).codesage"]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// Index the working tree under the default excludes and stamp the index with
/// HEAD, the way `codesage index` does.
fn index_at_head(root: &Path) -> Database {
    std::fs::create_dir_all(root.join(".codesage")).unwrap();
    let db = Database::open(&root.join(".codesage/index.db")).unwrap();
    let excludes: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    codesage_graph::full_index(root, &db, &excludes, false).unwrap();
    db.set_structural_index_state(&git(root, &["rev-parse", "HEAD"]))
        .unwrap();
    db
}

#[test]
fn commits_touching_only_unindexed_paths_report_no_affected_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(
        root,
        &[
            ("src/lib.rs", "pub fn keep() {}\n"),
            ("CHANGELOG.md", "# 0.1.0\n"),
            ("Cargo.lock", "version = 3\n"),
        ],
        "base",
    );
    let db = index_at_head(root);

    commit(root, &[("CHANGELOG.md", "# 0.2.0\n")], "changelog");
    commit(root, &[("Cargo.lock", "version = 4\n")], "lockfile");

    let report = check_drift(root, &db);

    assert_eq!(report.kind, DriftKind::BehindHead);
    assert_eq!(report.commits_between, Some(2));
    assert_eq!(report.indexed_files_behind, Some(0));
    assert!(report.indexed_files_behind_sample.is_empty());
    assert!(!report.indexed_files_behind_bounded);
    assert!(
        !report.recommends_reindex(),
        "a reindex would re-parse nothing"
    );
    let summary = report.summary();
    assert!(
        summary.contains("2 commits behind HEAD, 0 indexed files affected"),
        "{summary}"
    );
    assert!(!summary.contains('⚠'), "{summary}");
}

#[test]
fn a_commit_changing_an_indexed_file_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(
        root,
        &[
            ("src/lib.rs", "pub fn keep() {}\n"),
            ("CHANGELOG.md", "# 0.1.0\n"),
        ],
        "base",
    );
    let db = index_at_head(root);

    commit(root, &[("CHANGELOG.md", "# 0.2.0\n")], "changelog");
    commit(root, &[("src/lib.rs", "pub fn edited() {}\n")], "edit");

    let report = check_drift(root, &db);

    assert_eq!(report.commits_between, Some(2));
    assert_eq!(report.indexed_files_behind, Some(1));
    assert_eq!(report.indexed_files_behind_sample, vec!["src/lib.rs"]);
    assert!(report.recommends_reindex());
    let summary = report.summary();
    assert!(
        summary.contains("1 indexed file affected (src/lib.rs)"),
        "{summary}"
    );
    assert!(summary.starts_with('⚠'), "{summary}");
}

#[test]
fn renaming_an_indexed_file_counts_the_path_the_index_still_holds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(root, &[("src/old.rs", "pub fn moved() {}\n")], "base");
    let db = index_at_head(root);

    git(root, &["mv", "src/old.rs", "src/new.rs"]);
    git(root, &["commit", "-q", "-m", "rename"]);

    let report = check_drift(root, &db);

    // Rename detection would name only the destination, leaving the indexed
    // path out of the candidate set. Both endpoints count: the indexed path
    // HEAD dropped and the source file HEAD added that the index has not seen.
    assert_eq!(report.indexed_files_behind, Some(2));
    assert_eq!(
        report.indexed_files_behind_sample,
        vec!["src/new.rs", "src/old.rs"]
    );
    assert!(report.recommends_reindex());
}

#[test]
fn an_index_matching_head_reports_no_affected_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(root, &[("src/lib.rs", "pub fn keep() {}\n")], "base");
    let db = index_at_head(root);

    let report = check_drift(root, &db);

    assert_eq!(report.kind, DriftKind::Fresh);
    assert_eq!(report.indexed_files_behind, Some(0));
    assert!(!report.recommends_reindex());
}

#[test]
fn a_commit_adding_a_supported_source_file_recommends_a_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(root, &[("src/lib.rs", "pub fn keep() {}\n")], "base");
    let db = index_at_head(root);

    commit(
        root,
        &[("src/new.rs", "pub fn added() {}\n")],
        "add a module",
    );

    let report = check_drift(root, &db);

    assert_eq!(report.kind, DriftKind::BehindHead);
    assert_eq!(report.commits_between, Some(1));
    assert_eq!(
        report.indexed_files_behind,
        Some(1),
        "an added source file is exactly what a reindex would parse"
    );
    assert_eq!(report.indexed_files_behind_sample, vec!["src/new.rs"]);
    assert!(!report.indexed_files_behind_bounded);
    assert!(report.recommends_reindex());
    let summary = report.summary();
    assert!(
        summary.contains("1 indexed file affected (src/new.rs)"),
        "{summary}"
    );
    assert!(summary.starts_with('⚠'), "{summary}");
}

#[test]
fn added_unsupported_hidden_and_excluded_paths_do_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(root, &[("src/lib.rs", "pub fn keep() {}\n")], "base");
    let db = index_at_head(root);
    std::fs::write(
        root.join(".codesage/config.toml"),
        "[index]\nexclude_patterns = [\"generated/**\"]\n",
    )
    .unwrap();

    commit(
        root,
        &[
            ("NOTES.md", "# notes\n"),
            (".hidden/tool.rs", "fn hidden() {}\n"),
            ("vendor/dep.rs", "fn vendored() {}\n"),
            ("generated/out.rs", "fn generated() {}\n"),
        ],
        "add non-indexable paths",
    );

    let report = check_drift(root, &db);

    assert_eq!(report.kind, DriftKind::BehindHead);
    assert_eq!(report.indexed_files_behind, Some(0), "{report:?}");
    assert!(report.indexed_files_behind_sample.is_empty());
    assert!(!report.indexed_files_behind_bounded);
    assert!(
        !report.recommends_reindex(),
        "none of these paths would be picked up by `codesage index`"
    );
}

#[cfg(unix)]
#[test]
fn committed_gitignored_files_and_symlinks_do_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);
    commit(
        root,
        &[
            ("src/lib.rs", "pub fn keep() {}\n"),
            (".gitignore", "generated/\n"),
        ],
        "base",
    );
    let db = index_at_head(root);

    // Discovery honors `.gitignore` and takes regular files only, so neither
    // path is ever indexed; counting them would leave `files_behind` at 2
    // after every reindex.
    std::fs::create_dir_all(root.join("generated")).unwrap();
    std::fs::write(root.join("generated/out.rs"), "fn generated() {}\n").unwrap();
    git(root, &["add", "-f", "generated/out.rs"]);
    std::os::unix::fs::symlink("lib.rs", root.join("src/link.rs")).unwrap();
    git(root, &["add", "src/link.rs"]);
    git(root, &["commit", "-q", "-m", "add undiscoverable paths"]);

    let report = check_drift(root, &db);

    assert_eq!(report.kind, DriftKind::BehindHead);
    assert_eq!(report.commits_between, Some(1));
    assert_eq!(report.indexed_files_behind, Some(0), "{report:?}");
    assert!(report.indexed_files_behind_sample.is_empty());
    assert!(!report.indexed_files_behind_bounded);
    assert!(!report.recommends_reindex());

    // The same commit plus a real source file still names exactly that file.
    commit(
        root,
        &[("src/new.rs", "pub fn added() {}\n")],
        "add a module",
    );
    let report = check_drift(root, &db);
    assert_eq!(report.indexed_files_behind, Some(1), "{report:?}");
    assert_eq!(report.indexed_files_behind_sample, vec!["src/new.rs"]);
}
