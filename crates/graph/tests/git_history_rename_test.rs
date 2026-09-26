//! Rename continuity: history recorded before a `git mv` stays with the file.

use std::path::Path;
use std::process::Command;

use codesage_graph::{IndexMode, find_coupling, git_history_index_with_options};
use codesage_protocol::GitIndexStats;
use codesage_storage::Database;

const DAY: i64 = 86_400;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn git_at(root: &Path, args: &[&str], ts: i64) -> String {
    let date = format!("{ts} +0000");
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository whose commits land one day apart, starting 100 days ago.
struct Repo {
    dir: tempfile::TempDir,
    next_ts: i64,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Self {
            dir,
            next_ts: unix_now() - 100 * DAY,
        };
        repo.git(&["init", "-q"]);
        std::fs::create_dir_all(repo.root().join(".git/disabled-hooks")).unwrap();
        for (name, value) in [
            ("user.email", "rename@example.invalid"),
            ("user.name", "Rename"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/disabled-hooks"),
            // Detection must not depend on the user's rename configuration.
            ("diff.renames", "false"),
        ] {
            repo.git(&["config", name, value]);
        }
        repo
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        git_at(self.root(), args, self.next_ts)
    }

    fn write(&self, path: &str, body: &str) {
        let full = self.root().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    fn mv(&self, from: &str, to: &str) {
        if let Some(parent) = Path::new(to).parent() {
            std::fs::create_dir_all(self.root().join(parent)).unwrap();
        }
        self.git(&["mv", from, to]);
    }

    fn commit(&mut self, subject: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", subject]);
        self.next_ts += DAY;
    }

    /// Rewrite each file with a body unique to `tag`, then commit.
    fn edit(&mut self, files: &[&str], tag: u32) {
        for file in files {
            self.write(file, &body(file, tag));
        }
        self.commit(&format!("feat: edit {tag}"));
    }

    fn index(&self, db: &Database, mode: IndexMode) -> GitIndexStats {
        self.index_excluding(db, mode, &[])
    }

    fn index_excluding(&self, db: &Database, mode: IndexMode, excludes: &[&str]) -> GitIndexStats {
        let excludes: Vec<String> = excludes.iter().map(|s| (*s).to_owned()).collect();
        git_history_index_with_options(db, self.root(), &excludes, mode).unwrap()
    }

    fn full(&self) -> Database {
        let db = Database::open_in_memory().unwrap();
        self.index(&db, IndexMode::Full);
        db
    }
}

/// Twenty distinct lines so rename detection sees an unmodified move as R100
/// and two files never look alike.
fn body(file: &str, tag: u32) -> String {
    (0..20)
        .map(|i| {
            format!(
                "fn {}_{i}() {{ let _ = {tag}; }}\n",
                file.replace(['/', '.'], "_")
            )
        })
        .collect()
}

fn commits(db: &Database, path: &str) -> Option<u32> {
    db.git_file(path).unwrap().map(|row| row.total_commits)
}

/// Every stored column for `paths`, compared between two indexes.
fn assert_same_history(got: &Database, want: &Database, paths: &[&str]) {
    for path in paths {
        let g = got.git_file(path).unwrap();
        let w = want.git_file(path).unwrap();
        assert_eq!(g.is_some(), w.is_some(), "{path}: row presence");
        if let (Some(g), Some(w)) = (g, w) {
            assert_eq!(g.total_commits, w.total_commits, "{path}: total_commits");
            assert_eq!(g.fix_count, w.fix_count, "{path}: fix_count");
            assert_eq!(g.last_commit_at, w.last_commit_at, "{path}: last_commit_at");
            assert!(
                (g.churn_score - w.churn_score).abs() < 1e-9,
                "{path}: churn {} vs {}",
                g.churn_score,
                w.churn_score
            );
        }
        let g = got.co_changes_for(path, 50).unwrap();
        let w = want.co_changes_for(path, 50).unwrap();
        let key = |rows: &[codesage_storage::db::CoChangeRow]| {
            rows.iter()
                .map(|r| {
                    (
                        r.file.clone(),
                        r.count,
                        r.window_mask,
                        r.first_observed_at,
                        r.last_observed_at,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&g), key(&w), "{path}: co-change pairs");
        for (g, w) in g.iter().zip(&w) {
            assert!(
                (g.weight - w.weight).abs() < 1e-9,
                "{path}: pair weight {} vs {}",
                g.weight,
                w.weight
            );
        }
        assert_eq!(
            got.git_author_events(path).unwrap(),
            want.git_author_events(path).unwrap(),
            "{path}: author events"
        );
    }
}

/// Four co-change commits on a.rs + b.rs, then `git mv a.rs c.rs`.
fn renprobe() -> Repo {
    let mut repo = Repo::new();
    for tag in 1..=4 {
        repo.edit(&["a.rs", "b.rs"], tag);
    }
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: rename a to c");
    repo
}

#[test]
fn renamed_file_keeps_its_history_under_the_new_path() {
    let repo = renprobe();
    let db = Database::open_in_memory().unwrap();
    let stats = repo.index(&db, IndexMode::Full);
    assert_eq!(stats.commits_scanned, 5);
    assert_eq!(stats.files_tracked, 2, "b.rs and c.rs only");
    assert_eq!(stats.co_change_pairs, 1);

    assert_eq!(commits(&db, "a.rs"), None, "the dead path keeps no row");
    assert_eq!(commits(&db, "b.rs"), Some(4));
    assert_eq!(commits(&db, "c.rs"), Some(5), "4 edits plus the rename");
    assert!(db.git_author_events("a.rs").unwrap().is_empty());
    assert_eq!(db.git_author_events("c.rs").unwrap().len(), 5);

    let report = find_coupling(&db, "b.rs", 10).unwrap();
    assert_eq!(report.coupled.len(), 1, "{report:?}");
    assert_eq!(report.coupled[0].file, "c.rs");
    assert_eq!(report.coupled[0].count, 4);
    assert_eq!(report.coupled[0].p_cochange, 1.0);
    assert!(
        (report.coupled[0].p_reverse - 0.8).abs() < 1e-6,
        "c.rs has five commits, four shared: {}",
        report.coupled[0].p_reverse
    );
    assert!(find_coupling(&db, "a.rs", 10).unwrap().coupled.is_empty());
}

#[test]
fn incremental_pass_spanning_a_rename_matches_a_full_scan() {
    let mut repo = Repo::new();
    for tag in 1..=4 {
        repo.edit(&["a.rs", "b.rs"], tag);
    }
    let incremental = repo.full();
    assert_eq!(commits(&incremental, "a.rs"), Some(4));

    // The rename and later edits under the new name land in one range.
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: rename a to c");
    repo.edit(&["b.rs", "c.rs"], 5);
    let stats = repo.index(&incremental, IndexMode::Incremental);
    assert_eq!(
        stats.commits_scanned, 2,
        "only the range after the last pass"
    );

    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "b.rs", "c.rs"]);
    assert_eq!(commits(&incremental, "a.rs"), None);
    assert_eq!(commits(&incremental, "c.rs"), Some(6));
    let pairs = incremental.co_changes_for("b.rs", 10).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("c.rs", 5));
}

#[test]
fn rename_chain_follows_every_hop_under_both_modes() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "peer.rs"], tag);
    }
    let incremental = repo.full();

    repo.mv("a.rs", "b.rs");
    repo.commit("feat: a to b");
    repo.edit(&["b.rs", "peer.rs"], 4);
    repo.index(&incremental, IndexMode::Incremental);
    assert_eq!(commits(&incremental, "b.rs"), Some(5));

    repo.mv("b.rs", "sub/c.rs");
    repo.commit("feat: b to c");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    let paths = ["a.rs", "b.rs", "sub/c.rs", "peer.rs"];
    assert_same_history(&incremental, &full, &paths);
    assert_eq!(commits(&full, "a.rs"), None);
    assert_eq!(commits(&full, "b.rs"), None);
    assert_eq!(commits(&full, "sub/c.rs"), Some(6));
    let pairs = full.co_changes_for("peer.rs", 10).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("sub/c.rs", 4));
}

