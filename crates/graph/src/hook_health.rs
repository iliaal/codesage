//! Read-side view of the `codesage install-hooks` indexing hooks: which hooks
//! are installed, what `.codesage/hooks.log` last recorded, and whether the
//! single-flight `.codesage/hook-index.lock` is held by a live run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use codesage_protocol::{HookHealth, HookLockState};

use crate::drift::git_common_dir;

/// Hooks `codesage install-hooks` wires for indexing.
pub const INDEXING_HOOKS: &[&str] = &["post-commit", "post-merge", "post-checkout", "post-rewrite"];

/// Marker the installer writes into every hook body it owns.
pub(crate) const HOOK_MARKER: &str = "codesage install-hooks";

pub const LOCK_DIR: &str = ".codesage/hook-index.lock";
pub const LOG_FILE: &str = ".codesage/hooks.log";

/// Age at which a lock without a pid file (written by a pre-pid hook) is an
/// orphan. A lock whose pid is a live hook run is never reaped by age: a
/// first full semantic pass can outlast this. The hook template interpolates
/// this value (in minutes) into its `find -mmin` reap, so the two sites
/// cannot drift; the `.reap` mutex ages out on the same ceiling.
pub const STALE_LOCK_SECS: u64 = 30 * 60;

/// How much of the log tail is scanned for the last run and exit lines.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

pub const LOCK_ABSENT: &str = "absent";
pub const LOCK_HELD_LIVE: &str = "held_live";
pub const LOCK_HELD_DEAD: &str = "held_dead";
pub const LOCK_HELD_NO_PID: &str = "held_no_pid";

/// Directory Git runs hooks from for this checkout; `None` outside a Git
/// repository, even when a global `core.hooksPath` is set. Honors
/// `core.hooksPath` (absolute or root-relative); a Husky runtime dir (`…/_`)
/// is mapped to the user hooks directory beside it, where the installer
/// writes.
pub(crate) fn hooks_dir(root: &Path) -> Option<PathBuf> {
    let common = git_common_dir(root)?;
    let configured = Command::new("git")
        .args(["config", "--type=path", "--get", "core.hooksPath"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match configured {
        Some(raw) => {
            let path = Path::new(&raw);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            if resolved.file_name().is_some_and(|n| n == "_")
                && let Some(parent) = resolved.parent()
                && parent.is_dir()
            {
                return Some(parent.to_path_buf());
            }
            Some(resolved)
        }
        None => Some(common.join("hooks")),
    }
}

/// Names from `INDEXING_HOOKS` whose file in `hooks_dir` carries the marker.
pub(crate) fn installed_hooks(hooks_dir: &Path) -> Vec<String> {
    INDEXING_HOOKS
        .iter()
        .filter(|name| {
            std::fs::read_to_string(hooks_dir.join(name))
                .is_ok_and(|body| body.contains(HOOK_MARKER))
        })
        .map(|name| (*name).to_string())
        .collect()
}

/// Observe without mutating. `None` when `root` is not a Git repository, so
/// that callers can omit the field instead of reporting an empty hook set.
pub fn inspect(root: &Path) -> Option<HookHealth> {
    let dir = hooks_dir(root)?;
    let (last_run, last_exit) = parse_log_tail(&root.join(LOG_FILE));
    Some(HookHealth {
        installed_hooks: installed_hooks(&dir),
        last_run,
        last_exit,
        lock: lock_state_in(root, Some(&dir)),
    })
}

/// Remove a lock no hook run can own: its recorded pid is dead or belongs to
/// an unrelated process, or it carries no pid and is older than
/// `STALE_LOCK_SECS` (the hook applies the same rule on its next fire;
/// doctor, an operator command, does it now). Returns the state that was
/// reaped. A live hook run's lock, whatever its age, and an absent lock are
/// left alone.
///
/// Reapers serialise through the hook's `.reap` mutex directory; under it the
/// state is re-read, the lock renamed away, and removed, so a concurrent hook
/// fire and doctor cannot both believe they reaped it. `Ok(None)` also covers
/// a mutex already held by another reaper.
pub fn reap_orphan_lock(root: &Path) -> Result<Option<HookLockState>> {
    let dir = hooks_dir(root);
    if !is_reapable(&lock_state_in(root, dir.as_deref())) {
        return Ok(None);
    }
    let lockdir = root.join(LOCK_DIR);
    let mutex = root.join(format!("{LOCK_DIR}.reap"));
    // A reaper killed mid-reap leaves the mutex behind; age it out like the hook does.
    if let Ok(meta) = std::fs::symlink_metadata(&mutex)
        && meta.is_dir()
        && meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age.as_secs() >= STALE_LOCK_SECS)
    {
        let _ = std::fs::remove_dir(&mutex);
    }
    match std::fs::create_dir(&mutex) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("creating {}", mutex.display())),
    }
    let _release = ReleaseOnDrop(&mutex);

    let state = lock_state_in(root, dir.as_deref());
    if !is_reapable(&state) {
        return Ok(None);
    }
    let graveyard = root.join(format!("{LOCK_DIR}.dead.{}", std::process::id()));
    match std::fs::rename(&lockdir, &graveyard) {
        Ok(()) => {}
        // A hook fire claimed it first; the lock is gone either way.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(state)),
        Err(e) => return Err(e).with_context(|| format!("renaming {}", lockdir.display())),
    }
    let removed = if graveyard.is_dir() {
        std::fs::remove_dir_all(&graveyard)
    } else {
        std::fs::remove_file(&graveyard)
    };
    removed.with_context(|| format!("removing {}", graveyard.display()))?;
    Ok(Some(state))
}

