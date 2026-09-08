//! Compare the structural index's recorded commit with HEAD without reindexing.
//! Matching commits do not attest to working-tree or semantic-index freshness.

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
    /// Stored SHA matches HEAD.
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

/// Compare the recorded structural-index commit with the current HEAD.
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

    let (kind, commits_between) = match (&stored_sha, &head_sha) {
        // An unborn HEAD is still a Git repository.
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

/// Append a drift record under `project_dir_name`, rotating logs over 1 MiB.
/// Rotation retains at most 10,000 valid records from an 8 MiB tail.
pub fn append_drift_log(
    project_root: &Path,
    project_dir_name: &str,
    report: &DriftReport,
) -> anyhow::Result<()> {
    let dir = project_root.join(project_dir_name);
    // Reject directory symlinks before checking the log's final component.
    if !std::fs::symlink_metadata(&dir)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let path = dir.join("drift.log");

    // A cloned repository can plant the log as a symlink; telemetry must not
    // follow it. The opened handle is checked again under the writer lock.
    if !drift_log_target_is_writable(&path) {
        return Ok(());
    }
    let _lock = crate::state_file::lock(&dir.join("drift.lock"))?;

    if let Ok(meta) = std::fs::metadata(&path) {
        // Avoid scanning small logs solely to count records.
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

    crate::state_file::append_line(&path, format!("{line}\n").as_bytes())?;
    Ok(())
}

fn drift_log_target_is_writable(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_file(),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Bound memory use for repository-supplied logs.
const MAX_ROTATE_BYTES: u64 = 8 << 20;

fn rotate_log(path: &Path) -> anyhow::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = crate::state_file::open(path, false)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_ROTATE_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_ROTATE_BYTES).read_to_end(&mut bytes)?;
    let contents = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |i| &bytes[i + 1..])
    } else {
        &bytes
    };
    let lines: Vec<&[u8]> = contents
        .split(|b| *b == b'\n')
        .filter(|line| serde_json::from_slice::<serde_json::Value>(line).is_ok())
        .collect();
    if start == 0
        && lines.len() <= 10_000
        && lines.len()
            == contents
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .count()
    {
        return Ok(());
    }
    let mut tail = Vec::new();
    for line in &lines[lines.len().saturating_sub(10_000)..] {
        tail.extend_from_slice(line);
        tail.push(b'\n');
    }
    crate::state_file::replace(path, &tail)?;
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

pub fn drift_log_path(project_root: &Path, project_dir_name: &str) -> PathBuf {
    project_root.join(project_dir_name).join("drift.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_append_preserves_valid_records_around_an_interrupted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join(".codesage");
        std::fs::create_dir(&cs).unwrap();
        let path = cs.join("drift.log");
        std::fs::write(&path, b"{\"saved\":true}\n{\"partial\":").unwrap();
        append_drift_log(dir.path(), ".codesage", &drift_report()).unwrap();
        let bytes = std::fs::read_to_string(path).unwrap();
        let records: Vec<serde_json::Value> = bytes
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["saved"], true);
        assert!(records[1].get("ts").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn drift_rotation_retains_valid_tail_of_oversized_invalid_utf8_log() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drift.log");
        let mut bytes = vec![0xff; MAX_ROTATE_BYTES as usize + 1];
        bytes.extend_from_slice(b"\n{\"saved\":true}\n{\"partial\":");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        rotate_log(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"saved\":true}\n");
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn concurrent_drift_rotation_and_appends_keep_every_new_record() {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join(".codesage");
        std::fs::create_dir(&cs).unwrap();
        let path = cs.join("drift.log");
        std::fs::write(
            &path,
            format!("{{\"old\":\"{}\"}}\n", "x".repeat(110)).repeat(10_001),
        )
        .unwrap();
        std::thread::scope(|scope| {
            for n in 0..16 {
                let root = dir.path();
                scope.spawn(move || {
                    let mut report = drift_report();
                    report.head_sha = Some(format!("new-{n}"));
                    append_drift_log(root, ".codesage", &report).unwrap();
                });
            }
        });
        let raw = std::fs::read_to_string(path).unwrap();
        let records: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for n in 0..16 {
            assert_eq!(
                records
                    .iter()
                    .filter(|row| row["head"] == format!("new-{n}"))
                    .count(),
                1
            );
        }
    }

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
    fn append_drift_log_does_not_use_a_planted_legacy_rotation_tmp() {
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
            std::fs::read_to_string(cs.join("drift.log"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_drift_log_refuses_a_symlinked_project_dir() {
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
