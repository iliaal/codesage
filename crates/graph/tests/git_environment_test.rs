use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{io::Write, time::Duration};

use codesage_graph::{IndexMode, git_history_index_with_options};
use codesage_storage::Database;

fn fixture_git(root: &Path, args: &[&str], input: Option<&str>) -> String {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn append_commit(root: &Path, prefix: &str, step: usize, rename: bool) {
    let mut input = format!(
        "commit refs/heads/main\ncommitter Fixture <{prefix}@example.invalid> {} +0000\ndata 4\nfix\n",
        1_700_000_000 + step * 86_400
    );
    if step > 0 {
        input.push_str(&format!(
            "from {}\n",
            fixture_git(root, &["rev-parse", "HEAD"], None)
        ));
    }
    for suffix in ["a", "b"] {
        let body = format!(
            "pub fn {prefix}_{suffix}() {{}}\n{}",
            (0..=step)
                .map(|n| format!("// step {n}\n"))
                .collect::<String>()
        );
        let path = if rename && suffix == "a" {
            format!("{prefix}-renamed.rs")
        } else {
            format!("{prefix}-{suffix}.rs")
        };
        input.push_str(&format!(
            "M 100644 inline {path}\ndata {}\n{body}",
            body.len()
        ));
    }
    if rename {
        input.push_str(&format!("D {prefix}-a.rs\n"));
    }
    input.push('\n');
    fixture_git(root, &["fast-import", "--quiet"], Some(&input));
    fixture_git(root, &["reset", "--hard", "-q", "HEAD"], None);
}

fn fixture(root: &Path, prefix: &str, commits: usize) {
    std::fs::create_dir(root).unwrap();
    fixture_git(root, &["init", "-q", "-b", "main"], None);
    for step in 0..commits {
        append_commit(root, prefix, step, false);
    }
    fixture_git(
        root,
        &["config", "core.hooksPath", &format!("{prefix}-hooks")],
        None,
    );
    std::fs::create_dir(root.join(format!("{prefix}-hooks"))).unwrap();
    std::fs::write(
        root.join(format!("{prefix}-hooks/post-commit")),
        if prefix == "target" {
            "# codesage install-hooks\n"
        } else {
            "# decoy\n"
        },
    )
    .unwrap();
}

fn assert_history_matches(actual: &Database, expected: &Database) {
    assert_eq!(
        actual.get_git_index_state().unwrap().unwrap().0,
        expected.get_git_index_state().unwrap().unwrap().0
    );
    assert_eq!(
        actual.git_index_exclusion_fingerprint().unwrap(),
        expected.git_index_exclusion_fingerprint().unwrap()
    );
    for path in ["target-a.rs", "target-b.rs"] {
        let actual_file = actual.git_file(path).unwrap().unwrap();
        let expected_file = expected.git_file(path).unwrap().unwrap();
        assert_eq!(
            (
                actual_file.path,
                actual_file.churn_score,
                actual_file.fix_count,
                actual_file.total_commits,
                actual_file.last_commit_at,
            ),
            (
                expected_file.path,
                expected_file.churn_score,
                expected_file.fix_count,
                expected_file.total_commits,
                expected_file.last_commit_at,
            )
        );
        assert_eq!(
            actual.git_author_events(path).unwrap(),
            expected.git_author_events(path).unwrap()
        );
        let actual_pairs = actual.co_changes_for(path, 10).unwrap();
        let expected_pairs = expected.co_changes_for(path, 10).unwrap();
        assert_eq!(actual_pairs.len(), expected_pairs.len());
        for (actual_pair, expected_pair) in actual_pairs.into_iter().zip(expected_pairs) {
            assert_eq!(
                (
                    actual_pair.file,
                    actual_pair.weight,
                    actual_pair.count,
                    actual_pair.last_observed_at,
                    actual_pair.first_observed_at,
                    actual_pair.window_mask,
                    actual_pair.windows,
                    actual_pair.other_commits,
                ),
                (
                    expected_pair.file,
                    expected_pair.weight,
                    expected_pair.count,
                    expected_pair.last_observed_at,
                    expected_pair.first_observed_at,
                    expected_pair.window_mask,
                    expected_pair.windows,
                    expected_pair.other_commits,
                )
            );
        }
    }
}

fn legacy_same_head_shallow_decoy_rebuilds(mode: IndexMode) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("target");
    let decoy = temp.path().join("decoy");
    fixture(&root, "target", 3);
    fixture_git(
        temp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", root.display()),
            decoy.to_str().unwrap(),
        ],
        None,
    );
    let head = fixture_git(&root, &["rev-parse", "HEAD"], None);
    assert_eq!(fixture_git(&decoy, &["rev-parse", "HEAD"], None), head);
    assert_eq!(
        fixture_git(&root, &["rev-list", "--count", "HEAD"], None),
        "3"
    );
    assert_eq!(
        fixture_git(&decoy, &["rev-list", "--count", "HEAD"], None),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(decoy.join(".git/shallow"))
            .unwrap()
            .trim(),
        head
    );

    let db = Database::open_in_memory().unwrap();
    let legacy = git_history_index_with_options(&db, &decoy, &[], IndexMode::Full).unwrap();
    assert_eq!(legacy.commits_scanned, 1);
    assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head);
    assert_eq!(
        db.git_file("target-a.rs").unwrap().unwrap().total_commits,
        1
    );
    assert!(db.co_changes_for("target-a.rs", 10).unwrap().is_empty());
    let fingerprint = db.git_index_exclusion_fingerprint().unwrap().unwrap();
    // The rows come from real shallow Git history; only the writer's policy
    // identity is restored to the version used before root isolation.
    db.set_git_index_exclusion_fingerprint(&format!(
        "policy:1\0{}",
        fingerprint.split_once('\0').unwrap().1
    ))
    .unwrap();

    let rebuilt = git_history_index_with_options(&db, &root, &[], mode).unwrap();
    assert_eq!(
        rebuilt.commits_scanned, 3,
        "{mode:?} must replace legacy rows"
    );
    assert_eq!(rebuilt.files_tracked, 2);
    assert_eq!(rebuilt.co_change_pairs, 1);
    let expected = Database::open_in_memory().unwrap();
    git_history_index_with_options(&expected, &root, &[], IndexMode::Full).unwrap();
    assert_history_matches(&db, &expected);

    for current in [&db, &expected] {
        let unchanged = git_history_index_with_options(current, &root, &[], mode).unwrap();
        assert_eq!(unchanged.commits_scanned, 0);
        assert_eq!(unchanged.files_tracked, 0);
        assert_eq!(unchanged.co_change_pairs, 0);
    }
    assert_history_matches(&db, &expected);
}

