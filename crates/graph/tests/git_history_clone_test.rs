use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use codesage_graph::{
    IndexMode, assess_risk, assess_risk_batch, assess_risk_diff, build_review_rehearsal,
    full_index, git_history_index_with_options,
};
use codesage_storage::Database;

fn git(root: &Path, args: &[&str], input: Option<&str>) -> String {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn origin(root: &Path) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "-q", "-b", "main"], None);
    for step in 0..4 {
        let subject = if step == 2 {
            "fix: boundary"
        } else {
            "implement change"
        };
        let mut input = format!(
            "commit refs/heads/main\ncommitter Author{step} <a{step}@example.invalid> {} +0000\ndata {}\n{subject}\n",
            1_700_000_000 + step * 86_400,
            subject.len(),
        );
        if step != 0 {
            input.push_str(&format!(
                "from {}\n",
                git(root, &["rev-parse", "HEAD"], None)
            ));
        }
        for name in if step == 3 {
            &["a"][..]
        } else {
            &["a", "b"][..]
        } {
            let content = format!("pub fn {name}() {{}}\n{}", "// change\n".repeat(step + 1));
            input.push_str(&format!(
                "M 100644 inline {name}.rs\ndata {}\n{content}",
                content.len()
            ));
        }
        git(root, &["fast-import", "--quiet"], Some(&(input + "\n")));
    }
    git(root, &["config", "uploadpack.allowFilter", "true"], None);
    git(
        root,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
        None,
    );
}

fn clone_repo(base: &Path, source: &Path, name: &str, option: &str) -> std::path::PathBuf {
    let root = base.join(name);
    git(
        base,
        &[
            "clone",
            "-q",
            option,
            &format!("file://{}", source.display()),
            root.to_str().unwrap(),
        ],
        None,
    );
    root
}

fn missing(root: &Path) -> Vec<String> {
    let mut objects: Vec<_> = git(
        root,
        &["rev-list", "--objects", "--all", "--missing=print"],
        None,
    )
    .lines()
    .filter(|line| line.starts_with('?'))
    .map(str::to_owned)
    .collect();
    objects.sort();
    objects
}

#[test]
fn blobless_file_clone_keeps_missing_objects_and_discloses_tree_only_history() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    let root = clone_repo(temp.path(), &source, "partial", "--filter=blob:none");
    let before = missing(&root);
    assert!(
        before.len() >= 4,
        "fixture must retain missing historical blobs"
    );
    let db = Database::open_in_memory().unwrap();
    let stats = git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    assert_eq!(stats.commits_scanned, 4);
    assert_eq!(stats.files_tracked, 2);
    assert_eq!(missing(&root), before);
    assert!(stats.notes.iter().any(|note| note.contains("tree-only")));
    let file = db.git_file("a.rs").unwrap().unwrap();
    assert_eq!(file.total_commits, 4);
    assert_eq!(file.churn_score, 0.0);
    assert_eq!(db.git_author_events("a.rs").unwrap().len(), 4);
    let targets = vec!["a.rs".to_owned()];
    let full_db = Database::open_in_memory().unwrap();
    git_history_index_with_options(&full_db, &source, &[], IndexMode::Full).unwrap();
    let full_risk = assess_risk(&full_db, "a.rs").unwrap();
    assert_eq!(full_risk.churn_percentile, 1.0);
    let diff = assess_risk_diff(&db, &targets).unwrap();
    assert!(diff.hotspot_files.is_empty());
    assert!(diff.summary_notes.iter().any(|note| {
        note.contains("tree-only") && note.contains("omitted from the risk score")
    }));
    for risk in [
        assess_risk(&db, "a.rs").unwrap(),
        assess_risk_batch(&db, &targets).unwrap().files.remove(0),
        diff.files.into_iter().next().unwrap(),
    ] {
        assert_eq!(risk.churn_percentile, 0.0);
        assert!((full_risk.score - risk.score - 0.32).abs() < 1e-12);
        assert!(!risk.notes.iter().any(|note| note.starts_with("hotspot:")));
        assert!(risk.notes.iter().any(|note| {
            note.contains("line churn and rename continuity are unavailable")
                && note.contains("omitted from the risk score")
        }));
    }
    let unchanged = git_history_index_with_options(&db, &root, &[], IndexMode::Auto).unwrap();
    assert_eq!(unchanged.commits_scanned, 0);
    assert_eq!(unchanged.notes, stats.notes);
    assert_eq!(missing(&root), before);
}

