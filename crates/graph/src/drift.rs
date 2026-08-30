//! Structural-index drift instrumentation.
//!
//! Answers: "does the structural/semantic index's last-indexed HEAD SHA match
//! the current git HEAD?" If yes, hooks are firing as intended and the index
//! is fresh. If no, either a git hook missed (husky override, worktree gitlink,
//! missing `codesage install-hooks`) or the user made commits without a
//! triggering event — all cases we need to measure before deciding whether to
//! build the full content-hash backstop (recommendations doc §1.3).
//!
//! This module is **measurement only**. It never auto-reindexes, never raises
//! errors beyond `tracing::debug!` on malformed state, and never blocks a user
//! command. Output surfaces:
//!
//! - `codesage doctor` — a human-readable line under the `index-drift` check.
//! - `codesage status` — one-line indicator when a project is indexed.
//! - MCP server startup — silent append of one JSON line to
//!   `<project>/.codesage/drift.log`. Appended lines are greppable with `jq`
//!   and suitable for computing a drift-rate over a user's session history.

use std::path::{Path, PathBuf};
use std::process::Command;

use codesage_storage::Database;
use serde::Serialize;

/// Current drift state for a project's structural index.
#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    /// SHA the structural index was last built against. `None` means the index
    /// has never been stamped (pre-migration, or no successful `codesage index`
    /// run yet).
    pub stored_sha: Option<String>,
    /// Current `git rev-parse HEAD`. `None` when not a git repo or git is
    /// unavailable.
    pub head_sha: Option<String>,
    /// Unix timestamp of the last stamp, if any.
    pub stored_at: Option<i64>,
    /// Commits in `stored_sha..HEAD`. `None` when either sha is missing or the
    /// stored SHA is not an ancestor of HEAD (branch switch / rebase / shallow
    /// clone). `Some(0)` means fresh.
    pub commits_between: Option<u32>,
    /// Classification — see [`DriftKind`] for semantics.
    pub kind: DriftKind,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Not a git repo — nothing to measure.
    NotGit,
    /// Git repo but no structural index has ever been stamped.
    NeverIndexed,
    /// Stored SHA == HEAD. Hooks are working.
    Fresh,
    /// HEAD is N commits past the stored SHA on the same history line.
    BehindHead,
    /// Stored SHA is not an ancestor of HEAD. Rebase, branch switch, or force
    /// update — content divergence is ambiguous by commit count alone.
    UnrelatedAncestor,
    /// Any structured failure (git not on PATH, shallow clone, etc.). Recorded
    /// rather than hidden so the log keeps a signal.
    Unknown,
}

impl DriftReport {
    /// Used by tests and reserved for future callers (e.g. a future `codesage
    /// drift-report` summary). Keeps the semantic meaning close to the enum.
    #[cfg(test)]
    pub(crate) fn is_drift(&self) -> bool {
        matches!(
            self.kind,
            DriftKind::BehindHead | DriftKind::UnrelatedAncestor
        )
    }

    /// One-line human summary. Safe to print in non-JSON tooling output.
    pub fn summary(&self) -> String {
        match self.kind {
            DriftKind::NotGit => "not a git repository".to_string(),
            DriftKind::NeverIndexed => {
                "structural index has never been stamped (run `codesage index`)".to_string()
            }
            DriftKind::Fresh => match (&self.head_sha, &self.stored_at) {
                (Some(h), Some(at)) => format!("fresh (HEAD {} indexed {})", short(h), fmt_ts(*at)),
                (Some(h), None) => format!("fresh (HEAD {})", short(h)),
                _ => "fresh".to_string(),
            },
            DriftKind::BehindHead => {
                let commits = self
                    .commits_between
                    .map(|n| format!("{n} commit{}", if n == 1 { "" } else { "s" }))
                    .unwrap_or_else(|| "unknown".to_string());
                match (&self.stored_sha, &self.head_sha) {
                    (Some(s), Some(h)) => format!(
                        "⚠ index is {commits} behind HEAD (indexed: {}, HEAD: {})",
                        short(s),
                        short(h)
                    ),
                    _ => format!("⚠ index is {commits} behind HEAD"),
                }
            }
            DriftKind::UnrelatedAncestor => match (&self.stored_sha, &self.head_sha) {
                (Some(s), Some(h)) => format!(
                    "⚠ indexed SHA {} is not an ancestor of HEAD {} (rebase/branch switch?)",
                    short(s),
                    short(h)
                ),
                _ => "⚠ indexed SHA is not an ancestor of HEAD (rebase/branch switch?)".to_string(),
            },
            DriftKind::Unknown => "drift check failed (see logs)".to_string(),
        }
    }
}