fn is_reapable(state: &HookLockState) -> bool {
    let stale = state.age_secs.is_some_and(|a| a >= STALE_LOCK_SECS);
    state.state == LOCK_HELD_DEAD || (state.state == LOCK_HELD_NO_PID && stale)
}

struct ReleaseOnDrop<'a>(&'a Path);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(self.0);
    }
}

pub fn lock_state(root: &Path) -> HookLockState {
    lock_state_in(root, hooks_dir(root).as_deref())
}

/// `lock_state` with an already-resolved hooks directory, so callers that
/// hold one do not spawn `git` again.
fn lock_state_in(root: &Path, hooks_dir: Option<&Path>) -> HookLockState {
    let lockdir = root.join(LOCK_DIR);
    let Ok(meta) = std::fs::symlink_metadata(&lockdir) else {
        return HookLockState {
            state: LOCK_ABSENT.to_string(),
            ..HookLockState::default()
        };
    };
    let since_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_secs = since_unix.map(|s| now.saturating_sub(s));
    let since = since_unix.map(format_utc);

    let pid = meta
        .is_dir()
        .then(|| read_pid(&lockdir.join("pid")))
        .flatten();
    let state = match pid {
        None => LOCK_HELD_NO_PID,
        Some(pid) if hook_run_alive(pid, root, hooks_dir) => LOCK_HELD_LIVE,
        Some(_) => LOCK_HELD_DEAD,
    };
    HookLockState {
        state: state.to_string(),
        pid,
        since_unix,
        since,
        age_secs,
        reaped: false,
    }
}

/// A regular, non-symlink file holding one decimal pid.
fn read_pid(pidfile: &Path) -> Option<u32> {
    let meta = std::fs::symlink_metadata(pidfile).ok()?;
    if !meta.is_file() || meta.len() > 32 {
        return None;
    }
    let raw = std::fs::read_to_string(pidfile).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

/// Same rule as the hook template's `hook_alive`: liveness from `kill(pid,
/// 0)`; `ps -o args=` only demotes a live pid whose readable command line
/// names no hook file — `<hooks_dir>/<hook>` spelled absolute or relative to
/// `root`, `.git/hooks/<hook>`, or Husky's `.husky/<hook>` — which is pid
/// reuse, including a recycled pid now running the codesage binary. A
/// missing or BusyBox `ps` never demotes. Pid-bearing locks have no age
/// backstop by design; this check is what frees them.
fn hook_run_alive(pid: u32, root: &Path, hooks_dir: Option<&Path>) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Ok(out) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    let args = String::from_utf8_lossy(&out.stdout);
    let args = args.trim();
    if args.is_empty() {
        return true;
    }
    names_hook_file(args, root, hooks_dir)
}

fn names_hook_file(args: &str, root: &Path, hooks_dir: Option<&Path>) -> bool {
    let mut dirs: Vec<String> = vec![".git/hooks".to_string(), ".husky".to_string()];
    if let Some(dir) = hooks_dir {
        dirs.push(dir.display().to_string());
        if let Ok(rel) = dir.strip_prefix(root) {
            dirs.push(rel.display().to_string());
        }
    }
    INDEXING_HOOKS.iter().any(|name| {
        dirs.iter()
            .any(|dir| args.contains(&format!("{dir}/{name}")))
    })
}