#[test]
fn shallow_boundary_adds_no_churn_fix_author_or_pair_and_deepening_rebuilds() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    let root = clone_repo(temp.path(), &source, "shallow", "--depth=2");
    let db = Database::open_in_memory().unwrap();
    db.execute_raw_for_tests("CREATE TEMP TRIGGER reject_boundary_pairs BEFORE INSERT ON git_co_changes BEGIN SELECT RAISE(ABORT, 'boundary contributes a pair'); END;").unwrap();
    let stats = git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    assert_eq!(stats.commits_scanned, 2);
    assert!(
        stats
            .notes
            .iter()
            .any(|note| note.contains("shallow clone"))
    );
    let a = db.git_file("a.rs").unwrap().unwrap();
    assert_eq!(a.total_commits, 2);
    assert_eq!(a.fix_count, 0);
    assert!((a.churn_score - 0.01).abs() < 1e-12);
    let b = db.git_file("b.rs").unwrap().unwrap();
    assert_eq!(b.total_commits, 1);
    assert_eq!(b.fix_count, 0);
    assert_eq!(b.churn_score, 0.0);
    assert_eq!(b.last_commit_at, Some(1_700_172_800));
    assert_eq!(assess_risk(&db, "a.rs").unwrap().churn_percentile, 1.0);
    assert_eq!(assess_risk(&db, "b.rs").unwrap().churn_percentile, 0.0);
    assert!(db.git_author_events("b.rs").unwrap().is_empty());
    assert_eq!(
        db.git_author_events("a.rs").unwrap(),
        [("email:a3@example.invalid".into(), 1_700_259_200)]
    );
    assert!(
        assess_risk(&db, "b.rs")
            .unwrap()
            .notes
            .iter()
            .any(|note| note.contains("shallow clone") && note.contains("last_commit_at"))
    );
    db.execute_raw_for_tests("DROP TRIGGER reject_boundary_pairs;")
        .unwrap();

    git(&root, &["fetch", "-q", "--deepen=1"], None);
    let deeper = git_history_index_with_options(&db, &root, &[], IndexMode::Auto).unwrap();
    assert_eq!(
        deeper.commits_scanned, 3,
        "same HEAD with a deeper boundary must rescan"
    );
    assert!(
        deeper
            .notes
            .iter()
            .any(|note| note.contains("shallow clone"))
    );
    assert_eq!(db.git_file("b.rs").unwrap().unwrap().fix_count, 1);
    assert_eq!(db.git_author_events("b.rs").unwrap().len(), 1);

    git(&root, &["fetch", "-q", "--unshallow"], None);
    let full = git_history_index_with_options(&db, &root, &[], IndexMode::Auto).unwrap();
    assert_eq!(full.commits_scanned, 4);
    assert!(full.notes.is_empty());
    let expected = Database::open_in_memory().unwrap();
    git_history_index_with_options(&expected, &source, &[], IndexMode::Full).unwrap();
    for path in ["a.rs", "b.rs"] {
        let actual = db.git_file(path).unwrap().unwrap();
        let control = expected.git_file(path).unwrap().unwrap();
        assert_eq!(actual.total_commits, control.total_commits);
        assert_eq!(actual.fix_count, control.fix_count);
        assert_eq!(actual.churn_score, control.churn_score);
        assert_eq!(
            db.git_author_events(path).unwrap(),
            expected.git_author_events(path).unwrap()
        );
        assert!(
            db.git_author_events(path)
                .unwrap()
                .iter()
                .any(|(author, _)| author == "email:a0@example.invalid")
        );
    }
    assert!(
        !assess_risk(&db, "a.rs")
            .unwrap()
            .notes
            .iter()
            .any(|note| note.contains("shallow clone"))
    );
}

#[test]
fn boundary_only_shallow_history_never_becomes_a_churn_hotspot() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    let root = clone_repo(temp.path(), &source, "shallow", "--depth=1");
    let db = Database::open_in_memory().unwrap();
    let stats = git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    assert_eq!(stats.commits_scanned, 1);
    assert_eq!(db.git_file("a.rs").unwrap().unwrap().total_commits, 1);
    let targets = vec!["a.rs".to_owned()];
    let diff = assess_risk_diff(&db, &targets).unwrap();
    assert!(diff.hotspot_files.is_empty());
    for risk in [
        assess_risk(&db, "a.rs").unwrap(),
        assess_risk_batch(&db, &targets).unwrap().files.remove(0),
        diff.files.into_iter().next().unwrap(),
    ] {
        assert_eq!(risk.churn_score, 0.0);
        assert_eq!(risk.churn_percentile, 0.0);
        assert_eq!(risk.score, 0.13, "only the test-gap term remains");
        assert!(!risk.notes.iter().any(|note| note.starts_with("hotspot:")));
        assert!(
            risk.notes
                .iter()
                .any(|note| note.contains("churn risk omitted"))
        );
        assert!(risk.notes.iter().any(|note| note.contains("shallow clone")));
    }
}