/// Drop `sha` to 12 hex chars for display. Leaves non-hex input untouched so a
/// malformed stamp still shows up verbatim in the log.
fn short(sha: &str) -> String {
    if sha.len() > 12 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        sha[..12].to_string()
    } else {
        sha.to_string()
    }
}

/// Format a unix timestamp as a short relative-time string ("3 hours ago",
/// "just now", "5 days ago"). Avoids pulling in chrono for one line of
/// user-facing output.
fn fmt_ts(unix: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let delta = now - unix;
    if delta < 0 {
        return format!("in the future? ts={unix}");
    }
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3600 {
        let m = delta / 60;
        return format!("{m} minute{} ago", if m == 1 { "" } else { "s" });
    }
    if delta < 86_400 {
        let h = delta / 3600;
        return format!("{h} hour{} ago", if h == 1 { "" } else { "s" });
    }
    let d = delta / 86_400;
    format!("{d} day{} ago", if d == 1 { "" } else { "s" })
}

/// Compute the drift report for `project_root`. Reads `structural_index_state`
/// from `db` and queries git for the current HEAD. Never panics; returns
/// `DriftKind::Unknown` when git/rusqlite surface a structured error.
pub fn check_drift(project_root: &Path, db: &Database) -> DriftReport {
    let (stored_sha, stored_at) = match db.get_structural_index_state() {
        Ok(Some((sha, at))) => (Some(sha), Some(at)),
        Ok(None) => (None, None),
        Err(e) => {
            tracing::debug!(error = %e, "read structural_index_state failed");
            (None, None)
        }
    };

    let head_sha = git_head_sha(project_root);

    // Single `commits_between` spawn: derive both the classification and the
    // count from one result instead of running the git pair twice.
    let (kind, commits_between) = match (&stored_sha, &head_sha) {
        // No HEAD SHA: distinguish a repo with an unborn HEAD (fresh `git
        // init`, `checkout --orphan` — no commits yet) from a non-repo. Both
        // yield `head_sha == None`, but only the latter is `NotGit`.
        (_, None) => {
            if git_common_dir(project_root).is_some() {
                (DriftKind::NeverIndexed, None)
            } else {
                (DriftKind::NotGit, None)
            }
        }
        (None, Some(_)) => (DriftKind::NeverIndexed, None),
        (Some(stored), Some(head)) if stored == head => (DriftKind::Fresh, None),
        (Some(stored), Some(head)) => match commits_between(project_root, stored, head) {
            CommitsBetween::Count(n) => (DriftKind::BehindHead, Some(n)),
            CommitsBetween::NotAncestor => (DriftKind::UnrelatedAncestor, None),
            CommitsBetween::Unknown => (DriftKind::Unknown, None),
        },
    };

    DriftReport {
        stored_sha,
        head_sha,
        stored_at,
        commits_between,
        kind,
    }
}

