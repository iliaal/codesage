use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

const REPOSITORY_ENV: &[&str] = &[
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

/// Construct a Git child whose repository is discovered from `root`, including
/// linked worktrees, rather than inherited repository or object-store selectors.
///
/// Configuration overrides remain inherited: `GIT_CONFIG`,
/// `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, its key/value pairs, and global
/// and system config controls. `GIT_NO_REPLACE_OBJECTS` and unrelated environment
/// also remain inherited. This selects a repository; it does not restrict Git's
/// execution configuration. Callers retain their deadlines and hardening flags.
pub fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(root);
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command
}

/// [`git_command`] for children that read the index or working tree (`diff`
/// against the work tree or index, `ls-files`, `status`). Each child gets
/// `-c core.fsmonitor=false`, so a repository-configured fsmonitor hook command
/// is never executed; command-line configuration outranks local, included, and
/// per-worktree files and an inherited `GIT_CONFIG_PARAMETERS` or
/// `GIT_CONFIG_COUNT` value for the same key. Output is unchanged: Git falls back
/// to its own stat scan. Every other inherited configuration key is preserved.
pub fn git_worktree_command(root: &Path) -> Command {
    let mut command = git_command(root);
    command.args(["-c", "core.fsmonitor=false"]);
    command
}

pub(crate) fn git_history_command(root: &Path) -> Command {
    let mut command = git_command(root);
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// A history walk that ran past its deadline. Its child was killed and no
/// output was used, so callers can tell a budget overrun from a git failure.
#[derive(Debug)]
pub(crate) struct HistoryDeadline {
    pub budget: Duration,
}

impl std::fmt::Display for HistoryDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git history walk truncated by {} ms deadline; index unchanged",
            self.budget.as_millis()
        )
    }
}

impl std::error::Error for HistoryDeadline {}

pub(crate) fn history_output(mut command: Command, budget: Duration) -> Result<Output> {
    use std::io::Read;

    const MAX_OUTPUT_BYTES: u64 = 128 << 20;
    let deadline = Instant::now() + budget;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting git history walk")?;
    let stdout = child.stdout.take().context("opening git history stdout")?;
    let stderr = child.stderr.take().context("opening git history stderr")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(2);
    let readers = [
        (true, Box::new(stdout) as Box<dyn Read + Send>),
        (false, Box::new(stderr) as Box<dyn Read + Send>),
    ]
    .map(|(is_stdout, stream)| {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stream.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes);
            let _ = sender.send((is_stdout, result.map(|_| bytes)));
        })
    });
    drop(sender);
    let mut reaped = false;
    let result = (|| {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for _ in 0..2 {
            let (is_stdout, bytes) = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| HistoryDeadline { budget })?;
            let bytes = bytes.context("reading git history output")?;
            ensure!(
                bytes.len() as u64 <= MAX_OUTPUT_BYTES,
                "git history walk exceeds 128 MiB output limit; index unchanged"
            );
            if is_stdout {
                stdout = bytes;
            } else {
                stderr = bytes;
            }
        }
        loop {
            if let Some(status) = child.try_wait().context("waiting for git history")? {
                reaped = true;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                return Err(HistoryDeadline { budget }.into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    if !reaped {
        crate::drift::kill_git(&mut child);
        let _ = child.wait();
    }
    for reader in readers {
        let _ = reader.join();
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn history_deadline_kills_children_holding_pipes_and_children_without_output() {
        for script in ["sleep 10 & exit 0", "exec >/dev/null 2>/dev/null; sleep 10"] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let start = Instant::now();
            let error = history_output(command, Duration::from_millis(50)).unwrap_err();
            assert!(
                error.to_string().contains("truncated by 50 ms deadline"),
                "{error:#}"
            );
            assert!(start.elapsed() >= Duration::from_millis(50));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn history_children_override_lazy_fetch_without_process_environment_writes() {
        let before = std::env::vars_os().collect::<Vec<_>>();
        let mut command = git_history_command(Path::new("."));
        command.args(["--version"]);
        let overrides: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("GIT_NO_LAZY_FETCH")),
            Some(&Some(std::ffi::OsStr::new("1")))
        );
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("GIT_ALLOW_PROTOCOL")),
            Some(&Some(std::ffi::OsStr::new("")))
        );
        assert!(command.status().unwrap().success());
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
    }
}