#[test]
fn clustered_shallow_files_keep_omitted_churn_cautions_in_the_summary() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    git(&source, &["reset", "--hard", "HEAD"], None);
    std::fs::create_dir(source.join("src")).unwrap();
    for (step, names) in ["abcde", "abc"].into_iter().enumerate() {
        for name in names.chars() {
            std::fs::write(
                source.join(format!("src/{name}.rs")),
                format!("pub fn {name}() {{}}\n{}", "// changed\n".repeat(step + 1)),
            )
            .unwrap();
        }
        git(&source, &["add", "src"], None);
        git(
            &source,
            &[
                "-c",
                "user.name=Author",
                "-c",
                "user.email=author@example.invalid",
                "commit",
                "-qm",
                "implement change",
            ],
            None,
        );
    }
    let root = clone_repo(temp.path(), &source, "shallow", "--depth=2");
    let db = Database::open_in_memory().unwrap();
    git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
    let targets: Vec<_> = "abcde"
        .chars()
        .map(|name| format!("src/{name}.rs"))
        .collect();
    let diff = assess_risk_diff(&db, &targets).unwrap();
    assert_eq!(diff.clustered_directories.len(), 1);
    let cluster = &diff.clustered_directories[0];
    assert_eq!(cluster.top_files.len(), 3);
    assert!(cluster.top_files.iter().all(|file| file.churn_score > 0.0));
    for path in ["src/d.rs", "src/e.rs"] {
        assert!(cluster.omitted_files.iter().any(|file| file == path));
        assert!(diff.summary_notes.iter().any(|note| {
            note.starts_with(&format!("{path}: ")) && note.contains("churn risk omitted")
        }));
    }
    assert!(
        diff.summary_notes
            .iter()
            .any(|note| note.contains("shallow clone"))
    );
}

#[test]
fn cycle_refactor_targets_require_available_churn() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    git(&source, &["reset", "--hard", "-q", "HEAD"], None);
    for step in 0..2 {
        for (name, peer) in [("a", "b"), ("b", "a")] {
            std::fs::write(
                source.join(format!("{name}.js")),
                format!(
                    "import './{peer}.js';\nexport function {name}() {{}}\n{}",
                    "// change\n".repeat(step + 1)
                ),
            )
            .unwrap();
        }
        git(&source, &["add", "a.js", "b.js"], None);
        git(
            &source,
            &[
                "-c",
                "user.name=Cycle Author",
                "-c",
                "user.email=cycle@example.invalid",
                "commit",
                "-qm",
                "implement cycle",
            ],
            None,
        );
    }
    for (name, option, measured) in [
        ("partial", "--filter=blob:none", false),
        ("boundary", "--depth=1", false),
        ("shallow", "--depth=2", true),
        ("full", "--no-local", true),
    ] {
        let root = clone_repo(temp.path(), &source, name, option);
        let db = Database::open_in_memory().unwrap();
        full_index(&root, &db, &[], false).unwrap();
        git_history_index_with_options(&db, &root, &[], IndexMode::Full).unwrap();
        let targets = vec!["a.js".to_owned()];
        let diff = assess_risk_diff(&db, &targets).unwrap();
        assert_eq!(diff.cycles_touching_patch.len(), 1, "{name}");
        assert_eq!(
            diff.cycles_touching_patch[0].max_churn_file.is_some(),
            measured,
            "{name}"
        );
        let rehearsal = build_review_rehearsal(&root, &db, &targets).unwrap();
        let cycle = rehearsal
            .objections
            .iter()
            .find(|objection| objection.category == "import-cycle")
            .unwrap();
        assert_eq!(
            cycle
                .evidence
                .iter()
                .any(|entry| entry.starts_with("best refactor target:")),
            measured,
            "{name}"
        );
    }
}

#[test]
fn legacy_read_only_history_remains_scorable_without_clone_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("origin");
    origin(&source);
    let path = temp.path().join("legacy.db");
    let db = Database::open(&path).unwrap();
    git_history_index_with_options(&db, &source, &[], IndexMode::Full).unwrap();
    let expected = assess_risk(&db, "a.rs").unwrap();
    db.execute_raw_for_tests(
        "DROP TRIGGER git_index_state_invalidate_exclusion;
         ALTER TABLE git_index_state DROP COLUMN exclude_fingerprint;",
    )
    .unwrap();
    drop(db);

    let reader = Database::open_read_only(&path).unwrap();
    assert_eq!(reader.git_index_exclusion_fingerprint().unwrap(), None);
    let targets = vec!["a.rs".to_owned()];
    for risk in [
        assess_risk(&reader, "a.rs").unwrap(),
        assess_risk_batch(&reader, &targets)
            .unwrap()
            .files
            .remove(0),
        assess_risk_diff(&reader, &targets).unwrap().files.remove(0),
    ] {
        assert_eq!(risk.score, expected.score);
        assert_eq!(risk.churn_percentile, expected.churn_percentile);
    }
    drop(reader);

    let writer = Database::open(&path).unwrap();
    writer
        .execute_raw_for_tests("DROP TABLE git_index_state;")
        .unwrap();
    assert!(writer.git_index_exclusion_fingerprint().is_err());
}
