//! Compare the structural index's recorded commit with HEAD without reindexing.
//! Matching commits do not attest to working-tree or semantic-index freshness.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use codesage_parser::discover::{DEFAULT_EXCLUDE_PATTERNS, MAX_INDEXABLE_FILE_BYTES};
use codesage_storage::Database;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

/// Indexed paths whose HEAD content is compared. Past it the count is a lower
/// bound and `indexed_files_behind_bounded` says so.
const MAX_DRIFT_CANDIDATES: usize = 2_000;

/// Wall-clock budget for the HEAD-blob comparison. It is checked between
/// records, so it bounds the comparison loop, not a `git` child that never
/// answers.
const DRIFT_CONTENT_BUDGET: Duration = Duration::from_millis(500);

/// A blob past this size is reported affected instead of read. It already
/// changed between the two commits, so a revert back to the indexed bytes is
/// the rare case, and reading it would blow the budget.
const MAX_DRIFT_BLOB_BYTES: u64 = 16 << 20;
/// Hard upper bound for one metadata Git child. Content hashing gets a
/// separate, shorter budget below; startup/configuration hangs must not pin
/// status, overview, or rehearsal callers.
const GIT_COMMAND_BUDGET: Duration = Duration::from_secs(2);
const MAX_GIT_OUTPUT_BYTES: usize = 4 << 20;

fn git_output(cwd: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((MAX_GIT_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let mut reaped = false;
    let result = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok)
        .filter(|bytes| bytes.len() <= MAX_GIT_OUTPUT_BYTES)
        .and_then(|bytes| {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        reaped = true;
                        break if status.success() { Some(bytes) } else { None };
                    }
                    Err(_) => break None,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(None) => break None,
                }
            }
        });
    if result.is_none() && !reaped {
        #[cfg(unix)]
        // SAFETY: the child was placed in its own process group above.
        let _ = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };

        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn git_output_with_stdin(
    cwd: &Path,
    args: &[&str],
    input: &[u8],
    deadline: Instant,
) -> Option<(Option<i32>, Vec<u8>)> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        let _ = stdin.flush();
    });
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((MAX_GIT_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let mut reaped = false;
    let result = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok)
        .filter(|bytes| bytes.len() <= MAX_GIT_OUTPUT_BYTES)
        .and_then(|bytes| {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        reaped = true;
                        break Some((status.code(), bytes));
                    }
                    Err(_) => break None,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(None) => break None,
                }
            }
        });
    if result.is_none() && !reaped {
        #[cfg(unix)]
        // SAFETY: the child was placed in its own process group above.
        let _ = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = writer.join();
    let _ = reader.join();
    result
}

fn git_succeeded(cwd: &Path, args: &[&str], deadline: Instant) -> Option<bool> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    #[cfg(unix)]
    // SAFETY: process_group(0) above created a group owned by this child.
    let _ = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Affected paths named in the drift summary.