#[test]
fn rename_back_to_a_prior_name_rejoins_one_history() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "peer.rs"], tag);
    }
    let incremental = repo.full();
    repo.mv("a.rs", "b.rs");
    repo.commit("feat: a to b");
    repo.edit(&["b.rs", "peer.rs"], 4);
    repo.mv("b.rs", "a.rs");
    repo.commit("feat: b back to a");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "b.rs", "peer.rs"]);
    assert_eq!(commits(&full, "b.rs"), None);
    assert_eq!(commits(&full, "a.rs"), Some(6));
    assert_eq!(full.co_changes_for("peer.rs", 10).unwrap()[0].count, 4);
}

#[test]
fn copy_starts_a_fresh_history_and_leaves_the_source_alone() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "peer.rs"], tag);
    }
    repo.git(&["config", "diff.renames", "copies"]);
    let original = body("a.rs", 3);
    repo.write("copy.rs", &original);
    repo.write("a.rs", &format!("{original}fn extra() {{}}\n"));
    repo.commit("feat: copy a");
    // The fixture's own config reports the copy, so an indexer that honoured
    // it would move a.rs's history onto copy.rs.
    let raw = repo.git(&["log", "-1", "--numstat", "-z", "--format="]);
    assert!(
        raw.contains("\0a.rs\0copy.rs\0"),
        "fixture must record a copy: {raw:?}"
    );

    let full = repo.full();
    assert_eq!(commits(&full, "a.rs"), Some(4));
    assert_eq!(commits(&full, "copy.rs"), Some(1));
    let pairs = full.co_changes_for("peer.rs", 10).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("a.rs", 3));
}