#[test]
fn legacy_same_head_shallow_decoy_rebuilds_in_auto() {
    legacy_same_head_shallow_decoy_rebuilds(IndexMode::Auto);
}

#[test]
fn legacy_same_head_shallow_decoy_rebuilds_incrementally() {
    legacy_same_head_shallow_decoy_rebuilds(IndexMode::Incremental);
}

#[test]
fn project_root_overrides_inherited_git_selectors() {
    for poisoned in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target");
        let decoy = temp.path().join("decoy");
        fixture(&root, "target", 3);
        fixture(&decoy, "decoy", 1);
        let config = temp.path().join("caller-config");
        std::fs::write(
            &config,
            "[codesage]\nfile = retained\n[core]\nhooksPath = target-hooks\n",
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "inherited_environment_child", "--nocapture"])
            .env("CODESAGE_GIT_ENV_FIXTURE", &root)
            .env("CODESAGE_GIT_ENV_DECOY", &decoy)
            .env("CODESAGE_GIT_ENV_UNRELATED", temp.path())
            .env("GIT_CONFIG", &config)
            .env("GIT_CONFIG_PARAMETERS", "'codesage.parameters=retained'")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "codesage.count")
            .env("GIT_CONFIG_VALUE_0", "retained")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        if poisoned {
            command
                .env("GIT_DIR", decoy.join(".git"))
                .env("GIT_WORK_TREE", &decoy)
                .env("GIT_INDEX_FILE", decoy.join(".git/index"))
                .env("GIT_COMMON_DIR", decoy.join(".git"))
                .env("GIT_OBJECT_DIRECTORY", decoy.join(".git/objects"))
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    decoy.join(".git/objects"),
                )
                .env("GIT_IMPLICIT_WORK_TREE", "0")
                .env("GIT_GRAFT_FILE", decoy.join(".git/grafts"))
                .env("GIT_REPLACE_REF_BASE", "refs/decoy/")
                .env("GIT_PREFIX", "decoy/")
                .env("GIT_SHALLOW_FILE", decoy.join(".git/shallow"))
                .env("GIT_NAMESPACE", "decoy")
                .env("GIT_CEILING_DIRECTORIES", root.parent().unwrap())
                .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "0")
                .env("GIT_REFERENCE_BACKEND", "decoy");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "poisoned={poisoned}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("git-isolation-checked"));
    }
}