/// `kill(pid, 0)`: ESRCH is the only proof of death. EPERM means a live
/// process owned by another user. Off Unix nothing is provably dead.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // SAFETY: kill with signal 0 performs only the permission and existence
    // check; no signal is delivered and no memory is touched.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
pub(crate) fn pid_alive(_pid: u32) -> bool {
    true
}

/// (timestamp of the newest `hook start` line, exit code of the newest
/// `hook exit=` line at or after it). Scans only the log tail; a missing,
/// non-regular, or unreadable log yields `(None, None)`, and a start line
/// outside the window yields `(None, last_exit)`.
fn parse_log_tail(log: &Path) -> (Option<String>, Option<i32>) {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(meta) = std::fs::symlink_metadata(log) else {
        return (None, None);
    };
    if !meta.is_file() {
        return (None, None);
    }
    let Ok(mut file) = std::fs::File::open(log) else {
        return (None, None);
    };
    let len = meta.len();
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return (None, None);
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&buf);
    parse_log_text(&text, start > 0)
}

fn parse_log_text(text: &str, truncated_head: bool) -> (Option<String>, Option<i32>) {
    let mut lines: Vec<&str> = text.lines().collect();
    if truncated_head {
        // The first line of a mid-file window is a fragment.
        lines.drain(..1.min(lines.len()));
    }
    let mut last_exit = None;
    for line in lines.iter().rev() {
        if let Some(rest) = line.split(" hook exit=").nth(1) {
            if last_exit.is_none() {
                last_exit = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|d| d.parse::<i32>().ok());
            }
            continue;
        }
        if line.contains(" hook start") {
            let stamp = line
                .strip_prefix('[')
                .and_then(|l| l.split_once(']'))
                .map(|(ts, _)| ts.to_string());
            return (stamp, last_exit);
        }
    }
    (None, last_exit)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian, no leap