#[test]
fn rename_in_a_skipped_chore_commit_still_moves_history() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "b.rs"], tag);
    }
    let incremental = repo.full();
    repo.mv("a.rs", "c.rs");
    repo.commit("chore: move a");
    let stats = repo.index(&incremental, IndexMode::Incremental);
    assert_eq!(stats.commits_scanned, 0, "chore commits are not counted");

    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "b.rs", "c.rs"]);
    assert_eq!(commits(&full, "a.rs"), None);
    assert_eq!(commits(&full, "c.rs"), Some(3));
}

#[test]
fn renames_across_the_exclusion_boundary_carry_only_indexed_history() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["vendor/lib.rs", "src/out.rs", "peer.rs"], tag);
    }
    let excludes = ["vendor/**"];
    let incremental = Database::open_in_memory().unwrap();
    repo.index_excluding(&incremental, IndexMode::Full, &excludes);
    assert_eq!(commits(&incremental, "src/out.rs"), Some(3));

    repo.mv("vendor/lib.rs", "src/lib.rs");
    repo.mv("src/out.rs", "vendor/out.rs");
    repo.commit("feat: move across vendor");
    repo.index_excluding(&incremental, IndexMode::Incremental, &excludes);

    let full = Database::open_in_memory().unwrap();
    repo.index_excluding(&full, IndexMode::Full, &excludes);
    let paths = [
        "vendor/lib.rs",
        "src/lib.rs",
        "src/out.rs",
        "vendor/out.rs",
        "peer.rs",
    ];
    assert_same_history(&incremental, &full, &paths);
    assert_eq!(
        commits(&full, "src/lib.rs"),
        Some(1),
        "vendored history is never indexed, so it cannot follow the file"
    );
    assert_eq!(commits(&full, "src/out.rs"), None);
    assert_eq!(commits(&full, "vendor/out.rs"), None, "excluded successor");
    assert!(full.co_changes_for("peer.rs", 10).unwrap().is_empty());
}

#[test]
fn rename_onto_a_path_with_recorded_history_matches_a_full_scan() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "c.rs", "peer.rs"], tag);
    }
    let incremental = repo.full();
    repo.git(&["rm", "-q", "c.rs"]);
    repo.commit("feat: drop c");
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: a takes c's name");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "c.rs", "peer.rs"]);
    assert_eq!(
        commits(&full, "c.rs"),
        Some(5),
        "3 shared commits, the delete, and the rename"
    );
    let pairs = full.co_changes_for("peer.rs", 10).unwrap();
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("c.rs", 3));
}

#[test]
fn two_recorded_files_renamed_onto_one_path_match_a_full_scan() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["a.rs", "b.rs", "peer.rs"], tag);
    }
    let incremental = repo.full();
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: a to c");
    repo.git(&["rm", "-q", "c.rs"]);
    repo.commit("feat: drop c");
    repo.mv("b.rs", "c.rs");
    repo.commit("feat: b to c");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "b.rs", "c.rs", "peer.rs"]);
    assert_eq!(commits(&full, "c.rs"), Some(6));
    let pairs = full.co_changes_for("peer.rs", 10).unwrap();
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("c.rs", 3));
}