const DRIFT_SAMPLE: usize = 5;

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
    /// Indexed files whose HEAD content differs from the content the index
    /// holds, including indexed paths HEAD no longer carries and supported
    /// source files HEAD carries that the index has never seen. This, not
    /// `commits_between`, says whether reindexing would change anything: a
    /// commit range touching only unindexed paths reports `Some(0)`. `None`
    /// when the comparison could not run (no indexed SHA, unreadable HEAD, git
    /// failure).
    pub indexed_files_behind: Option<usize>,
    /// Up to [`DRIFT_SAMPLE`] affected paths, for notes. Empty when none are
    /// affected or the comparison did not run.
    pub indexed_files_behind_sample: Vec<String>,
    /// The comparison stopped at its candidate cap or time budget, so
    /// `indexed_files_behind` is a lower bound. `Some(0)` with this set says
    /// nothing was found before the stop, not that nothing differs.
    pub indexed_files_behind_bounded: bool,
    /// Classification; see [`DriftKind`] for semantics.
    pub kind: DriftKind,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Not a git repo; nothing to measure.
    NotGit,
    /// Git repo but no structural index has ever been stamped.
    NeverIndexed,
    /// Stored SHA matches HEAD.
    Fresh,
    /// HEAD is N commits past the stored SHA on the same history line.
    BehindHead,
    /// Stored SHA is not an ancestor of HEAD. Rebase, branch switch, or force
    /// update. Content divergence is ambiguous by commit count alone.
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

    /// Whether `codesage index` would change structural facts: the index sits
    /// off HEAD *and* at least one indexed file's content differs, or the
    /// content comparison could not run or stopped before finishing. Only a
    /// complete comparison that found nothing clears a commit range. When the
    /// SHA-level test itself failed, only a comparison that ran is evidence.
    pub fn recommends_reindex(&self) -> bool {
        let content_differs =
            self.indexed_files_behind != Some(0) || self.indexed_files_behind_bounded;
        match self.kind {
            DriftKind::BehindHead | DriftKind::UnrelatedAncestor => content_differs,
            DriftKind::Unknown => self.indexed_files_behind.is_some() && content_differs,
            DriftKind::NotGit | DriftKind::NeverIndexed | DriftKind::Fresh => false,
        }
    }

    /// The clause reporting how many indexed files the commit range actually
    /// touched. `None` when the content comparison did not run.
    fn affected_clause(&self) -> Option<String> {
        let n = self.indexed_files_behind?;
        if n == 0 {
            return Some(if self.indexed_files_behind_bounded {
                "no differing indexed file found before the comparison stopped".to_string()
            } else {
                "0 indexed files affected (index content matches HEAD)".to_string()
            });
        }
        let mut clause = format!(
            "{}{n} indexed file{} affected",
            if self.indexed_files_behind_bounded {
                "at least "
            } else {
                ""
            },
            if n == 1 { "" } else { "s" }
        );
        if !self.indexed_files_behind_sample.is_empty() {
            clause.push_str(&format!(
                " ({}{})",
                self.indexed_files_behind_sample.join(", "),
                if n > self.indexed_files_behind_sample.len() {
                    ", …"
                } else {
                    ""
                }
            ));
        }
        Some(clause)
    }

    /// One-line human summary. Safe to print in non-JSON tooling output.
    pub fn summary(&self) -> String {
        let warn = |text: String| {
            if self.recommends_reindex() {
                format!("⚠ {text}")
            } else {
                text
            }
        };
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
                let mut text = format!("index is {commits} behind HEAD");
                if let Some(clause) = self.affected_clause() {
                    text.push_str(&format!(", {clause}"));
                }
                if let (Some(s), Some(h)) = (&self.stored_sha, &self.head_sha) {
                    text.push_str(&format!(" (indexed: {}, HEAD: {})", short(s), short(h)));
                }
                warn(text)
            }
            DriftKind::UnrelatedAncestor => {
                let mut text = match (&self.stored_sha, &self.head_sha) {
                    (Some(s), Some(h)) => format!(
                        "indexed SHA {} is not an ancestor of HEAD {} (rebase/branch switch?)",
                        short(s),
                        short(h)
                    ),
                    _ => {
                        "indexed SHA is not an ancestor of HEAD (rebase/branch switch?)".to_string()
                    }
                };
                if let Some(clause) = self.affected_clause() {
                    text.push_str(&format!("; {clause}"));
                }
                warn(text)
            }
            DriftKind::Unknown => {
                let mut text = "drift check failed (see logs)".to_string();
                if let Some(clause) = self.affected_clause() {
                    text.push_str(&format!("; {clause}"));
                }
                warn(text)
            }
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

    let metadata_deadline = Instant::now() + GIT_COMMAND_BUDGET;
    let head_lookup = git_head_lookup_with_deadline(project_root, metadata_deadline);
    let head_sha = match &head_lookup {
        HeadLookup::Found(sha) => Some(sha.clone()),
        HeadLookup::Missing | HeadLookup::Indeterminate => None,
    };

    let (kind, commits_between) = match (&stored_sha, &head_lookup) {
        (_, HeadLookup::Indeterminate) => (DriftKind::Unknown, None),
        (_, HeadLookup::Missing) => {
            match git_common_dir_lookup_with_deadline(project_root, metadata_deadline) {
                CommonDirLookup::Found(_) if stored_sha.is_none() => {
                    (DriftKind::NeverIndexed, None)
                }
                CommonDirLookup::Found(_) => (DriftKind::Unknown, None),
                CommonDirLookup::NotGit => (DriftKind::NotGit, None),
                CommonDirLookup::Indeterminate => (DriftKind::Unknown, None),
            }
        }
        (None, HeadLookup::Found(_)) => (DriftKind::NeverIndexed, None),
        (Some(stored), HeadLookup::Found(head)) if stored == head => (DriftKind::Fresh, None),
        (Some(stored), HeadLookup::Found(head)) => {
            match commits_between(project_root, stored, head, metadata_deadline) {
                CommitsBetween::Count(n) => (DriftKind::BehindHead, Some(n)),
                CommitsBetween::NotAncestor => (DriftKind::UnrelatedAncestor, None),
                CommitsBetween::Unknown => (DriftKind::Unknown, None),
            }
        }
    };

    // The commit range is only a candidate list; the answer agents act on is
    // whether any *indexed* file's content actually moved.
    let behind = match (kind, &stored_sha, &head_sha) {
        (DriftKind::Fresh, _, _) => Some(FilesBehind::default()),
        (
            DriftKind::BehindHead | DriftKind::UnrelatedAncestor | DriftKind::Unknown,
            Some(stored),
            Some(head),
        ) => measure_indexed_files_behind(
            project_root,
            db,
            &IndexableFilter::for_project(project_root),
            stored,
            head,
            MAX_DRIFT_CANDIDATES,
            DRIFT_CONTENT_BUDGET,
        ),
        _ => None,
    };

    DriftReport {
        stored_sha,
        head_sha,
        stored_at,
        commits_between,
        indexed_files_behind: behind.as_ref().map(|b| b.count),
        indexed_files_behind_sample: behind
            .as_ref()
            .map(|b| b.sample.clone())
            .unwrap_or_default(),
        indexed_files_behind_bounded: behind.as_ref().is_some_and(|b| b.bounded),
        kind,
    }
}

/// Indexed files whose HEAD content differs from the indexed content.
#[derive(Debug, Default)]
struct FilesBehind {
    count: usize,
    sample: Vec<String>,
    bounded: bool,
}