/// seconds), avoiding a date-time dependency for one field.
pub(crate) fn format_utc(unix: u64) -> String {
    let days = unix / 86_400;
    let secs = unix % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        (secs % 3_600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_utc_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_757_833_777), "2025-09-14T07:09:37Z");
    }

    #[test]
    fn log_tail_reports_the_newest_run_and_its_exit() {
        let text = "[Sun Sep 14 07:00:00 UTC 2026] post-commit hook start pid=10\n\
                    [Sun Sep 14 07:00:05 UTC 2026] index exit=0\n\
                    [Sun Sep 14 07:00:06 UTC 2026] post-commit hook exit=0 pid=10\n\
                    [Sun Sep 14 07:09:37 UTC 2026] post-merge hook start pid=42\n\
                    [Sun Sep 14 07:09:40 UTC 2026] index exit=7\n\
                    [Sun Sep 14 07:09:41 UTC 2026] post-merge hook exit=7 pid=42\n\
                    [Sun Sep 14 07:10:00 UTC 2026] post-merge hook skip: another index already running\n";
        assert_eq!(
            parse_log_text(text, false),
            (Some("Sun Sep 14 07:09:37 UTC 2026".to_string()), Some(7))
        );
    }

    #[test]
    fn log_tail_reports_unknown_exit_when_the_newest_run_never_logged_one() {
        let text = "[Sun Sep 14 06:00:00 UTC 2026] post-commit hook start pid=10\n\
                    [Sun Sep 14 06:00:06 UTC 2026] post-commit hook exit=0 pid=10\n\
                    [Sun Sep 14 07:09:37 UTC 2026] post-merge hook start pid=42\n\
                    [Sun Sep 14 07:09:38 UTC 2026] index exit=0\n";
        assert_eq!(
            parse_log_text(text, false),
            (Some("Sun Sep 14 07:09:37 UTC 2026".to_string()), None)
        );
        assert_eq!(parse_log_text("", false), (None, None));
        assert_eq!(
            parse_log_text("garbage] post-commit hook start\n", true),
            (None, None)
        );
        assert_eq!(
            parse_log_text(
                "start pid=9 fell before the window\n[Sun Sep 14 07:09:41 UTC 2026] post-merge hook exit=7 pid=9\n",
                true
            ),
            (None, Some(7)),
            "an exit parsed inside the window survives a start line outside it"
        );
    }

    #[test]
    fn hook_run_predicate_keys_on_hook_file_names_in_any_spelling() {
        let root = Path::new("/srv/proj");
        let abs = Path::new("/srv/proj/.git/hooks");
        for args in [
            "/bin/sh .git/hooks/post-commit",
            "sh /srv/proj/.git/hooks/post-merge",
            "sh -e .husky/post-checkout",
        ] {
            assert!(
                names_hook_file(args, root, Some(abs)),
                "{args:?} must read as a hook run"
            );
        }
        let custom = Path::new("/srv/proj/tools/git-hooks");
        assert!(names_hook_file(
            "sh tools/git-hooks/post-commit",
            root,
            Some(custom)
        ));
        assert!(names_hook_file(
            "sh /srv/proj/tools/git-hooks/post-commit",
            root,
            Some(custom)
        ));
        for args in [
            "codesage daemon",
            "/home/u/codesage/target/debug/codesage index",
            "sleep 30",
            "sh .git/hooks/pre-commit",
            "sh tools/git-hooks/post-commit",
        ] {
            assert!(
                !names_hook_file(args, root, Some(abs)),
                "{args:?} must not read as an indexing hook run"
            );
        }
    }

    #[test]
    fn stale_lock_ceiling_is_a_whole_number_of_minutes_for_the_template() {
        assert_eq!(
            STALE_LOCK_SECS % 60,
            0,
            "the hook interpolates minutes into find -mmin"
        );
        assert_eq!(STALE_LOCK_SECS / 60, 30);
    }

    #[cfg(unix)]
    #[test]
    fn lock_state_distinguishes_absent_live_dead_and_no_pid() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(lock_state(root).state, LOCK_ABSENT);

        let lockdir = root.join(LOCK_DIR);
        std::fs::create_dir_all(&lockdir).unwrap();
        let no_pid = lock_state(root);
        assert_eq!(no_pid.state, LOCK_HELD_NO_PID);
        assert_eq!(no_pid.pid, None);
        assert!(no_pid.since_unix.is_some() && no_pid.since.is_some());

        let mut holder = Command::new("sh")
            .args(["-c", "sleep 30", ".git/hooks/post-commit"])
            .current_dir(root)
            .spawn()
            .unwrap();
        std::fs::write(lockdir.join("pid"), format!("{}\n", holder.id())).unwrap();
        let live = lock_state(root);
        assert_eq!(
            live.state, LOCK_HELD_LIVE,
            "a git-invoked hook runs as `sh .git/hooks/<hook>`: relative argv must count"
        );
        assert_eq!(live.pid, Some(holder.id()));
        assert!(
            reap_orphan_lock(root).unwrap().is_none(),
            "a fresh live lock is never reaped"
        );
        assert!(lockdir.is_dir());

        let old = SystemTime::now() - std::time::Duration::from_secs(STALE_LOCK_SECS + 60);
        std::fs::File::open(&lockdir)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let stale_live = lock_state(root);
        assert_eq!(stale_live.state, LOCK_HELD_LIVE);
        assert!(stale_live.age_secs.is_some_and(|a| a >= STALE_LOCK_SECS));
        assert!(
            reap_orphan_lock(root).unwrap().is_none(),
            "a live hook run's lock is never reaped, however old"
        );
        assert!(lockdir.is_dir());
        let _ = holder.kill();
        let _ = holder.wait();
        std::fs::remove_dir_all(&lockdir).unwrap();

        std::fs::create_dir_all(&lockdir).unwrap();
        std::fs::File::open(&lockdir)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(lock_state(root).state, LOCK_HELD_NO_PID);
        let reaped = reap_orphan_lock(root)
            .unwrap()
            .expect("a pidless lock past the ceiling is reaped");
        assert_eq!(reaped.state, LOCK_HELD_NO_PID);
        assert!(!lockdir.exists());

        std::fs::create_dir_all(&lockdir).unwrap();
        let mut sleeper = Command::new("sh")
            .args(["-c", "sleep 30", "/usr/local/bin/codesage"])
            .spawn()
            .unwrap();
        std::fs::write(lockdir.join("pid"), sleeper.id().to_string()).unwrap();
        let reused = lock_state(root);
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert_eq!(
            reused.state, LOCK_HELD_DEAD,
            "a live process that only names the codesage binary is no hook run: reuse reads as dead"
        );
        assert!(reap_orphan_lock(root).unwrap().is_some());
        assert!(!lockdir.exists());

        let mutex = root.join(format!("{LOCK_DIR}.reap"));
        std::fs::create_dir(&mutex).unwrap();
        std::fs::File::open(&mutex)
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::create_dir_all(&lockdir).unwrap();
        std::fs::write(lockdir.join("pid"), spawn_and_reap_pid().to_string()).unwrap();
        assert!(
            reap_orphan_lock(root).unwrap().is_some(),
            "a .reap mutex past the ceiling is aged out, not obeyed"
        );
        assert!(!lockdir.exists() && !mutex.exists());

        std::fs::create_dir_all(&lockdir).unwrap();
        let dead = spawn_and_reap_pid();
        std::fs::write(lockdir.join("pid"), dead.to_string()).unwrap();
        let state = lock_state(root);
        assert_eq!(state.state, LOCK_HELD_DEAD);
        assert_eq!(state.pid, Some(dead));
        let reaped = reap_orphan_lock(root).unwrap().unwrap();
        assert_eq!(reaped.pid, Some(dead));
        assert!(!lockdir.exists(), "dead-pid lock must be removed");
        assert_eq!(lock_state(root).state, LOCK_ABSENT);
        assert!(
            std::fs::read_dir(root.join(".codesage")).unwrap().all(|e| {
                let name = e.unwrap().file_name().to_string_lossy().into_owned();
                !name.contains(".dead.") && !name.ends_with(".reap")
            }),
            "no graveyard or mutex directory left behind"
        );

        std::fs::create_dir_all(&lockdir).unwrap();
        std::fs::write(lockdir.join("pid"), dead.to_string()).unwrap();
        let mutex = root.join(format!("{LOCK_DIR}.reap"));
        std::fs::create_dir(&mutex).unwrap();
        assert!(
            reap_orphan_lock(root).unwrap().is_none(),
            "another reaper holds the mutex: leave the lock to it"
        );
        assert!(lockdir.is_dir() && mutex.is_dir());
        std::fs::remove_dir(&mutex).unwrap();
        assert!(reap_orphan_lock(root).unwrap().is_some());
        assert!(!lockdir.exists());
    }

    #[test]
    fn lock_state_ignores_a_symlinked_or_malformed_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lockdir = root.join(LOCK_DIR);
        std::fs::create_dir_all(&lockdir).unwrap();
        std::fs::write(lockdir.join("pid"), "not a pid").unwrap();
        assert_eq!(lock_state(root).state, LOCK_HELD_NO_PID);
        std::fs::remove_file(lockdir.join("pid")).unwrap();
        #[cfg(unix)]
        {
            std::fs::write(root.join("planted"), "1\n").unwrap();
            std::os::unix::fs::symlink(root.join("planted"), lockdir.join("pid")).unwrap();
            assert_eq!(lock_state(root).state, LOCK_HELD_NO_PID);
            assert!(reap_orphan_lock(root).unwrap().is_none());
            assert!(lockdir.is_dir());
        }
    }

    #[test]
    fn inspect_is_none_outside_git_and_lists_marked_hooks_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            inspect(root).is_none(),
            "a non-repository yields nothing even when ~/.gitconfig sets core.hooksPath"
        );
        assert!(hooks_dir(root).is_none());

        for args in [
            &["init", "-q"][..],
            &["config", "core.hooksPath", ".git/hooks"][..],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(
            hooks.join("post-commit"),
            "#!/bin/sh\n# installed by codesage install-hooks\n",
        )
        .unwrap();
        std::fs::write(hooks.join("post-merge"), "#!/bin/sh\necho foreign\n").unwrap();
        let health = inspect(root).expect("git repo yields hook health");
        assert_eq!(health.installed_hooks, vec!["post-commit".to_string()]);
        assert_eq!(health.last_run, None);
        assert_eq!(health.last_exit, None);
        assert_eq!(health.lock.state, LOCK_ABSENT);
        assert!(!health.lock.reaped);
    }

    /// A pid that certainly refers to no live process: a child we spawned and waited for.
    #[cfg(unix)]
    fn spawn_and_reap_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }
}