/// A HEAD path that is not UTF-8, committed outside the history window, must
/// not fail a pass that consults HEAD's tree to follow a rename.
#[cfg(unix)]
#[test]
fn non_utf8_head_path_does_not_fail_a_pass_with_a_rename() {
    use std::os::unix::ffi::OsStrExt;

    let mut repo = Repo::new();
    let in_window = repo.next_ts;
    repo.next_ts = unix_now() - 1500 * DAY;
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    std::fs::write(repo.root().join(name), "old\n").unwrap();
    repo.commit("feat: latin-1 name");
    repo.next_ts = in_window;
    repo.edit(&["a.rs"], 1);
    let incremental = repo.full();
    repo.edit(&["a.rs"], 2);
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: a to c");

    repo.index(&incremental, IndexMode::Incremental);
    let full = repo.full();
    assert_same_history(&incremental, &full, &["a.rs", "c.rs"]);
    assert_eq!(commits(&full, "a.rs"), None);
    assert_eq!(commits(&full, "c.rs"), Some(3));
}

#[test]
fn rename_into_the_test_tree_matches_a_full_scan() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["src/x.rs", "tests/t.rs"], tag);
    }
    let incremental = repo.full();
    assert_eq!(
        incremental.co_changes_for("tests/t.rs", 10).unwrap().len(),
        1
    );
    repo.mv("src/x.rs", "tests/u.rs");
    repo.commit("feat: x becomes a test");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    assert_same_history(
        &incremental,
        &full,
        &["src/x.rs", "tests/u.rs", "tests/t.rs"],
    );
    assert!(
        full.co_changes_for("tests/t.rs", 10).unwrap().is_empty(),
        "test-test pairs are never recorded"
    );
}

#[test]
fn rename_out_of_the_test_tree_matches_a_full_scan() {
    let mut repo = Repo::new();
    for tag in 1..=3 {
        repo.edit(&["tests/a.rs", "tests/b.rs"], tag);
    }
    let incremental = repo.full();
    assert!(
        incremental
            .co_changes_for("tests/b.rs", 10)
            .unwrap()
            .is_empty()
    );
    repo.mv("tests/a.rs", "src/a.rs");
    repo.commit("feat: a leaves the tests");
    repo.index(&incremental, IndexMode::Incremental);

    let full = repo.full();
    assert_same_history(
        &incremental,
        &full,
        &["tests/a.rs", "src/a.rs", "tests/b.rs"],
    );
    let pairs = full.co_changes_for("tests/b.rs", 10).unwrap();
    assert_eq!((pairs[0].file.as_str(), pairs[0].count), ("src/a.rs", 3));
}

#[test]
fn branch_edit_merged_across_a_rename_follows_the_file() {
    let mut repo = Repo::new();
    repo.git(&["config", "merge.renames", "true"]);
    for tag in 1..=3 {
        repo.edit(&["a.rs"], tag);
    }
    let main = repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let main = main.trim().to_owned();
    let before_fork = repo.full();

    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.edit(&["a.rs"], 4);
    repo.git(&["checkout", "-q", &main]);
    repo.mv("a.rs", "c.rs");
    repo.commit("feat: rename a to c on main");
    let after_rename = repo.full();
    assert_eq!(commits(&after_rename, "c.rs"), Some(4));
    repo.git(&["merge", "-q", "--no-edit", "feature"]);
    repo.next_ts += DAY;

    let full = repo.full();
    assert_eq!(
        commits(&full, "a.rs"),
        None,
        "the branch edit must not resurrect the dead path"
    );
    assert_eq!(
        commits(&full, "c.rs"),
        Some(5),
        "3 edits, the branch edit, the rename"
    );

    // One incremental range holding the rename and the branch edit agrees.
    repo.index(&before_fork, IndexMode::Incremental);
    assert_same_history(&before_fork, &full, &["a.rs", "c.rs"]);

    // Documented limit: when the rename was indexed in an earlier pass, the
    // later range holds no rename to follow, so the branch edit stays on a.rs
    // until `git-index --full`.
    repo.index(&after_rename, IndexMode::Incremental);
    assert_eq!(commits(&after_rename, "a.rs"), Some(1));
    assert_eq!(commits(&after_rename, "c.rs"), Some(4));
}