impl FilesBehind {
    fn record(&mut self, path: &str) {
        self.count += 1;
        if self.sample.len() < DRIFT_SAMPLE {
            self.sample.push(path.to_string());
        }
    }
}

/// Would `codesage index` pick up a path the index does not hold? Mirrors
/// discovery for a single path: a supported language, no hidden component,
/// and no match against the default or configured `[index] exclude_patterns`.
/// The two remaining discovery rules, `.gitignore` and regular-file-only, need
/// git and are applied to the surviving candidates by [`retain_discoverable`].
pub struct IndexableFilter {
    excludes: GlobSet,
}

impl IndexableFilter {
    /// [`DEFAULT_EXCLUDE_PATTERNS`] plus `.codesage/config.toml`'s
    /// `[index] exclude_patterns`. A missing, unreadable, or malformed config
    /// adds no patterns, which keeps the unverifiable case on the side that
    /// recommends a reindex.
    pub fn for_project(root: &Path) -> Self {
        let mut patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        patterns.extend(configured_exclude_patterns(root));
        Self::from_patterns(&patterns)
    }

    /// Exactly these patterns; an invalid glob drops only itself.
    pub fn from_patterns(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
            }
        }
        Self {
            excludes: builder.build().unwrap_or_else(|_| GlobSet::empty()),
        }
    }

    fn admits(&self, rel_path: &str) -> bool {
        codesage_parser::detect::detect_language(Path::new(rel_path)).is_some()
            && !rel_path.split('/').any(|segment| segment.starts_with('.'))
            && !path_or_ancestor_excluded(&self.excludes, rel_path)
    }
}

/// `[index] exclude_patterns` from the project config, read without following
/// a symlinked config file.
fn configured_exclude_patterns(root: &Path) -> Vec<String> {
    let path = root.join(".codesage").join("config.toml");
    let is_file = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
    if !is_file {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    toml::from_str::<codesage_embed::config::ProjectConfig>(&text)
        .ok()
        .and_then(|config| config.index)
        .and_then(|index| index.exclude_patterns)
        .unwrap_or_default()
}

/// Discovery prunes a directory whose name matches a glob, so a path is
/// excluded when it or any ancestor matches; directory candidates are tried
/// with a trailing `/` and a child placeholder, the way `**/vendor/**` needs.
fn path_or_ancestor_excluded(excludes: &GlobSet, rel_path: &str) -> bool {
    if excludes.is_match(rel_path) {
        return true;
    }
    let segments: Vec<&str> = rel_path.split('/').collect();
    let mut prefix = String::new();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if excludes.is_match(&prefix)
            || excludes.is_match(format!("{prefix}/"))
            || excludes.is_match(format!("{prefix}/_"))
        {
            return true;
        }
    }
    false
}

/// Intersect the paths `stored..head` touched with the indexed set, then
/// compare each survivor's indexed `content_hash` against its HEAD blob. A
/// touched path the index lacks counts when HEAD carries it and `filter`
/// admits it: a source file added after indexing. `None` when git could not
/// answer; the caller then falls back to the SHA-level classification.
///
/// `budget` starts once the candidate set is known and is consulted between
/// records of the `cat-file` stream, so the `.gitignore` and symlink lookups
/// eat into the comparison loop's time; `git diff` and a `git` child that
/// never writes are not interrupted by it.
fn measure_indexed_files_behind(
    cwd: &Path,
    db: &Database,
    filter: &IndexableFilter,
    stored: &str,
    head: &str,
    max_candidates: usize,
    budget: Duration,
) -> Option<FilesBehind> {
    let deadline = Instant::now() + budget;
    if Instant::now() >= deadline {
        return Some(FilesBehind {
            bounded: true,
            ..FilesBehind::default()
        });
    }
    let changed = match changed_paths(cwd, stored, head, deadline) {
        Some(changed) => changed,
        None if Instant::now() >= deadline => {
            return Some(FilesBehind {
                bounded: true,
                ..FilesBehind::default()
            });
        }
        None => return None,
    };
    let mut behind = FilesBehind::default();
    if changed.is_empty() {
        return Some(behind);
    }
    let indexed = db.all_file_hashes().ok()?;

    let mut candidates: Vec<(String, Option<String>)> = changed
        .into_iter()
        .filter_map(|path| match indexed.get(&path) {
            Some(hash) => Some((path, Some(hash.clone()))),
            None if filter.admits(&path) => Some((path, None)),
            None => None,
        })
        .collect();
    candidates.sort_unstable();
    if candidates.len() > max_candidates {
        candidates.truncate(max_candidates);
        behind.bounded = true;
    }
    if !retain_discoverable(cwd, head, &mut candidates, deadline) {
        behind.bounded = true;
    }
    if candidates.is_empty() {
        return Some(behind);
    }
    compare_head_blobs(cwd, head, &candidates, deadline, &mut behind)?;
    Some(behind)
}