#[test]
fn inherited_environment_child() {
    let Some(root) = std::env::var_os("CODESAGE_GIT_ENV_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let decoy = PathBuf::from(std::env::var_os("CODESAGE_GIT_ENV_DECOY").unwrap());
    let environment_before: Vec<_> = std::env::vars_os().collect();
    let head = fixture_git(&root, &["rev-parse", "HEAD"], None);
    let selected = codesage_graph::git_command(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(selected.status.success(), "{selected:?}");
    assert_eq!(String::from_utf8(selected.stdout).unwrap().trim(), head);
    let command = codesage_graph::git_command(&root);
    let removals: Vec<_> = command
        .get_envs()
        .filter_map(|(name, value)| value.is_none().then_some(name))
        .collect();
    for name in fixture_git(&root, &["rev-parse", "--local-env-vars"], None)
        .lines()
        .chain([
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_REFERENCE_BACKEND",
        ])
    {
        if !matches!(
            name,
            "GIT_CONFIG" | "GIT_CONFIG_PARAMETERS" | "GIT_CONFIG_COUNT" | "GIT_NO_REPLACE_OBJECTS"
        ) {
            assert!(
                removals.contains(&std::ffi::OsStr::new(name)),
                "selector {name} is inherited"
            );
        }
    }
    let from_file = codesage_graph::git_command(&root)
        .args(["config", "--get", "codesage.file"])
        .output()
        .unwrap();
    assert!(from_file.status.success(), "{from_file:?}");
    assert_eq!(from_file.stdout, b"retained\n");
    for key in ["codesage.count", "codesage.parameters"] {
        let output = codesage_graph::git_command(&root)
            .env_remove("GIT_CONFIG")
            .args(["config", "--get", key])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"retained\n");
    }
    let inherited = codesage_graph::git_command(&root)
        .args(["-c", "alias.codesage-env=!env", "codesage-env"])
        .output()
        .unwrap();
    assert!(inherited.status.success(), "{inherited:?}");
    let inherited = String::from_utf8(inherited.stdout).unwrap();
    for name in [
        "CODESAGE_GIT_ENV_UNRELATED",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_NO_REPLACE_OBJECTS",
    ] {
        let expected = format!("{name}={}", std::env::var(name).unwrap());
        assert!(
            inherited.lines().any(|line| line == expected),
            "{name} was not inherited"
        );
    }
    let db = Database::open_in_memory().unwrap();
    let full = git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    assert_eq!(full.commits_scanned, 3);
    assert_eq!(full.files_tracked, 2);
    assert_eq!(full.co_change_pairs, 1);
    assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head);
    assert_eq!(
        db.git_file("target-a.rs").unwrap().unwrap().total_commits,
        3
    );
    assert!(db.git_file("decoy-a.rs").unwrap().is_none());
    let expected_churn = (0..3)
        .map(|step| {
            let delta = if step == 0 { 2.0 } else { 1.0 };
            delta / 100.0 * (-((2 - step) as f64) / 180.0).exp()
        })
        .sum::<f64>();
    assert!(
        (db.git_file("target-a.rs").unwrap().unwrap().churn_score - expected_churn).abs() < 1e-9
    );
    let pair = db.co_changes_for("target-a.rs", 10).unwrap().pop().unwrap();
    assert_eq!(pair.count, 3);

    append_commit(&root, "target", 3, true);
    let next_head = fixture_git(&root, &["rev-parse", "HEAD"], None);
    let incremental =
        git_history_index_with_options(&db, &root, &[], IndexMode::Incremental).unwrap();
    assert_eq!(incremental.commits_scanned, 1);
    assert_eq!(db.get_git_index_state().unwrap().unwrap().0, next_head);
    assert!(db.git_file("target-a.rs").unwrap().is_none());
    assert_eq!(
        db.git_file("target-renamed.rs")
            .unwrap()
            .unwrap()
            .total_commits,
        4
    );
    assert!(db.git_file("decoy-a.rs").unwrap().is_none());
    assert_eq!(
        db.co_changes_for("target-renamed.rs", 10).unwrap()[0].count,
        4
    );
    let rescan = Database::open_in_memory().unwrap();
    git_history_index_with_options(&rescan, &root, &[], IndexMode::Full).unwrap();
    let actual = db.git_file("target-renamed.rs").unwrap().unwrap();
    let expected = rescan.git_file("target-renamed.rs").unwrap().unwrap();
    assert!((actual.churn_score - expected.churn_score).abs() < 1e-9);
    assert_eq!(actual.fix_count, expected.fix_count);

    assert_eq!(
        codesage_graph::drift::git_head_sha(&root).as_deref(),
        Some(next_head.as_str())
    );
    assert_eq!(
        codesage_graph::drift::git_common_dir(&root),
        Some(root.join(".git"))
    );
    let changed = codesage_graph::changed_files_since(&root, &head).unwrap();
    assert_eq!(
        changed,
        ["target-a.rs", "target-renamed.rs", "target-b.rs"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    let check = codesage_graph::edit_check::edit_check(
        &root,
        "target-renamed.rs",
        "target_a",
        None,
        "pub fn target_a(x: i32) {}",
    )
    .unwrap();
    assert_eq!(check.head, next_head);
    assert_eq!(check.before.arity.unwrap().minimum, 0);
    assert_eq!(
        codesage_graph::hook_health::inspect(&root)
            .unwrap()
            .installed_hooks,
        ["post-commit"]
    );
    let snapshot = codesage_graph::session_start(&root, &db, "git-environment").unwrap();
    assert_eq!(snapshot.git_head.as_deref(), Some(next_head.as_str()));
    let branches = codesage_graph::branch_overlap::branch_overlap(
        &root,
        &["target-b.rs".into()],
        Duration::from_secs(2),
    );
    assert_eq!(branches.current_commit.as_deref(), Some(next_head.as_str()));
    assert!(branches.complete, "{branches:?}");

    let plain = root.parent().unwrap().join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert!(
        git_history_index_with_options(
            &Database::open_in_memory().unwrap(),
            &plain,
            &[],
            IndexMode::Full
        )
        .is_err()
    );
    assert!(codesage_graph::drift::git_head_sha(&plain).is_none());
    assert!(codesage_graph::hook_health::inspect(&plain).is_none());

    let linked = root.parent().unwrap().join("linked");
    fixture_git(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().unwrap(),
        ],
        None,
    );
    let linked_db = Database::open_in_memory().unwrap();
    let linked_stats =
        git_history_index_with_options(&linked_db, &linked, &[], IndexMode::Full).unwrap();
    assert_eq!(linked_stats.commits_scanned, 4);
    assert_eq!(
        linked_db.get_git_index_state().unwrap().unwrap().0,
        next_head
    );
    assert_eq!(
        codesage_graph::drift::git_common_dir(&linked),
        Some(root.join(".git"))
    );
    assert_ne!(fixture_git(&decoy, &["rev-parse", "HEAD"], None), next_head);
    assert_eq!(std::env::vars_os().collect::<Vec<_>>(), environment_before);
    println!("git-isolation-checked");
}