/// `git rev-parse HEAD`, returning the full SHA string. `None` when git fails
/// or the repo has no HEAD (fresh `git init`, for example).
pub fn git_head_sha(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Resolve the canonical git common directory (the actual `.git`, even from
/// inside a worktree) for `cwd`. Returns `None` when not a git repo or git is
/// unavailable. Result paths are absolute.
pub fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("rev-parse")
        .arg("--git-common-dir")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8(out.stdout).ok()?;
    let dir = dir.trim();
    if dir.is_empty() {
        return None;
    }
    let path = Path::new(dir);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

enum CommitsBetween {
    Count(u32),
    NotAncestor,
    Unknown,
}

/// `git rev-list --count a..b`. Returns `NotAncestor` when the stored SHA is
/// not an ancestor of HEAD (git prints 0 in that case too, so we explicitly
/// test ancestry first to avoid conflating rebases with freshness).
fn commits_between(cwd: &Path, a: &str, b: &str) -> CommitsBetween {
    let ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(cwd)
        .status();
    match ancestor {
        Ok(s) if s.success() => {}
        Ok(_) => return CommitsBetween::NotAncestor,
        Err(_) => return CommitsBetween::Unknown,
    }
    let out = Command::new("git")
        .args(["rev-list", "--count", &format!("{a}..{b}")])
        .current_dir(cwd)
        .output();
    let Ok(out) = out else {
        return CommitsBetween::Unknown;
    };
    if !out.status.success() {
        return CommitsBetween::Unknown;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    raw.trim()
        .parse::<u32>()
        .map(CommitsBetween::Count)
        .unwrap_or(CommitsBetween::Unknown)
}

/// Append one JSON-line drift record to `<project>/.codesage/drift.log`.
/// Truncates the log to the last 10,000 lines on entry to keep growth
/// bounded — roughly a year of once-per-session records.
pub fn append_drift_log(
    project_root: &Path,
    project_dir_name: &str,
    report: &DriftReport,
) -> anyhow::Result<()> {
    let dir = project_root.join(project_dir_name);
    // lstat, not `exists()`: a repo-planted `.codesage` *directory* symlink
    // would otherwise redirect the whole log path out of the project tree, and
    // the per-file guard below only inspects the final component.
    if !std::fs::symlink_metadata(&dir)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let path = dir.join("drift.log");

    // Refuse a symlinked or otherwise non-regular target, for the log itself
    // and for the sibling the rotation renames over it. `.codesage/` is part of
    // the repository work tree, so a hostile repo can ship either name as a
    // git symlink (mode 120000): the append would put a repo-controlled JSON
    // line at an arbitrary file's EOF, and the rotation's write-then-rename
    // would replace an arbitrary file wholesale. Same posture as the hooks.log
    // guard in the post-commit hook template and the session snapshot writer:
    // skip the telemetry write, never follow the link.
    if !drift_log_target_is_writable(&path) {
        return Ok(());
    }

    // Bounded rotation: if the log has grown past 10k lines, keep the tail.
    if let Ok(meta) = std::fs::metadata(&path) {
        // Cheap guard: only do the rewrite when file is larger than ~1 MiB.
        // Below that, line count won't exceed 10k for any plausible record size.
        if meta.len() > 1 << 20 {
            rotate_log(&path)?;
        }
    }

    let line = serde_json::to_string(&DriftLogLine {
        ts: now_unix(),
        stored: report.stored_sha.as_deref(),
        head: report.head_sha.as_deref(),
        delta: report.commits_between,
        kind: report.kind,
    })?;

    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// True when both `drift.log` and the `drift.log.tmp` the rotation writes are
/// safe to touch: each is either absent or an existing regular file. A symlink,
/// fifo, or directory at either name means the write would land somewhere the
/// project does not own, so the caller skips the record entirely.
fn drift_log_target_is_writable(path: &Path) -> bool {
    let regular_or_absent = |p: &Path| match std::fs::symlink_metadata(p) {
        Ok(meta) => meta.is_file(),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    };
    regular_or_absent(path) && regular_or_absent(&rotation_tmp_path(path))
}

fn rotation_tmp_path(path: &Path) -> PathBuf {
    path.with_extension("log.tmp")
}

/// Largest drift log the rotation will read back into memory.
///
/// Rotation only ever keeps the last 10k records, so anything past this is
/// discarded wholesale rather than loaded: the log is repository-supplied
/// content, and a cloned repo can commit an arbitrarily large regular file at
/// this path. Well above the 1 MiB rotation trigger, so ordinary logs still
/// rotate by keeping their tail.
const MAX_ROTATE_BYTES: u64 = 8 << 20;

fn rotate_log(path: &Path) -> anyhow::Result<()> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_ROTATE_BYTES {
        // Too large to tail cheaply: start the log over instead of reading it.
        std::fs::write(path, b"")?;
        return Ok(());
    }
    let contents = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() <= 10_000 {
        return Ok(());
    }
    let tail = lines[lines.len() - 10_000..].join("\n");
    let tmp = rotation_tmp_path(path);
    std::fs::write(&tmp, format!("{tail}\n"))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Serialize)]
struct DriftLogLine<'a> {
    ts: i64,
    stored: Option<&'a str>,
    head: Option<&'a str>,
    delta: Option<u32>,
    kind: DriftKind,
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Helper for tests and log tooling: path to the drift log. Public so callers
/// outside this crate can inspect the file `append_drift_log` writes.
pub fn drift_log_path(project_root: &Path, project_dir_name: &str) -> PathBuf {
    project_root.join(project_dir_name).join("drift.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_truncates_hex() {
        assert_eq!(short("0123456789abcdef0123"), "0123456789ab");
    }

    #[test]
    fn short_leaves_non_hex_untouched() {
        assert_eq!(short("not-a-git-repo"), "not-a-git-repo");
    }

    #[test]
    fn drift_report_summary_fresh() {
        let r = DriftReport {
            stored_sha: Some("abcdef123456abcdef".to_string()),
            head_sha: Some("abcdef123456abcdef".to_string()),
            stored_at: Some(0),
            commits_between: None,
            kind: DriftKind::Fresh,
        };
        assert!(r.summary().contains("fresh"));
        assert!(!r.is_drift());
    }

    #[test]
    fn drift_report_summary_behind() {
        let r = DriftReport {
            stored_sha: Some("1111111111111111".to_string()),
            head_sha: Some("2222222222222222".to_string()),
            stored_at: Some(0),
            commits_between: Some(3),
            kind: DriftKind::BehindHead,
        };
        let s = r.summary();
        assert!(s.contains("3 commits behind"));
        assert!(r.is_drift());
    }

    #[test]
    fn drift_report_behind_pluralizes() {
        let r = DriftReport {
            stored_sha: Some("1111111111111111".to_string()),
            head_sha: Some("2222222222222222".to_string()),
            stored_at: Some(0),
            commits_between: Some(1),
            kind: DriftKind::BehindHead,
        };
        assert!(r.summary().contains("1 commit behind"));
    }

    #[test]
    fn drift_report_unrelated_ancestor() {
        let r = DriftReport {
            stored_sha: Some("1111111111111111".to_string()),
            head_sha: Some("2222222222222222".to_string()),
            stored_at: Some(0),
            commits_between: None,
            kind: DriftKind::UnrelatedAncestor,
        };
        assert!(r.summary().contains("not an ancestor"));
        assert!(r.is_drift());
    }

    fn git_init(dir: &Path) {
        let status = Command::new("git")
            .arg("init")
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
    }

    #[test]
    fn unborn_head_repo_is_not_classified_notgit() {
        // Fresh `git init` with no commits: HEAD is unborn so `git rev-parse
        // HEAD` fails, but it is still a real repo. Must not be reported as
        // "not a git repository".
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let db = Database::open_in_memory().unwrap();

        let report = check_drift(dir.path(), &db);

        assert_eq!(report.kind, DriftKind::NeverIndexed);
        assert!(!report.is_drift());
        assert_ne!(report.kind, DriftKind::NotGit);
    }

    #[test]
    fn non_repo_dir_is_classified_notgit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();

        let report = check_drift(dir.path(), &db);

        assert_eq!(report.kind, DriftKind::NotGit);
    }

    #[test]
    fn drift_report_not_git() {
        let r = DriftReport {
            stored_sha: None,
            head_sha: None,
            stored_at: None,
            commits_between: None,
            kind: DriftKind::NotGit,
        };
        assert!(!r.is_drift());
        assert_eq!(r.summary(), "not a git repository");
    }

    #[cfg(unix)]
    fn drift_report() -> DriftReport {
        DriftReport {
            kind: DriftKind::BehindHead,
            stored_sha: Some("\"; touch /tmp/pwned; #".to_string()),
            head_sha: Some("deadbeef".to_string()),
            stored_at: None,
            commits_between: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_refuses_a_symlinked_log() {
        // A hostile repo ships `.codesage/drift.log` as a symlink; the append
        // would put one repo-controlled JSON line at the target's EOF.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let victim = root.join("victim.rc");
        std::fs::write(&victim, b"# victim\n").unwrap();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::os::unix::fs::symlink(&victim, root.join(".codesage/drift.log")).unwrap();

        append_drift_log(root, ".codesage", &drift_report()).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"# victim\n");
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_refuses_a_dangling_symlinked_log() {
        // A dangling link would otherwise have the append *create* the file at
        // an attacker-chosen path.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("created-by-attacker");
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::os::unix::fs::symlink(&target, root.join(".codesage/drift.log")).unwrap();

        append_drift_log(root, ".codesage", &drift_report()).unwrap();

        assert!(!target.exists(), "the append created the symlink's target");
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_refuses_a_symlinked_rotation_tmp() {
        // The >1 MiB rotation writes `drift.log.tmp` and renames it over the
        // log; a symlink there turns the write into a full-file replacement.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let victim = root.join("victim.rc");
        std::fs::write(&victim, b"# victim\n").unwrap();
        let cs = root.join(".codesage");
        std::fs::create_dir_all(&cs).unwrap();
        std::fs::write(cs.join("drift.log"), b"{}\n").unwrap();
        std::os::unix::fs::symlink(&victim, cs.join("drift.log.tmp")).unwrap();

        append_drift_log(root, ".codesage", &drift_report()).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"# victim\n");
        assert_eq!(
            std::fs::read_to_string(cs.join("drift.log")).unwrap(),
            "{}\n",
            "the record must be skipped, not appended, while the tmp name is unsafe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_refuses_a_symlinked_project_dir() {
        // `.codesage` itself can be a planted directory symlink.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".codesage")).unwrap();

        append_drift_log(&root, ".codesage", &drift_report()).unwrap();

        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "the record was written through the symlinked project dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotation_discards_an_oversized_log_without_reading_it() {
        // The log is repo-supplied; a huge regular file must not be slurped.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cs = root.join(".codesage");
        std::fs::create_dir_all(&cs).unwrap();
        let log = cs.join("drift.log");
        std::fs::write(&log, vec![b'x'; (MAX_ROTATE_BYTES + 1) as usize]).unwrap();

        rotate_log(&log).unwrap();

        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_writes_an_ordinary_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        append_drift_log(root, ".codesage", &drift_report()).unwrap();

        let log = std::fs::read_to_string(root.join(".codesage/drift.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "expected exactly one record: {log}");
        assert!(
            log.contains("\"head\":\"deadbeef\""),
            "unexpected record: {log}"
        );
    }
}