/// Drop the never-indexed candidates discovery would skip even though HEAD
/// carries them: paths `.gitignore` matches (the walker honors it, `git add
/// `-f` does not) and symlinks (the walker takes regular files only, while
/// `cat-file` types a symlink as `blob`). Indexed candidates are kept as they
/// are. A git failure drops nothing, which keeps the unverifiable case on the
/// side that recommends a reindex.
fn retain_discoverable(
    cwd: &Path,
    head: &str,
    candidates: &mut Vec<(String, Option<String>)>,
    deadline: Instant,
) -> bool {
    let unindexed: Vec<&str> = candidates
        .iter()
        .filter(|(_, hash)| hash.is_none())
        .map(|(path, _)| path.as_str())
        .collect();
    if unindexed.is_empty() {
        return true;
    }
    let ignored = gitignored_paths(cwd, &unindexed, deadline);
    let symlinks = symlink_paths(cwd, head, &unindexed, deadline);
    let complete = ignored.is_some() && symlinks.is_some();
    candidates.retain(|(path, hash)| {
        hash.is_some()
            || !(ignored.as_ref().is_some_and(|paths| paths.contains(path))
                || symlinks.as_ref().is_some_and(|paths| paths.contains(path)))
    });
    complete
}

fn gitignored_paths(
    cwd: &Path,
    paths: &[&str],
    deadline: Instant,
) -> Option<std::collections::HashSet<String>> {
    let requests: Vec<u8> = paths
        .iter()
        .flat_map(|path| path.bytes().chain(std::iter::once(0)))
        .collect();
    let (status, bytes) = git_output_with_stdin(
        cwd,
        &["check-ignore", "-z", "--stdin", "--no-index"],
        &requests,
        deadline,
    )?;
    match status {
        Some(0) => Some(
            String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        Some(1) => Some(std::collections::HashSet::new()),
        _ => None,
    }
}

fn symlink_paths(
    cwd: &Path,
    head: &str,
    paths: &[&str],
    deadline: Instant,
) -> Option<std::collections::HashSet<String>> {
    let mut args = vec!["--literal-pathspecs", "ls-tree", "-r", "-z", head, "--"];
    args.extend(paths.iter().copied());
    let bytes = git_output(cwd, &args, deadline)?;
    Some(
        String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter_map(|record| {
                let (meta, path) = record.split_once('\t')?;
                meta.starts_with("120000 ").then(|| path.to_owned())
            })
            .collect(),
    )
}

fn changed_paths(cwd: &Path, stored: &str, head: &str, deadline: Instant) -> Option<Vec<String>> {
    if !is_object_name(stored) || !is_object_name(head) {
        return None;
    }
    let range = format!("{stored}..{head}");
    let bytes = git_output(
        cwd,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--relative",
            &range,
            "--",
        ],
        deadline,
    )?;
    Some(
        String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn is_object_name(sha: &str) -> bool {
    !sha.is_empty() && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

type Candidate<'a> = &'a (String, Option<String>);

fn compare_head_blobs(
    cwd: &Path,
    head: &str,
    candidates: &[(String, Option<String>)],
    deadline: Instant,
    out: &mut FilesBehind,
) -> Option<()> {
    use std::io::{BufRead, BufReader, Write};
    let (askable, unaskable): (Vec<Candidate<'_>>, Vec<Candidate<'_>>) = candidates
        .iter()
        .partition(|(path, _)| !path.contains('\n'));
    for (path, _) in &unaskable {
        out.record(path);
    }
    if askable.is_empty() {
        return Some(());
    }
    let mut command = Command::new("git");
    command
        .args(["cat-file", "--batch"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = Arc::new(Mutex::new(command.spawn().ok()?));
    #[cfg(unix)]
    let child_pid = child.lock().ok()?.id();
    let (mut stdin, stdout) = {
        let mut child = child.lock().ok()?;
        (child.stdin.take()?, child.stdout.take()?)
    };
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog_finished = Arc::clone(&finished);
    let watchdog_child = Arc::clone(&child);
    let watchdog = std::thread::spawn(move || {
        while !watchdog_finished.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !watchdog_finished.load(Ordering::Acquire) {
            #[cfg(unix)]
            // SAFETY: the child was placed in its own process group above.
            let _ = unsafe { libc::killpg(child_pid as libc::pid_t, libc::SIGKILL) };
            if let Ok(mut child) = watchdog_child.lock() {
                let _ = child.kill();
            }
        }
    });
    let requests: String = askable
        .iter()
        .map(|(path, _)| format!("{head}:./{path}\n"))
        .collect();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(requests.as_bytes());
        let _ = stdin.flush();
    });
    let mut reader = BufReader::new(stdout);
    let mut incomplete = false;
    for (path, stored_hash) in askable.iter().copied() {
        if Instant::now() >= deadline {
            incomplete = true;
            break;
        }
        let mut header = String::new();
        if !matches!(reader.read_line(&mut header), Ok(n) if n > 0) {
            incomplete = true;
            break;
        }
        match (
            parse_batch_header(header.trim_end_matches('\n')),
            stored_hash,
        ) {
            (None, Some(_)) => out.record(path),
            (None, None) => {}
            (Some((is_blob, size)), Some(_)) if !is_blob || size > MAX_DRIFT_BLOB_BYTES => {
                out.record(path);
                if !skip_exactly(&mut reader, size + 1) {
                    incomplete = true;
                    break;
                }
            }
            (Some((_, size)), Some(stored_hash)) => {
                let mut bytes = vec![0u8; size as usize];
                let mut trailer = [0u8; 1];
                if reader.read_exact(&mut bytes).is_err()
                    || reader.read_exact(&mut trailer).is_err()
                {
                    incomplete = true;
                    break;
                }
                if codesage_parser::discover::content_hash(&bytes) != *stored_hash {
                    out.record(path);
                }
            }
            (Some((is_blob, size)), None) => {
                if is_blob && size <= MAX_INDEXABLE_FILE_BYTES {
                    out.record(path);
                }
                if !skip_exactly(&mut reader, size + 1) {
                    incomplete = true;
                    break;
                }
            }
        }
    }
    if incomplete {
        out.bounded = true;
    }
    finished.store(true, Ordering::Release);
    let _ = watchdog.join();
    let mut child = child.lock().ok()?;
    #[cfg(unix)]
    // SAFETY: process_group(0) above created a group owned by this child.
    let _ = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.kill();
    let _ = writer.join();
    let _ = child.wait();
    Some(())
}

fn parse_batch_header(header: &str) -> Option<(bool, u64)> {
    let fields: Vec<&str> = header.split(' ').collect();
    if fields.len() != 3
        || fields[0].len() < 40
        || !fields[0].chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    fields[2]
        .parse()
        .ok()
        .map(|size| (fields[1] == "blob", size))
}

fn skip_exactly(reader: &mut impl std::io::Read, mut remaining: u64) -> bool {
    let mut scratch = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(scratch.len() as u64) as usize;
        if std::io::Read::read_exact(reader, &mut scratch[..want]).is_err() {
            return false;
        }
        remaining -= want as u64;
    }
    true
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum HeadLookup {
    Found(String),
    Missing,
    Indeterminate,
}

fn git_head_lookup(cwd: &Path) -> HeadLookup {
    git_head_lookup_with_deadline(cwd, Instant::now() + GIT_COMMAND_BUDGET)
}

fn git_head_lookup_with_deadline(cwd: &Path, deadline: Instant) -> HeadLookup {
    match git_succeeded(cwd, &["rev-parse", "HEAD"], deadline) {
        Some(true) => {}
        Some(false) => return HeadLookup::Missing,
        None => return HeadLookup::Indeterminate,
    }
    let output_deadline = Instant::now() + GIT_COMMAND_BUDGET;
    let Some(bytes) = git_output(cwd, &["rev-parse", "HEAD"], output_deadline) else {
        return HeadLookup::Indeterminate;
    };
    let Ok(sha) = String::from_utf8(bytes) else {
        return HeadLookup::Indeterminate;
    };
    let sha = sha.trim();
    if sha.is_empty() {
        HeadLookup::Missing
    } else {
        HeadLookup::Found(sha.to_string())
    }
}

pub fn git_head_sha(cwd: &Path) -> Option<String> {
    match git_head_lookup(cwd) {
        HeadLookup::Found(sha) => Some(sha),
        HeadLookup::Missing | HeadLookup::Indeterminate => None,
    }
}

pub fn git_head_sha_for_attestation(cwd: &Path) -> anyhow::Result<Option<String>> {
    match git_head_lookup(cwd) {
        HeadLookup::Found(sha) => Ok(Some(sha)),
        HeadLookup::Missing => Ok(None),
        HeadLookup::Indeterminate => {
            anyhow::bail!("could not determine Git HEAD for index attestation")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommonDirLookup {
    Found(PathBuf),
    NotGit,
    Indeterminate,
}

fn git_common_dir_lookup(cwd: &Path) -> CommonDirLookup {
    git_common_dir_lookup_with_deadline(cwd, Instant::now() + GIT_COMMAND_BUDGET)
}

fn git_common_dir_lookup_with_deadline(cwd: &Path, deadline: Instant) -> CommonDirLookup {
    match git_succeeded(cwd, &["rev-parse", "--git-common-dir"], deadline) {
        Some(true) => {}
        Some(false) => return CommonDirLookup::NotGit,
        None => return CommonDirLookup::Indeterminate,
    }
    let output_deadline = Instant::now() + GIT_COMMAND_BUDGET;
    let Some(bytes) = git_output(cwd, &["rev-parse", "--git-common-dir"], output_deadline) else {
        return CommonDirLookup::Indeterminate;
    };
    let Ok(dir) = String::from_utf8(bytes) else {
        return CommonDirLookup::Indeterminate;
    };
    let dir = dir.trim();
    if dir.is_empty() {
        return CommonDirLookup::Indeterminate;
    }
    let path = Path::new(dir);
    CommonDirLookup::Found(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

pub fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    match git_common_dir_lookup(cwd) {
        CommonDirLookup::Found(path) => Some(path),
        CommonDirLookup::NotGit | CommonDirLookup::Indeterminate => None,
    }
}

enum CommitsBetween {
    Count(u32),
    NotAncestor,
    Unknown,
}

fn commits_between(cwd: &Path, a: &str, b: &str, deadline: Instant) -> CommitsBetween {
    match git_succeeded(cwd, &["merge-base", "--is-ancestor", a, b], deadline) {
        Some(true) => {}
        Some(false) => return CommitsBetween::NotAncestor,
        None => return CommitsBetween::Unknown,
    }
    let output_deadline = Instant::now() + GIT_COMMAND_BUDGET;
    let Some(bytes) = git_output(
        cwd,
        &["rev-list", "--count", &format!("{a}..{b}")],
        output_deadline,
    ) else {
        return CommitsBetween::Unknown;
    };
    String::from_utf8_lossy(&bytes)
        .trim()
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
        files: report.indexed_files_behind,
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
    /// Indexed files the range actually touched; `delta` alone over-reports.
    files: Option<usize>,
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
    fn incomplete_discoverability_metadata_keeps_candidates_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut candidates = vec![("new.rs".to_string(), None)];
        assert!(!retain_discoverable(
            dir.path(),
            "not-a-sha",
            &mut candidates,
            Instant::now() + Duration::from_millis(100),
        ));
        assert_eq!(candidates, vec![("new.rs".to_string(), None)]);
    }

    #[cfg(unix)]
    #[test]
    fn stalled_git_child_is_killed_at_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(init.success());
        let fifo = root.join("stall.config");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let config_path = root.join(".git/config");
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str(&format!("\n[include]\n\tpath = {}\n", fifo.display()));
        std::fs::write(&config_path, config).unwrap();

        let started = Instant::now();
        assert_eq!(
            git_succeeded(
                root,
                &["rev-parse", "HEAD"],
                started + Duration::from_millis(100)
            ),
            None
        );
        assert_eq!(
            git_head_lookup_with_deadline(root, started + Duration::from_millis(100)),
            HeadLookup::Indeterminate
        );
        assert_eq!(
            git_common_dir_lookup_with_deadline(root, started + Duration::from_millis(100)),
            CommonDirLookup::Indeterminate
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

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

    /// A behind-HEAD report whose content comparison has not run yet.
    fn behind(commits: Option<u32>) -> DriftReport {
        DriftReport {
            stored_sha: Some("1111111111111111".to_string()),
            head_sha: Some("2222222222222222".to_string()),
            stored_at: Some(0),
            commits_between: commits,
            indexed_files_behind: None,
            indexed_files_behind_sample: Vec::new(),
            indexed_files_behind_bounded: false,
            kind: DriftKind::BehindHead,
        }
    }

    #[test]
    fn drift_report_summary_fresh() {
        let r = DriftReport {
            stored_sha: Some("abcdef123456abcdef".to_string()),
            head_sha: Some("abcdef123456abcdef".to_string()),
            stored_at: Some(0),
            commits_between: None,
            indexed_files_behind: Some(0),
            indexed_files_behind_sample: Vec::new(),
            indexed_files_behind_bounded: false,
            kind: DriftKind::Fresh,
        };
        assert!(r.summary().contains("fresh"));
        assert!(!r.is_drift());
        assert!(!r.recommends_reindex());
    }

    #[test]
    fn drift_report_summary_behind() {
        let r = behind(Some(3));
        let s = r.summary();
        assert!(s.contains("3 commits behind"));
        assert!(r.is_drift());
    }

    #[test]
    fn drift_report_behind_pluralizes() {
        assert!(behind(Some(1)).summary().contains("1 commit behind"));
    }

    #[test]
    fn an_unmeasured_comparison_still_recommends_a_reindex() {
        let r = behind(Some(5));
        assert!(r.recommends_reindex());
        assert!(r.summary().starts_with("⚠"), "{}", r.summary());
        assert!(!r.summary().contains("indexed files affected"));
    }

    #[test]
    fn commits_over_unindexed_paths_do_not_recommend_a_reindex() {
        let mut r = behind(Some(5));
        r.indexed_files_behind = Some(0);
        assert!(!r.recommends_reindex());
        let s = r.summary();
        assert!(
            s.contains("5 commits behind HEAD, 0 indexed files affected"),
            "{s}"
        );
        assert!(s.contains("index content matches HEAD"), "{s}");
        assert!(!s.contains("⚠"), "{s}");
    }

    #[test]
    fn affected_files_are_counted_sampled_and_warned_about() {
        let mut r = behind(Some(5));
        r.indexed_files_behind = Some(7);
        r.indexed_files_behind_sample = vec![
            "a.rs".into(),
            "b.rs".into(),
            "c.rs".into(),
            "d.rs".into(),
            "e.rs".into(),
        ];
        let s = r.summary();
        assert!(s.starts_with("⚠"), "{s}");
        assert!(
            s.contains("7 indexed files affected (a.rs, b.rs, c.rs, d.rs, e.rs, …)"),
            "{s}"
        );
        assert!(r.recommends_reindex());
    }

    #[test]
    fn a_bounded_count_reads_as_a_lower_bound() {
        let mut r = behind(Some(5));
        r.indexed_files_behind = Some(2_000);
        r.indexed_files_behind_bounded = true;
        assert!(
            r.summary().contains("at least 2000 indexed files affected"),
            "{}",
            r.summary()
        );
    }

    #[test]
    fn drift_report_unrelated_ancestor() {
        let mut r = behind(None);
        r.kind = DriftKind::UnrelatedAncestor;
        assert!(r.summary().contains("not an ancestor"));
        assert!(r.is_drift());
        assert!(r.recommends_reindex());

        r.indexed_files_behind = Some(0);
        let s = r.summary();
        assert!(s.contains("not an ancestor"), "{s}");
        assert!(s.contains("0 indexed files affected"), "{s}");
        assert!(!r.recommends_reindex());
    }

    #[test]
    fn a_failed_sha_test_still_recommends_a_reindex_for_a_measured_difference() {
        let mut r = behind(None);
        r.kind = DriftKind::Unknown;
        assert!(!r.recommends_reindex(), "nothing was measured");
        assert_eq!(r.summary(), "drift check failed (see logs)");

        r.indexed_files_behind = Some(0);
        assert!(
            !r.recommends_reindex(),
            "a complete comparison found nothing"
        );
        assert!(!r.summary().starts_with('⚠'), "{}", r.summary());

        r.indexed_files_behind = Some(3);
        r.indexed_files_behind_sample = vec!["a.rs".into(), "b.rs".into(), "c.rs".into()];
        assert!(
            r.recommends_reindex(),
            "the envelope reports behind + files_behind: 3"
        );
        let s = r.summary();
        assert!(s.starts_with('⚠'), "{s}");
        assert!(
            s.contains(
                "drift check failed (see logs); 3 indexed files affected (a.rs, b.rs, c.rs)"
            ),
            "{s}"
        );
    }

    #[test]
    fn a_batch_header_is_parsed_only_from_a_real_object_id() {
        assert_eq!(
            parse_batch_header(&format!("{} blob 12", "a".repeat(40))),
            Some((true, 12))
        );
        assert_eq!(
            parse_batch_header(&format!("{} tree 40", "b".repeat(40))),
            Some((false, 40))
        );
        assert_eq!(parse_batch_header("HEAD:./gone.rs missing"), None);
        // A path that spells a header keeps the stream in sync.
        assert_eq!(parse_batch_header("HEAD:./x blob 12 missing"), None);
    }

    fn git_init(dir: &Path) {
        let status = Command::new("git")
            .arg("init")
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A repository isolated from the caller's signing and hook configuration.
    fn hermetic_repo(dir: &Path) {
        git_init(dir);
        git(dir, &["config", "user.email", "drift@example.invalid"]);
        git(dir, &["config", "user.name", "Drift"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(dir.join(".git/disabled-hooks")).unwrap();
        git(dir, &["config", "core.hooksPath", ".git/disabled-hooks"]);
    }

    fn write_and_commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (rel, body) in files {
            let abs = dir.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, body).unwrap();
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    fn no_excludes() -> IndexableFilter {
        IndexableFilter::from_patterns(&[])
    }

    #[test]
    fn a_bounded_zero_count_still_recommends_a_reindex() {
        let mut r = behind(Some(5));
        r.indexed_files_behind = Some(0);
        r.indexed_files_behind_bounded = true;
        assert!(
            r.recommends_reindex(),
            "a comparison that stopped before its first record proved nothing"
        );
        let s = r.summary();
        assert!(s.starts_with('⚠'), "{s}");
        assert!(
            s.contains("no differing indexed file found before the comparison stopped"),
            "{s}"
        );
        assert!(!s.contains("matches HEAD"), "{s}");
    }

    #[test]
    fn the_indexable_filter_mirrors_discovery_for_a_single_path() {
        let filter = IndexableFilter::from_patterns(&[
            "**/vendor/**".to_string(),
            "generated/**".to_string(),
        ]);
        assert!(filter.admits("src/new.rs"));
        assert!(filter.admits("include/api.h"));
        assert!(!filter.admits("NOTES.md"), "no parser, never indexed");
        assert!(!filter.admits("Cargo.lock"));
        assert!(!filter.admits(".hidden/x.rs"), "hidden component");
        assert!(!filter.admits("src/.cache.rs"), "hidden leaf");
        assert!(!filter.admits("third_party/vendor/lib.rs"), "ancestor glob");
        assert!(!filter.admits("generated/out.rs"), "configured glob");
    }

    #[test]
    fn the_project_filter_reads_configured_exclude_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join(".codesage");
        std::fs::create_dir_all(&cs).unwrap();
        std::fs::write(
            cs.join("config.toml"),
            "[index]\nexclude_patterns = [\"gen/**\"]\n",
        )
        .unwrap();
        let filter = IndexableFilter::for_project(dir.path());
        assert!(!filter.admits("gen/x.rs"));
        assert!(
            !filter.admits("node_modules/x.js"),
            "default patterns apply"
        );
        assert!(filter.admits("src/x.rs"));

        std::fs::write(cs.join("config.toml"), "[index\n").unwrap();
        let broken = IndexableFilter::for_project(dir.path());
        assert!(
            broken.admits("gen/x.rs"),
            "a malformed config adds no patterns"
        );
    }

    #[test]
    fn an_added_source_file_counts_and_an_added_unsupported_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        hermetic_repo(root);
        let base = write_and_commit(root, &[("src/a.rs", "fn a() {}\n")], "base");
        let db = Database::open_in_memory().unwrap();
        index_current(&db, root, &["src/a.rs"]);
        let head = write_and_commit(
            root,
            &[("src/new.rs", "fn new() {}\n"), ("NOTES.md", "# notes\n")],
            "add",
        );

        let out = measure_indexed_files_behind(
            root,
            &db,
            &no_excludes(),
            &base,
            &head,
            MAX_DRIFT_CANDIDATES,
            Duration::from_secs(30),
        )
        .expect("git answered");

        assert_eq!(out.count, 1, "{out:?}");
        assert_eq!(out.sample, vec!["src/new.rs"]);
        assert!(!out.bounded);
    }

    #[test]
    fn a_never_indexed_path_deleted_at_head_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        hermetic_repo(root);
        let base = write_and_commit(
            root,
            &[
                ("src/a.rs", "fn a() {}\n"),
                ("src/skipped.rs", "fn s() {}\n"),
            ],
            "base",
        );
        let db = Database::open_in_memory().unwrap();
        index_current(&db, root, &["src/a.rs"]);
        std::fs::remove_file(root.join("src/skipped.rs")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "drop"]);
        let head = git(root, &["rev-parse", "HEAD"]);

        let out = measure_indexed_files_behind(
            root,
            &db,
            &no_excludes(),
            &base,
            &head,
            MAX_DRIFT_CANDIDATES,
            Duration::from_secs(30),
        )
        .expect("git answered");

        assert_eq!(out.count, 0, "{out:?}");
    }

    fn index_current(db: &Database, dir: &Path, paths: &[&str]) {
        for rel in paths {
            let bytes = std::fs::read(dir.join(rel)).unwrap();
            db.upsert_file(&codesage_protocol::FileInfo {
                path: (*rel).to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: codesage_parser::discover::content_hash(&bytes),
                is_test: false,
            })
            .unwrap();
        }
    }

    /// Two indexed files, both changed by the newest commit.
    fn two_changed_indexed_files() -> (tempfile::TempDir, Database, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        hermetic_repo(root);
        let base = write_and_commit(
            root,
            &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
            "base",
        );
        let db = Database::open_in_memory().unwrap();
        index_current(&db, root, &["src/a.rs", "src/b.rs"]);
        let head = write_and_commit(
            root,
            &[("src/a.rs", "fn a2() {}\n"), ("src/b.rs", "fn b2() {}\n")],
            "edit both",
        );
        (dir, db, base, head)
    }

    #[test]
    fn the_candidate_cap_reports_a_lower_bound() {
        let (dir, db, base, head) = two_changed_indexed_files();

        let capped = measure_indexed_files_behind(
            dir.path(),
            &db,
            &no_excludes(),
            &base,
            &head,
            1,
            Duration::from_secs(30),
        )
        .expect("git answered");
        assert_eq!(capped.count, 1);
        assert!(capped.bounded, "a truncated candidate set is a lower bound");

        let full = measure_indexed_files_behind(
            dir.path(),
            &db,
            &no_excludes(),
            &base,
            &head,
            MAX_DRIFT_CANDIDATES,
            Duration::from_secs(30),
        )
        .expect("git answered");
        assert_eq!(full.count, 2);
        assert!(!full.bounded);
        assert_eq!(full.sample, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn an_exhausted_time_budget_reports_a_lower_bound() {
        let (dir, db, base, head) = two_changed_indexed_files();

        let out = measure_indexed_files_behind(
            dir.path(),
            &db,
            &no_excludes(),
            &base,
            &head,
            MAX_DRIFT_CANDIDATES,
            Duration::ZERO,
        )
        .expect("git answered");

        assert!(out.bounded, "a stopped comparison must disclose itself");
        assert!(out.count < 2, "no path was compared: {out:?}");
    }

    #[test]
    fn a_reverted_file_is_not_behind_even_though_git_saw_two_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        hermetic_repo(root);
        let base = write_and_commit(root, &[("src/a.rs", "fn a() {}\n")], "base");
        let db = Database::open_in_memory().unwrap();
        index_current(&db, root, &["src/a.rs"]);
        write_and_commit(root, &[("src/a.rs", "fn broken() {}\n")], "change");
        let head = write_and_commit(root, &[("src/a.rs", "fn a() {}\n")], "revert");

        let out = measure_indexed_files_behind(
            root,
            &db,
            &no_excludes(),
            &base,
            &head,
            MAX_DRIFT_CANDIDATES,
            Duration::from_secs(30),
        )
        .expect("git answered");

        assert_eq!(out.count, 0, "HEAD content matches the indexed content");
        assert!(!out.bounded);
    }

    #[test]
    fn a_non_hex_stored_sha_never_reaches_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        hermetic_repo(root);
        let head = write_and_commit(root, &[("src/a.rs", "fn a() {}\n")], "base");
        let db = Database::open_in_memory().unwrap();

        assert!(
            changed_paths(
                root,
                "--output=/tmp/pwned",
                &head,
                Instant::now() + Duration::from_secs(1),
            )
            .is_none()
        );
        assert!(
            measure_indexed_files_behind(
                root,
                &db,
                &no_excludes(),
                "HEAD~1",
                &head,
                MAX_DRIFT_CANDIDATES,
                Duration::from_secs(30)
            )
            .is_none()
        );
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
            indexed_files_behind: None,
            indexed_files_behind_sample: Vec::new(),
            indexed_files_behind_bounded: false,
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
            indexed_files_behind: None,
            indexed_files_behind_sample: Vec::new(),
            indexed_files_behind_bounded: false,
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
