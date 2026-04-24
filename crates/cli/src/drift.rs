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
pub(crate) struct DriftReport {
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
pub(crate) enum DriftKind {
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
    pub(crate) fn summary(&self) -> String {
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
pub(crate) fn check_drift(project_root: &Path, db: &Database) -> DriftReport {
    let (stored_sha, stored_at) = match db.get_structural_index_state() {
        Ok(Some((sha, at))) => (Some(sha), Some(at)),
        Ok(None) => (None, None),
        Err(e) => {
            tracing::debug!(error = %e, "read structural_index_state failed");
            (None, None)
        }
    };

    let head_sha = git_head_sha(project_root);

    let kind = match (&stored_sha, &head_sha) {
        (_, None) => DriftKind::NotGit,
        (None, Some(_)) => DriftKind::NeverIndexed,
        (Some(stored), Some(head)) if stored == head => DriftKind::Fresh,
        (Some(stored), Some(head)) => match commits_between(project_root, stored, head) {
            CommitsBetween::Count(_) => DriftKind::BehindHead,
            CommitsBetween::NotAncestor => DriftKind::UnrelatedAncestor,
            CommitsBetween::Unknown => DriftKind::Unknown,
        },
    };

    let commits_between = match &kind {
        DriftKind::BehindHead => match (&stored_sha, &head_sha) {
            (Some(s), Some(h)) => match commits_between(project_root, s, h) {
                CommitsBetween::Count(n) => Some(n),
                _ => None,
            },
            _ => None,
        },
        _ => None,
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
pub(crate) fn git_head_sha(cwd: &Path) -> Option<String> {
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
pub(crate) fn append_drift_log(
    project_root: &Path,
    project_dir_name: &str,
    report: &DriftReport,
) -> anyhow::Result<()> {
    let dir = project_root.join(project_dir_name);
    if !dir.exists() {
        return Ok(());
    }
    let path = dir.join("drift.log");

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

fn rotate_log(path: &Path) -> anyhow::Result<()> {
    let contents = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() <= 10_000 {
        return Ok(());
    }
    let tail = lines[lines.len() - 10_000..].join("\n");
    let tmp = path.with_extension("log.tmp");
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

/// Helper for tests: path to the drift log. Kept `pub(crate)` so integration
/// tests from other crate modules can inspect it.
#[allow(dead_code)]
pub(crate) fn drift_log_path(project_root: &Path, project_dir_name: &str) -> PathBuf {
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
}
