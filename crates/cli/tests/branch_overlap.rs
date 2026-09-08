use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn fixture() -> tempfile::TempDir {
    fixture_in("")
}

fn fixture_in(prefix: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "base"]);
    let project = dir.path().join(prefix);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("shared.rs"), "base\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "base"]);
    git(dir.path(), &["checkout", "-b", "sibling"]);
    std::fs::write(project.join("shared.rs"), "sibling\n").unwrap();
    git(dir.path(), &["commit", "-am", "sibling"]);
    git(dir.path(), &["checkout", "-b", "current", "base"]);
    std::fs::write(dir.path().join("current.rs"), "current\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "current"]);
    dir
}

fn run(root: &Path, state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .env("CODESAGE_DAEMON_RUNTIME_DIR", state)
        .env("XDG_STATE_HOME", state)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn brief_and_rehearsal_serve_real_git_evidence_without_creating_an_index() {
    let root = fixture();
    let state = tempfile::tempdir().unwrap();
    let brief = run(root.path(), state.path(), &["brief", "shared.rs", "--json"]);
    assert!(brief.status.success(), "{brief:?}");
    let value: serde_json::Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(value["empty"], false);
    assert_eq!(
        value["branch_overlap"]["branches"][0]["branch"],
        "refs/heads/sibling"
    );
    let served = run(
        root.path(),
        state.path(),
        &["brief", "shared.rs", "--session", "overlap-test"],
    );
    assert!(served.status.success());
    assert!(
        String::from_utf8_lossy(&served.stdout).contains("branch overlap: \"refs/heads/sibling\"")
    );
    let repeated = run(
        root.path(),
        state.path(),
        &["brief", "shared.rs", "--session", "overlap-test", "--json"],
    );
    assert!(repeated.status.success());
    assert!(repeated.stdout.is_empty(), "{repeated:?}");
    let rehearsal = run(
        root.path(),
        state.path(),
        &["rehearse", "shared.rs", "--json"],
    );
    assert!(rehearsal.status.success(), "{rehearsal:?}");
    let value: serde_json::Value = serde_json::from_slice(&rehearsal.stdout).unwrap();
    assert_eq!(value["objections"][0]["category"], "branch-overlap");
    assert!(
        value["summary_notes"][0]
            .as_str()
            .unwrap()
            .contains("did not run")
    );
    assert!(!root.path().join(".codesage").exists());
}

#[test]
fn empty_overlap_does_not_charge_the_brief_budget_and_corruption_is_not_missing_index() {
    let root = fixture();
    let state = tempfile::tempdir().unwrap();
    let empty = run(
        root.path(),
        state.path(),
        &["brief", "current.rs", "--session", "empty-test"],
    );
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    std::fs::write(root.path().join(".codesage/index.db"), b"corrupt-db").unwrap();
    let corrupt = run(
        root.path(),
        state.path(),
        &["rehearse", "shared.rs", "--json"],
    );
    assert!(!corrupt.status.success());
    assert!(corrupt.stdout.is_empty());
}

#[test]
fn nested_onboarded_project_preserves_project_relative_overlap_paths() {
    let root = fixture_in("module");
    let project = root.path().join("module");
    std::fs::create_dir(project.join(".codesage")).unwrap();
    drop(codesage_storage::Database::open(&project.join(".codesage/index.db")).unwrap());
    let state = tempfile::tempdir().unwrap();
    let brief = run(&project, state.path(), &["brief", "shared.rs", "--json"]);
    assert!(brief.status.success(), "{brief:?}");
    let value: serde_json::Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(
        value["branch_overlap"]["branches"][0]["files"][0],
        "shared.rs"
    );
    let rehearsal = run(&project, state.path(), &["rehearse", "shared.rs", "--json"]);
    assert!(rehearsal.status.success(), "{rehearsal:?}");
    let value: serde_json::Value = serde_json::from_slice(&rehearsal.stdout).unwrap();
    assert!(
        value["objections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["category"] == "branch-overlap" && row["files"][0] == "shared.rs")
    );
}
