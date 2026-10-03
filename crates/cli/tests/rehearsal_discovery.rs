use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(root: &Path, args: &[&str]) {
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("module")).unwrap();
    git(root, &["init", "-q"]);
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.join("module/tracked.rs"), "fn base() {}\n").unwrap();
    std::fs::write(root.join("outside.rs"), "fn base() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    std::fs::write(root.join("module/tracked.rs"), "fn changed() {}\n").unwrap();
    std::fs::write(root.join("outside.rs"), "fn outside() {}\n").unwrap();
    for name in ["fresh.rs", " odd \"é\"\nfile.rs", "ignored.rs"] {
        std::fs::write(root.join("module").join(name), "fn fresh() {}\n").unwrap();
    }
    std::fs::create_dir(root.join("module/.codesage")).unwrap();
    std::fs::write(root.join("module/.codesage/local-state"), "state\n").unwrap();
    dir
}

fn files(output: std::process::Output) -> Vec<String> {
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file.as_str().unwrap().to_string())
        .collect()
}

#[cfg(target_os = "linux")]
fn terminal_command(project: &Path) -> Command {
    use std::io::IsTerminal;

    let terminal = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/ptmx")
        .unwrap();
    assert!(terminal.is_terminal());
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    command
        .current_dir(project)
        .args(["rehearse", "--json"])
        .stdin(terminal);
    command
}

#[cfg(target_os = "linux")]
fn terminal_files(project: &Path) -> Vec<String> {
    files(terminal_command(project).output().unwrap())
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_rehearsal_discovers_nested_tracked_and_untracked_paths() {
    let root = fixture();
    let project = root.path().join("module");
    let mut found = terminal_files(&project);
    found.sort();
    assert_eq!(found, [" odd \"é\"\nfile.rs", "fresh.rs", "tracked.rs"]);
    assert!(!project.join(".codesage/index.db").exists());
}

#[cfg(target_os = "linux")]
fn unborn_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("module");
    git(root, &["init", "-q", "-b", "base"]);
    std::fs::create_dir_all(project.join(".codesage")).unwrap();
    std::fs::write(project.join(".codesage/local-state"), "state\n").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.join("outside.rs"), "fn outside() {}\n").unwrap();
    for name in ["fresh.rs", " odd \"é\"\nfile.rs", "ignored.rs"] {
        std::fs::write(project.join(name), "fn fresh() {}\n").unwrap();
    }
    dir
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_rehearsal_discovers_untracked_files_before_first_commit() {
    let root = unborn_fixture();
    let project = root.path().join("module");
    assert_eq!(
        terminal_files(&project),
        [" odd \"é\"\nfile.rs", "fresh.rs"]
    );
    assert!(!project.join(".codesage/index.db").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_rehearsal_discovers_staged_files_before_first_commit() {
    let root = unborn_fixture();
    git(root.path(), &["add", "--", "module/fresh.rs"]);
    let project = root.path().join("module");
    assert_eq!(
        terminal_files(&project),
        [" odd \"é\"\nfile.rs", "fresh.rs"]
    );
    assert!(!project.join(".codesage/index.db").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_rehearsal_uses_selected_project_with_foreign_git_environment() {
    let foreign = tempfile::tempdir().unwrap();
    git(foreign.path(), &["init", "-q"]);
    std::fs::write(foreign.path().join("decoy.rs"), "fn decoy() {}\n").unwrap();
    git(foreign.path(), &["add", "decoy.rs"]);
    git(foreign.path(), &["commit", "-qm", "foreign"]);
    std::fs::write(foreign.path().join("decoy.rs"), "fn changed_decoy() {}\n").unwrap();

    for (root, expected) in [
        (
            fixture(),
            vec![" odd \"é\"\nfile.rs", "fresh.rs", "tracked.rs"],
        ),
        (unborn_fixture(), vec![" odd \"é\"\nfile.rs", "fresh.rs"]),
    ] {
        let project = root.path().join("module");
        let found = files(
            terminal_command(&project)
                .env("GIT_DIR", foreign.path().join(".git"))
                .env("GIT_COMMON_DIR", foreign.path().join(".git"))
                .env("GIT_WORK_TREE", foreign.path())
                .env("GIT_INDEX_FILE", foreign.path().join(".git/index"))
                .output()
                .unwrap(),
        );
        assert_eq!(found, expected);
        assert!(!project.join(".codesage/index.db").exists());
    }
}

#[test]
fn explicit_rehearsal_paths_take_precedence_over_stdin_and_discovery() {
    let root = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root.path().join("module"))
        .args(["rehearse", "explicit.rs", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"piped.rs\n")
        .unwrap();
    assert_eq!(files(child.wait_with_output().unwrap()), ["explicit.rs"]);
}

#[test]
fn piped_rehearsal_paths_take_precedence_over_discovery() {
    let root = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root.path().join("module"))
        .args(["rehearse", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b" piped.rs \n\nsecond.rs\n")
        .unwrap();
    assert_eq!(
        files(child.wait_with_output().unwrap()),
        ["piped.rs", "second.rs"]
    );
}
