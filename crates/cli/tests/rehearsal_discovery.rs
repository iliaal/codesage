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

fn rehearse_explicit(cwd: &Path, env: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    rehearse_explicit_without(cwd, env, &[])
}

fn rehearse_explicit_without(
    cwd: &Path,
    env: &[(&str, &std::ffi::OsStr)],
    removed: &[&str],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    for name in removed {
        command.env_remove(name);
    }
    command
        .current_dir(cwd)
        .args(["rehearse", "x.rs", "--json"])
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// `outer/.codesage` holds an unreadable index, so resolving to `outer`
/// instead of the nested `inner` repository fails the rehearsal.
fn nested_repository() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let outer = dir.path().canonicalize().unwrap().join("outer");
    std::fs::create_dir_all(outer.join(".codesage")).unwrap();
    std::fs::write(outer.join(".codesage/index.db"), "not a database").unwrap();
    let inner = outer.join("inner");
    std::fs::create_dir_all(inner.join("src")).unwrap();
    git(&inner, &["init", "-q"]);
    (dir, inner.join("src"))
}

#[test]
fn rehearsal_resolves_the_nested_repository_when_git_refuses_ownership() {
    let (_dir, cwd) = nested_repository();
    assert_eq!(files(rehearse_explicit(&cwd, &[])), ["x.rs"]);
    // `safe.directory` is honored only from system, global, and command-line
    // or environment-scoped config, so dropping all of them keeps a
    // `safe.directory = *` from overriding the simulated foreign owner.
    let output = rehearse_explicit_without(
        &cwd,
        &[
            ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1".as_ref()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".as_ref()),
            ("GIT_CONFIG_NOSYSTEM", "1".as_ref()),
        ],
        &["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"],
    );
    assert_eq!(files(output), ["x.rs"]);
}

#[test]
fn rehearsal_falls_back_to_the_git_entry_when_git_is_not_installed() {
    let (_dir, cwd) = nested_repository();
    let empty = tempfile::tempdir().unwrap();
    let output = rehearse_explicit(&cwd, &[("PATH", empty.path().as_os_str())]);
    assert_eq!(files(output), ["x.rs"]);
}

#[test]
fn rehearsal_refuses_when_only_a_git_rejected_ancestor_entry_exists() {
    let Some(dir) = repository_free_dir() else {
        return skip("no candidate directory where Git finds no repository");
    };
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("stray/.git")).unwrap();
    let cwd = base.join("stray/work");
    std::fs::create_dir_all(&cwd).unwrap();
    let output = rehearse_explicit(&cwd, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no CodeSage project or Git repository found"),
        "{stderr}"
    );
}

/// A missing precondition fails under CI, where it would hide a regression,
/// and is reported as a skip on a developer host.
fn skip(reason: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "precondition missing under CI: {reason}"
    );
    eprintln!("skipping {reason}");
}

/// Discovery runs here because no `.codesage` is nearer than the `.git`
/// entry. An inherited `GIT_DIR` would make Git report the start directory as
/// the toplevel, and a ceiling at the repository would hide it.
#[cfg(target_os = "linux")]
#[test]
fn terminal_rehearsal_root_ignores_inherited_git_dir_and_ceiling() {
    let root = fixture();
    std::fs::remove_dir_all(root.path().join("module/.codesage")).unwrap();
    let project = root.path().join("module");
    let expected = terminal_files(&project);
    assert!(
        expected.iter().any(|file| file == "outside.rs"),
        "{expected:?}"
    );

    let foreign = tempfile::tempdir().unwrap();
    git(foreign.path(), &["init", "-q"]);
    let found = files(
        terminal_command(&project)
            .env("GIT_DIR", foreign.path().join(".git"))
            .env(
                "GIT_CEILING_DIRECTORIES",
                root.path().canonicalize().unwrap(),
            )
            .output()
            .unwrap(),
    );
    assert_eq!(found, expected);
}

/// The variables `codesage_graph::git_command` strips from every Git child.
const STRIPPED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_REFERENCE_BACKEND",
];

/// A fresh directory from which Git, run as CodeSage runs it, finds no
/// repository in any parent directory. The system temp directory can sit
/// under a repository or on its own mount, where Git stops at the boundary
/// with a different refusal, so `~/.cache` is tried too.
fn repository_free_dir() -> Option<tempfile::TempDir> {
    let cache = std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache"));
    for base in std::iter::once(std::env::temp_dir()).chain(cache) {
        if std::fs::create_dir_all(&base).is_err() {
            continue;
        }
        let Ok(dir) = tempfile::Builder::new()
            .prefix("codesage-discovery-")
            .tempdir_in(&base)
        else {
            continue;
        };
        let mut probe = Command::new("git");
        probe
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(dir.path())
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        for name in STRIPPED_GIT_ENV {
            probe.env_remove(name);
        }
        let Ok(output) = probe.output() else {
            continue;
        };
        if output.status.code() == Some(128)
            && String::from_utf8_lossy(&output.stderr).lines().any(|line| {
                line.starts_with("fatal: not a git repository (or any of the parent directories)")
            })
        {
            return Some(dir);
        }
    }
    None
}
