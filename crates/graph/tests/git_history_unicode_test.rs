//! Git paths are NUL-delimited, not Git's human-readable quoted display names.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use codesage_graph::{
    IndexMode, changed_files_since, feature_touched_since, git_history_index,
    git_history_index_with_options,
};
use codesage_protocol::{FeatureFileRef, FeatureFileRole};
use codesage_storage::Database;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "paths@example.invalid"]);
    git(root, &["config", "user.name", "Path regression"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir(root.join(".git/disabled-hooks")).unwrap();
    git(root, &["config", "core.hooksPath", ".git/disabled-hooks"]);
    git(root, &["config", "core.quotePath", "true"]);
    git(root, &["config", "diff.renames", "true"]);
    git(root, &["commit", "--allow-empty", "-qm", "feat: baseline"]);
}

#[test]
fn git_history_preserves_unicode_paths_and_rename_destinations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut paths = vec!["ascii.rs", "日本語.rs", "école.rs", "with space.rs"];
    // These bytes are legal filename bytes on Unix, but not on Windows.
    #[cfg(unix)]
    paths.extend([
        "tab\tname.rs",
        "line\nname.rs",
        "\nleading-newline.rs",
        " padded.rs ",
        "back\\slash.rs",
        "literal => arrow.rs",
        "literal{old => new}.rs",
        "quote\"name.rs",
    ]);
    for (index, path) in paths.iter().enumerate() {
        std::fs::write(root.join(path), format!("fn file_{index}() {{}}\n")).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "feat: unusual paths"]);

    let changed = changed_files_since(root, "HEAD~1").unwrap();
    assert_eq!(
        changed,
        paths.iter().map(|path| (*path).to_owned()).collect()
    );
    let db = Database::open_in_memory().unwrap();
    git_history_index(&db, root).unwrap();
    for path in &paths {
        let history = db
            .git_file(path)
            .unwrap()
            .unwrap_or_else(|| panic!("missing {path:?}"));
        assert_eq!(history.total_commits, 1, "{path:?}");
        assert!(feature_touched_since(
            &[FeatureFileRef {
                path: (*path).to_owned(),
                role: FeatureFileRole::Owned,
                reason: None,
            }],
            &changed,
        ));
    }

    let old = "日本語.rs";
    #[cfg(unix)]
    let new = "改名\t\n => {new}.rs";
    #[cfg(not(unix))]
    let new = "改名 new.rs";
    git(root, &["mv", "--", old, new]);
    std::fs::write(root.join("ascii.rs"), "fn ascii_modified() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "fix: rename and modify"]);

    let changed = changed_files_since(root, "HEAD~1").unwrap();
    assert_eq!(
        changed,
        HashSet::from([old.to_owned(), new.to_owned(), "ascii.rs".to_owned()])
    );
    assert!(feature_touched_since(
        &[FeatureFileRef {
            path: new.to_owned(),
            role: FeatureFileRole::Entry,
            reason: None,
        }],
        &changed,
    ));
    git_history_index_with_options(&db, root, &[], IndexMode::Incremental).unwrap();
    let full = Database::open_in_memory().unwrap();
    git_history_index(&full, root).unwrap();
    for path in paths.into_iter().chain(std::iter::once(new)) {
        let expected = if path == "ascii.rs" { 2 } else { 1 };
        assert_eq!(
            db.git_file(path).unwrap().unwrap().total_commits,
            expected,
            "{path:?}"
        );
        assert_eq!(
            full.git_file(path).unwrap().unwrap().total_commits,
            expected,
            "{path:?}"
        );
    }
}
