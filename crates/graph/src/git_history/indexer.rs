//! Git history indexer: `git log` pass, decay math, txn-wrapped writes.
//!
//! Source patterns from repowise's git_indexer.py, re-implemented from algorithm:
//! - one subprocess for the whole repo
//! - exponential decay (τ=180 days) on commit age, measured from HEAD's own
//!   committer epoch rather than the wall clock, so a pinned checkout is not
//!   flattened (or emptied outright) by how long ago it was tagged; see
//!   [`HistoryAnchor`]
//! - per-commit churn weight = decay * min((added+deleted)/100, 3.0); the clamp
//!   prevents one historic refactor from dominating forever
//! - co-change pair weight = sum over commits where both files appear, weighted by decay
//! - min visible co-change count = 3 (retain smaller counts for incremental indexing)
//! - soft-skip `chore:` / `build:` commits UNLESS message contains migrate/refactor/adopt/deprecate
//! - no-merges only (merge commits double-count work already in their parents)
//!
//! Recurrence (`git_co_changes.window_mask` / `windows`): every shared commit
//! sets bit `(ts / 90d) % 64` in the pair's mask, numbered against the unix
//! epoch rather than any commit, so a full scan and an incremental scan set
//! the same bits and `--incremental` composes exactly (`mask |= delta`).
//! `windows = popcount(mask)`. The 64-bit ring wraps only for commits 64
//! windows (~15.8 years) apart, far beyond `HISTORY_WINDOW_DAYS`; a full scan
//! never sees two windows that share a bit, and the const assert below keeps
//! that true if either constant moves. Incremental passes keep bits older
//! than the history window until the next `--full` rebaselines the row.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result, anyhow};
use codesage_parser::discover::{
    DEFAULT_EXCLUDE_PATTERNS, TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set,
};
use codesage_protocol::GitIndexStats;
use codesage_storage::Database;
use codesage_storage::db::CoChangeWrite;
use globset::GlobSet;

use super::bus_factor::normalized_author;

const DECAY_TAU_DAYS: f64 = 180.0;
const SECONDS_PER_DAY: f64 = 86_400.0;
const CHURN_CLAMP: f64 = 3.0;
const CHURN_DIVISOR: f64 = 100.0;
const MIN_CO_CHANGE_COUNT: u32 = 3;
/// Bound onboarding cost by excluding old commits whose decayed weight is small.
/// Measured back from the pass's [`HistoryAnchor`], not from the wall clock.
pub const HISTORY_WINDOW_DAYS: f64 = 730.0;
/// Bound quadratic pair generation and suppress broad mechanical co-changes.
const MAX_FILES_PER_COMMIT_FOR_COCHANGE: usize = 30;
/// Cell width of the fixed 90-day calendar grid (from the unix epoch) used
/// for the recurrence window mask. Commits in different cells set different
/// bits regardless of how close in time they are; distance-based recurrence
/// is decided downstream from the observation span, not from this grid.
const RECURRENCE_WINDOW_DAYS: f64 = 90.0;
const RECURRENCE_WINDOW_SECS: i64 = (RECURRENCE_WINDOW_DAYS * SECONDS_PER_DAY) as i64;
/// Width of the window-mask ring. The full scan's history bound must fit
/// inside it with room to spare, or two windows the scan can both see would
/// share a bit.
const RECURRENCE_RING: i64 = 64;
const _: () = assert!(
    (HISTORY_WINDOW_DAYS / RECURRENCE_WINDOW_DAYS) as i64 + 2 < RECURRENCE_RING,
    "history window must span fewer sub-windows than the 64-bit mask ring"
);

#[derive(Debug, Default)]
struct FileStats {
    churn_score: f64,
    fix_count: u32,
    total_commits: u32,
    last_commit_at: Option<i64>,
}

#[derive(Debug, Default)]
struct PairStats {
    weight: f64,
    count: u32,
    first_observed_at: Option<i64>,
    last_observed_at: Option<i64>,
    /// Bit `(ts / 90d) % 64` set for every shared commit; see the module doc.
    window_mask: u64,
}

impl PairStats {
    fn write(&self) -> CoChangeWrite {
        CoChangeWrite {
            weight: self.weight,
            count: self.count,
            window_mask: self.window_mask,
            first_observed_at: self.first_observed_at,
            last_observed_at: self.last_observed_at,
        }
    }
}

/// Ring index of the fixed-epoch 90-day window holding `timestamp`.
fn recurrence_window(timestamp: i64) -> u32 {
    ((timestamp.max(0) / RECURRENCE_WINDOW_SECS) % RECURRENCE_RING) as u32
}

fn window_bit(timestamp: i64) -> u64 {
    1u64 << recurrence_window(timestamp)
}

/// Which clock a pass measures the history window and the churn decay from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAnchorSource {
    /// HEAD's own committer epoch. Output is then a function of (repo, HEAD),
    /// so a pinned checkout reads the same on whatever day it is indexed.
    HeadCommit,
    /// Wall clock, reached only when there is no HEAD to read: an empty
    /// repository, or no usable git.
    WallClock,
}

/// Reference point for [`HISTORY_WINDOW_DAYS`] and the churn decay.
///
/// Measuring from the wall clock empties `git_files` and `git_co_changes`
/// entirely for a checkout whose newest commit predates the window, and
/// flattens churn into noise well before that, so a pass measures from HEAD's
/// own committer epoch instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryAnchor {
    pub epoch: i64,
    pub source: HistoryAnchorSource,
}

impl HistoryAnchor {
    pub(crate) fn indexed(db: &Database) -> Result<Option<Self>> {
        Ok(db.git_history_anchor()?.and_then(|(source, epoch)| {
            let source = match source.as_str() {
                "HEAD" => HistoryAnchorSource::HeadCommit,
                "now" => HistoryAnchorSource::WallClock,
                _ => return None,
            };
            Some(Self { epoch, source })
        }))
    }

    fn persist(self, db: &Database, sha: &str, full: bool) -> Result<()> {
        let established = full || Self::indexed(db)?.is_some_and(|old| old.source == self.source);
        let source = match self.source {
            HistoryAnchorSource::HeadCommit => "HEAD",
            HistoryAnchorSource::WallClock => "now",
        };
        db.set_git_index_state_with_anchor(sha, established.then_some((source, self.epoch)))
    }

    /// Oldest commit timestamp this pass admits.
    pub fn cutoff(&self) -> i64 {
        history_window_cutoff(self.epoch)
    }

    /// Stamp for operator-facing notes. The two regimes have to stay
    /// distinguishable, or an empty answer cannot name its cause.
    pub fn window_stamp(&self) -> String {
        let days = HISTORY_WINDOW_DAYS as i64;
        match self.source {
            HistoryAnchorSource::HeadCommit => format!("{days}d@HEAD"),
            HistoryAnchorSource::WallClock => format!("{days}d@now"),
        }
    }
}

/// Anchor at the already-resolved HEAD commit. Every pass resolves HEAD before
/// choosing a mode, so the anchor costs one `git log -1`, memoized per (root,
/// SHA); a read-side consumer that holds no path uses
/// [`history_predates_wall_clock_window`] against the newest timestamp its
/// rows carry instead.
fn anchor_at_commit(root: &Path, sha: &str) -> HistoryAnchor {
    match commit_epoch(root, sha) {
        Some(epoch) => HistoryAnchor {
            epoch,
            source: HistoryAnchorSource::HeadCommit,
        },
        None => {
            tracing::warn!(
                %sha,
                root = %root.display(),
                "no committer date for HEAD; measuring history from the wall clock"
            );
            wall_clock_anchor()
        }
    }
}

fn wall_clock_anchor() -> HistoryAnchor {
    HistoryAnchor {
        epoch: unix_now(),
        source: HistoryAnchorSource::WallClock,
    }
}

/// Whether the newest commit an index holds predates a wall-clock window, so
/// the history is visible only because the anchor follows HEAD. Read-side
/// callers pass the newest timestamp they already hold: a row's
/// `last_commit_at`, or the upper bound of
/// [`codesage_storage::Database::co_change_history_span`].
pub fn history_predates_wall_clock_window(newest_commit_at: i64, now: i64) -> bool {
    newest_commit_at < history_window_cutoff(now)
}

/// Time references for one pass: where this pass measures from, and the anchor
/// the already-stored rows were last decayed to.
#[derive(Debug, Clone, Copy)]
struct PassAnchors {
    current: HistoryAnchor,
    previous: i64,
}

type CommitEpochKey = (PathBuf, String);

/// One entry per (root, HEAD) pair; bounds a daemon that indexes for weeks.
const COMMIT_EPOCH_MEMO_CAP: usize = 256;

fn commit_epoch_memo() -> &'static Mutex<HashMap<CommitEpochKey, i64>> {
    static MEMO: OnceLock<Mutex<HashMap<CommitEpochKey, i64>>> = OnceLock::new();
    MEMO.get_or_init(Mutex::default)
}

/// The map carries no invariant a panicking holder could have broken.
fn lock_memo<K, V>(memo: &Mutex<HashMap<K, V>>) -> MutexGuard<'_, HashMap<K, V>> {
    memo.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Committer epoch of `sha`, memoized per (root, SHA). `sha` must be a resolved
/// object name: the memo assumes an immutable date, which a symbolic revision
/// such as `HEAD` would not have.
fn commit_epoch(root: &Path, sha: &str) -> Option<i64> {
    // `--` would not neutralize an attached-value option such as `-O/path`.
    if sha.starts_with('-') {
        return None;
    }
    let key = (root.to_path_buf(), sha.to_string());
    let memo = commit_epoch_memo();
    if let Some(hit) = lock_memo(memo).get(&key) {
        return Some(*hit);
    }
    let epoch = read_commit_epoch(root, sha)?;
    let mut guard = lock_memo(memo);
    if guard.len() >= COMMIT_EPOCH_MEMO_CAP {
        guard.clear();
    }
    guard.insert(key, epoch);
    Some(epoch)
}

fn read_commit_epoch(root: &Path, sha: &str) -> Option<i64> {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%ct", sha, "--"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    std::str::from_utf8(&out.stdout).ok()?.trim().parse().ok()
}

/// Indexing mode. `Auto` is the recommended default — reuses prior state if valid,
/// falls back to full rescan otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// Re-scan the whole history. Drops existing git_files/git_co_changes first.
    Full,
    /// Only scan commits after the last recorded SHA. Falls back to Full if state
    /// is missing, corrupted, or the prior SHA isn't an ancestor of HEAD.
    Incremental,
    /// Incremental if state is valid, else full. Default for hooks and CLI.
    Auto,
}

/// Full scan with no extra excludes.
pub fn git_history_index(db: &Database, root: &Path) -> Result<GitIndexStats> {
    git_history_index_with_options(db, root, &[], IndexMode::Full)
}

/// Index history with additional excludes and the requested scan mode.
pub fn git_history_index_with_options(
    db: &Database,
    root: &Path,
    extra_excludes: &[String],
    mode: IndexMode,
) -> Result<GitIndexStats> {
    let (exclude_set, test_like_set) = compile_excludes(extra_excludes)?;
    let head_sha = resolve_head_sha(root)?;
    let anchor = anchor_at_commit(root, &head_sha);
    log_anchor(root, anchor);

    let effective_mode = match mode {
        IndexMode::Full => IndexMode::Full,
        IndexMode::Incremental | IndexMode::Auto => match db.get_git_index_state()? {
            Some((last_sha, last_indexed_at)) if last_sha == head_sha => {
                // HEAD is unchanged, so the anchor is too: the decay below is a
                // no-op unless the last pass measured from a different clock.
                let previous = previous_anchor(root, &last_sha, last_indexed_at, anchor);
                db.execute_batch(|db| {
                    decay_git_history_between(db, previous, anchor.epoch)?;
                    db.prune_git_author_events(anchor.cutoff())?;
                    anchor.persist(db, &head_sha, false)
                })?;
                return Ok(GitIndexStats {
                    commits_scanned: 0,
                    files_tracked: 0,
                    co_change_pairs: 0,
                });
            }
            Some((last_sha, _)) => {
                if is_ancestor(root, &last_sha, &head_sha)? {
                    IndexMode::Incremental
                } else {
                    IndexMode::Full
                }
            }
            None => IndexMode::Full,
        },
    };

    match effective_mode {
        IndexMode::Full => run_full(db, root, &exclude_set, &test_like_set, &head_sha, anchor),
        IndexMode::Incremental => {
            let (last_sha, last_at) = db
                .get_git_index_state()?
                .expect("incremental path checked state present above");
            let anchors = PassAnchors {
                current: anchor,
                previous: previous_anchor(root, &last_sha, last_at, anchor),
            };
            run_incremental(
                db,
                root,
                &exclude_set,
                &test_like_set,
                &head_sha,
                &last_sha,
                anchors,
            )
        }
        IndexMode::Auto => unreachable!("resolved above"),
    }
}

/// Make the regime visible: an empty or flat history has to be traceable to its
/// anchor without a rerun.
fn log_anchor(root: &Path, anchor: HistoryAnchor) {
    let window = anchor.window_stamp();
    if history_predates_wall_clock_window(anchor.epoch, unix_now()) {
        tracing::warn!(
            root = %root.display(),
            %window,
            anchor = anchor.epoch,
            "HEAD predates a wall-clock history window; measuring history from HEAD"
        );
    } else {
        tracing::debug!(
            root = %root.display(),
            %window,
            anchor = anchor.epoch,
            "git history anchor"
        );
    }
}

/// The anchor the previous pass measured from, recovered from the SHA it
/// recorded. A commit's date is immutable, so this reproduces that pass's
/// reference exactly; the recorded wall-clock stamp is the fallback for a SHA
/// git can no longer read. Rows written before the anchor followed HEAD were
/// decayed to the wall clock, so a checkout more than a few months old needs
/// one `git-index --full` to rebaseline.
fn previous_anchor(
    root: &Path,
    last_sha: &str,
    last_indexed_at: i64,
    current: HistoryAnchor,
) -> i64 {
    match current.source {
        HistoryAnchorSource::HeadCommit => commit_epoch(root, last_sha).unwrap_or(last_indexed_at),
        HistoryAnchorSource::WallClock => last_indexed_at,
    }
}

/// Returns two glob sets:
/// - `hard_exclude`: files that don't enter `git_files` at all (vendor, build
///   outputs, binaries, lock files, generated docs).
/// - `test_like`: files retained in `git_files` and source-test pairs, but
///   excluded from test-test pairs.
fn compile_excludes(extra: &[String]) -> Result<(GlobSet, GlobSet)> {
    let mut hard: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    hard.extend(extra.iter().cloned());
    let hard_set =
        build_exclude_set(&hard).with_context(|| "compiling git history hard-exclude patterns")?;

    let test_patterns: Vec<String> = TEST_LIKE_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let test_set = build_exclude_set(&test_patterns)
        .with_context(|| "compiling test-like exclude patterns")?;

    Ok((hard_set, test_set))
}

fn run_full(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
    anchor: HistoryAnchor,
) -> Result<GitIndexStats> {
    let raw = run_git_log(root, None, anchor.cutoff())?;
    let commits = parse_log(&raw);

    let mut files: HashMap<String, FileStats> = HashMap::new();
    let mut pairs: HashMap<(String, String), PairStats> = HashMap::new();
    let mut commits_scanned = 0usize;

    for commit in &commits {
        let Some(kept_changes) = filter_kept(commit, exclude_set) else {
            continue;
        };
        commits_scanned += 1;
        accumulate(
            &mut files,
            &mut pairs,
            commit,
            &kept_changes,
            anchor.epoch,
            test_like_set,
        );
    }

    // Replace the index atomically and avoid one durable commit per row.
    let mut co_change_kept = 0usize;
    db.execute_batch(|db| {
        db.clear_git_data()?;
        db.reset_git_authors()?;
        write_author_events(db, &commits, exclude_set)?;
        for (path, stats) in &files {
            db.upsert_git_file(
                path,
                stats.churn_score,
                stats.fix_count,
                stats.total_commits,
                stats.last_commit_at,
            )?;
        }
        for ((a, b), stats) in &pairs {
            db.upsert_git_co_change_full(a, b, &stats.write())?;
            if stats.count >= MIN_CO_CHANGE_COUNT {
                co_change_kept += 1;
            }
        }
        anchor.persist(db, head_sha, true)?;
        Ok(())
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
    })
}

fn run_incremental(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
    last_sha: &str,
    anchors: PassAnchors,
) -> Result<GitIndexStats> {
    let anchor = anchors.current;
    let range = format!("{last_sha}..{head_sha}");
    let raw = run_git_log(root, Some(&range), anchor.cutoff())?;
    let commits = parse_log(&raw);

    let mut files: HashMap<String, FileStats> = HashMap::new();
    let mut pairs: HashMap<(String, String), PairStats> = HashMap::new();
    let mut commits_scanned = 0usize;

    for commit in &commits {
        let Some(kept_changes) = filter_kept(commit, exclude_set) else {
            continue;
        };
        commits_scanned += 1;
        accumulate(
            &mut files,
            &mut pairs,
            commit,
            &kept_changes,
            anchor.epoch,
            test_like_set,
        );
    }

    // Commit decay and deltas together so a crash cannot leave only the decay applied.
    let mut co_change_kept = 0usize;
    db.execute_batch(|db| {
        decay_git_history_between(db, anchors.previous, anchor.epoch)?;
        db.prune_git_author_events(anchor.cutoff())?;
        write_author_events(db, &commits, exclude_set)?;
        for (path, stats) in &files {
            db.incr_git_file(
                path,
                stats.churn_score,
                stats.fix_count,
                stats.total_commits,
                stats.last_commit_at,
            )?;
        }
        for ((a, b), stats) in &pairs {
            db.incr_git_co_change_full(a, b, &stats.write())?;
            if db.co_change_pair_exists(a, b)? {
                co_change_kept += 1;
            }
        }
        anchor.persist(db, head_sha, false)?;
        Ok(())
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
    })
}

/// Oldest commit timestamp a pass measuring from `anchor` admits. Clamped at
/// the epoch so `git log --since=@<n>` never receives a negative second.
fn history_window_cutoff(anchor: i64) -> i64 {
    (anchor - (HISTORY_WINDOW_DAYS * SECONDS_PER_DAY) as i64).max(0)
}

fn write_author_events(db: &Database, commits: &[Commit], excludes: &GlobSet) -> Result<()> {
    for commit in commits {
        let Some(changes) = filter_kept(commit, excludes) else {
            continue;
        };
        for change in changes {
            db.upsert_git_author_event(
                &change.path,
                &commit.sha,
                &commit.author,
                commit.timestamp,
            )?;
        }
    }
    Ok(())
}

/// Age stored weights from the anchor they were computed at to this pass's
/// anchor, so incremental deltas compose into the same totals as a full scan.
fn decay_git_history_between(db: &Database, from_anchor: i64, to_anchor: i64) -> Result<()> {
    let delta_seconds = (to_anchor - from_anchor).max(0) as f64;
    if delta_seconds > 0.0 {
        let factor = (-delta_seconds / (DECAY_TAU_DAYS * SECONDS_PER_DAY)).exp();
        db.scale_git_decay(factor)?;
    }
    Ok(())
}

fn filter_kept<'a>(commit: &'a Commit, exclude_set: &GlobSet) -> Option<Vec<&'a FileChange>> {
    if soft_skip(&commit.subject) {
        return None;
    }
    let kept: Vec<&FileChange> = commit
        .changes
        .iter()
        .filter(|c| !is_excluded(exclude_set, &c.path))
        .collect();
    if kept.is_empty() { None } else { Some(kept) }
}

fn accumulate(
    files: &mut HashMap<String, FileStats>,
    pairs: &mut HashMap<(String, String), PairStats>,
    commit: &Commit,
    kept_changes: &[&FileChange],
    anchor: i64,
    test_like_set: &GlobSet,
) {
    let age_days = ((anchor - commit.timestamp).max(0) as f64) / SECONDS_PER_DAY;
    let decay = (-age_days / DECAY_TAU_DAYS).exp();
    let is_fix = is_fix_commit(&commit.subject);

    for change in kept_changes {
        let stats = files.entry(change.path.clone()).or_default();
        stats.total_commits += 1;
        if is_fix {
            stats.fix_count += 1;
        }
        if stats.last_commit_at.is_none_or(|t| t < commit.timestamp) {
            stats.last_commit_at = Some(commit.timestamp);
        }
        let churn_units = ((change.added + change.deleted) as f64) / CHURN_DIVISOR;
        let weight = decay * churn_units.min(CHURN_CLAMP);
        stats.churn_score += weight;
    }

    // Source-test pairs support test recommendations; test-test pairs add noise.
    if kept_changes.len() <= MAX_FILES_PER_COMMIT_FOR_COCHANGE {
        for i in 0..kept_changes.len() {
            for j in (i + 1)..kept_changes.len() {
                let a = &kept_changes[i].path;
                let b = &kept_changes[j].path;
                if test_like_set.is_match(a) && test_like_set.is_match(b) {
                    continue;
                }
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                let pair = pairs.entry((lo.clone(), hi.clone())).or_default();
                pair.weight += decay;
                pair.count += 1;
                pair.window_mask |= window_bit(commit.timestamp);
                if pair.first_observed_at.is_none_or(|t| t > commit.timestamp) {
                    pair.first_observed_at = Some(commit.timestamp);
                }
                if pair.last_observed_at.is_none_or(|t| t < commit.timestamp) {
                    pair.last_observed_at = Some(commit.timestamp);
                }
            }
        }
    }
}

fn resolve_head_sha(root: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| format!("git rev-parse HEAD in {}", root.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8(out.stdout)
        .context("HEAD SHA not UTF-8")?
        .trim()
        .to_string())
}

/// Repo-relative paths changed between the merge base of `git_ref` and `HEAD`
/// and `HEAD` itself. Unresolvable refs return an error, not an empty set.
pub fn changed_files_since(
    root: &Path,
    git_ref: &str,
) -> Result<std::collections::HashSet<String>> {
    // Appending `...HEAD` does not neutralize attached-value options such as `-O/path`.
    if git_ref.starts_with('-') {
        return Err(anyhow!(
            "invalid git ref `{git_ref}`: must not start with '-'"
        ));
    }
    let range = format!("{git_ref}...HEAD");
    let out = Command::new("git")
        // `--` terminates option parsing so `range` is always read as a
        // revision range, never as flags.
        .args(["diff", "--name-only", "--relative", &range, "--"])
        .current_dir(root)
        .output()
        .with_context(|| format!("git diff --name-only {range} in {}", root.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git diff --name-only {range} failed (is `{git_ref}` a valid ref?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8(out.stdout).context("git diff output not UTF-8")?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect())
}

/// Match changed entry, owned, or context files. Entry-only slices count;
/// test-role changes belong to the test suite's own slice.
pub fn feature_touched_since(
    files: &[codesage_protocol::FeatureFileRef],
    changed: &std::collections::HashSet<String>,
) -> bool {
    use codesage_protocol::FeatureFileRole;
    files.iter().any(|f| {
        matches!(
            f.role,
            FeatureFileRole::Entry | FeatureFileRole::Owned | FeatureFileRole::Context
        ) && changed.contains(&f.path)
    })
}

fn is_ancestor(root: &Path, old: &str, new: &str) -> Result<bool> {
    // Missing ancestry or an unavailable SHA requires a full scan; spawn errors propagate.
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .current_dir(root)
        .status()
        .with_context(|| {
            format!(
                "spawning `git merge-base --is-ancestor {old} {new}` in {}",
                root.display()
            )
        })?;
    if !status.success() && status.code() == Some(128) {
        tracing::warn!(
            %old,
            "git rejected old SHA (likely rewritten history); falling back to full rescan"
        );
    }
    Ok(status.success())
}

#[derive(Debug, Default)]
struct Commit {
    sha: String,
    author: String,
    timestamp: i64,
    subject: String,
    changes: Vec<FileChange>,
}

#[derive(Debug)]
struct FileChange {
    path: String,
    added: u32,
    deleted: u32,
}

fn run_git_log(root: &Path, range: Option<&str>, since_epoch: i64) -> Result<String> {
    // `@<unix>` is git's epoch-format specifier; locale-independent.
    let since_arg = format!("--since=@{since_epoch}");
    let mut args: Vec<&str> = vec![
        "log",
        "--no-merges",
        "--numstat",
        "--pretty=format:commit\x09%H\x09%ct\x09%ae%x1f%an%x1f%s",
        &since_arg,
    ];
    if let Some(r) = range {
        args.push(r);
    }
    let output = Command::new("git")
        .args(&args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running git log in {}", root.display()))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git log failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).context("git log output not UTF-8")
}

fn parse_log(raw: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    let mut current: Option<Commit> = None;
    let mut skipped_commits = 0usize;
    let mut skipped_changes = 0usize;
    // Keep a bounded sample of dropped commit SHAs so the warning can name
    // which commits were skipped, not just how many.
    let mut skipped_shas: Vec<String> = Vec::new();

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("commit\t") {
            if let Some(prev) = current.take() {
                commits.push(prev);
            }
            let mut parts = rest.splitn(3, '\t');
            let _sha = parts.next().unwrap_or("");
            // A fabricated timestamp of zero would still affect fix counts despite zero churn.
            let ts: Option<i64> = parts.next().and_then(|s| s.parse().ok());
            let Some(ts) = ts else {
                skipped_commits += 1;
                if skipped_shas.len() < 20 && !_sha.is_empty() {
                    skipped_shas.push(_sha.to_string());
                }
                current = None;
                continue;
            };
            let remaining = parts.next().unwrap_or("");
            let author_fields: Vec<&str> = remaining.splitn(3, '\u{1f}').collect();
            let (author, subject) = if let [email, name, subject] = author_fields.as_slice() {
                (normalized_author(email, name).unwrap_or_default(), *subject)
            } else {
                (String::new(), remaining)
            };
            current = Some(Commit {
                sha: _sha.to_string(),
                author,
                timestamp: ts,
                subject: subject.to_string(),
                changes: Vec::new(),
            });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some(commit) = current.as_mut() else {
            continue;
        };
        // numstat line: "<added>\t<deleted>\t<path>"; binary files use "-\t-\t<path>"
        let mut parts = line.splitn(3, '\t');
        let added_s = parts.next().unwrap_or("-");
        let deleted_s = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("");
        if path.is_empty() || added_s == "-" || deleted_s == "-" {
            continue;
        }
        // Malformed counts must not become zero-churn history entries.
        let (Ok(added), Ok(deleted)) = (added_s.parse::<u32>(), deleted_s.parse::<u32>()) else {
            skipped_changes += 1;
            continue;
        };
        // Rename detection in numstat looks like `path/{old => new}/file`. Normalize
        // to the destination by stripping `{old => ` and `}`.
        let normalized = normalize_rename_path(path);
        commit.changes.push(FileChange {
            path: normalized,
            added,
            deleted,
        });
    }
    if let Some(prev) = current.take() {
        commits.push(prev);
    }
    if skipped_commits > 0 || skipped_changes > 0 {
        tracing::warn!(
            skipped_commits,
            skipped_changes,
            skipped_shas = ?skipped_shas,
            "parse_log skipped unparseable git output entries"
        );
    }
    commits
}

fn normalize_rename_path(raw: &str) -> String {
    // git format examples:
    //   src/{foo.rs => bar.rs}
    //   src/{old => new}/inner/file.rs
    //   {old/dir => new/dir}/file.rs
    //   src/old.rs => src/new.rs          (braceless: whole-path move)
    //   src/FOO.rs => src/foo.rs          (braceless: case-only rename)
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}'))
        && open < close
        && let Some(arrow) = raw[open..close].find(" => ")
    {
        let prefix = &raw[..open];
        let after_arrow_in_braces = &raw[open + arrow + 4..close];
        let suffix = &raw[close + 1..];
        return format!("{prefix}{after_arrow_in_braces}{suffix}");
    }
    // Braceless renames carry full paths, including case-only renames.
    // Empty sides indicate a literal arrow rather than a rename.
    if let Some((src, dest)) = raw.rsplit_once(" => ")
        && !src.is_empty()
        && !dest.is_empty()
    {
        return dest.to_string();
    }
    raw.to_string()
}

fn is_fix_commit(subject: &str) -> bool {
    let s = subject.to_ascii_lowercase();
    s.starts_with("fix:")
        || s.starts_with("fix(")
        || s.starts_with("bugfix:")
        || s.starts_with("hotfix:")
        || s.contains(" fix ")
        || s.contains(" fixes ")
        || s.contains("fixes #")
        || s.contains("fixes gh-")
        || s.contains("closes #")
}

fn soft_skip(subject: &str) -> bool {
    let s = subject.to_ascii_lowercase();
    let is_chore = s.starts_with("chore:")
        || s.starts_with("chore(")
        || s.starts_with("build:")
        || s.starts_with("build(")
        || s.starts_with("ci:")
        || s.starts_with("style:")
        || s.starts_with("docs:");
    if !is_chore {
        return false;
    }
    let rescued = s.contains("migrate")
        || s.contains("refactor")
        || s.contains("adopt")
        || s.contains("deprecate")
        || s.contains("upgrade");
    !rescued
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_excluded(set: &GlobSet, path: &str) -> bool {
    set.is_match(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_files_since_rejects_dash_prefixed_ref() {
        let dir = tempfile::tempdir().unwrap();
        let err = changed_files_since(dir.path(), "-O/etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("must not start with '-'"),
            "expected leading-dash rejection, got: {err}"
        );
    }

    #[test]
    fn feature_touched_since_matches_entry_owned_context_not_test() {
        use codesage_protocol::{FeatureFileRef, FeatureFileRole};
        let f = |path: &str, role| FeatureFileRef {
            path: path.to_string(),
            role,
            reason: None,
        };
        let changed: std::collections::HashSet<String> =
            ["src/main.rs".to_string(), "tests/it.rs".to_string()]
                .into_iter()
                .collect();

        assert!(feature_touched_since(
            &[f("src/main.rs", FeatureFileRole::Entry)],
            &changed
        ));
        assert!(feature_touched_since(
            &[f("src/main.rs", FeatureFileRole::Owned)],
            &changed
        ));
        assert!(feature_touched_since(
            &[f("src/main.rs", FeatureFileRole::Context)],
            &changed
        ));
        assert!(!feature_touched_since(
            &[f("tests/it.rs", FeatureFileRole::Test)],
            &changed
        ));
        assert!(!feature_touched_since(
            &[f("src/other.rs", FeatureFileRole::Entry)],
            &changed
        ));
    }

    #[test]
    fn detects_fix_commits() {
        assert!(is_fix_commit("fix: avoid UAF in foo"));
        assert!(is_fix_commit("Fix(parser): off-by-one"));
        assert!(is_fix_commit("Fixes #1234"));
        assert!(is_fix_commit("hotfix: prod outage"));
        assert!(!is_fix_commit("feat: add foo"));
        assert!(!is_fix_commit("refactor parser internals"));
    }

    #[test]
    fn soft_skip_filters_chores_but_rescues_meaningful() {
        assert!(soft_skip("chore: bump deps"));
        assert!(soft_skip("build: switch to bun"));
        assert!(!soft_skip("chore: migrate from yarn to pnpm"));
        assert!(!soft_skip("chore(refactor): rename internal helpers"));
        assert!(!soft_skip("feat: add foo"));
    }

    #[test]
    fn rename_normalization() {
        assert_eq!(
            normalize_rename_path("src/{foo.rs => bar.rs}"),
            "src/bar.rs"
        );
        assert_eq!(
            normalize_rename_path("src/{old => new}/inner.rs"),
            "src/new/inner.rs"
        );
        assert_eq!(
            normalize_rename_path("{old/dir => new/dir}/file.rs"),
            "new/dir/file.rs"
        );
        assert_eq!(normalize_rename_path("plain/path.rs"), "plain/path.rs");
        assert_eq!(
            normalize_rename_path("src/old.rs => src/new.rs"),
            "src/new.rs"
        );
        assert_eq!(
            normalize_rename_path("src/FOO.rs => src/foo.rs"),
            "src/foo.rs"
        );
        // Degenerate arrows are not renames; leave them untouched.
        assert_eq!(normalize_rename_path(" => src/new.rs"), " => src/new.rs");
        assert_eq!(normalize_rename_path("src/old.rs => "), "src/old.rs => ");
    }

    #[test]
    fn parse_log_handles_basic_format() {
        let raw = "commit\tabc\t1700000000\tfix: x\n10\t2\tsrc/a.rs\n5\t1\tsrc/b.rs\n\
                   commit\tdef\t1700001000\tfeat: y\n3\t0\tsrc/c.rs\n-\t-\tbinary.bin\n";
        let commits = parse_log(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "fix: x");
        assert_eq!(commits[0].changes.len(), 2);
        assert_eq!(commits[1].changes.len(), 1, "binary file skipped");
        assert_eq!(commits[1].changes[0].path, "src/c.rs");
    }

    fn make_change(path: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            added: 5,
            deleted: 1,
        }
    }

    fn test_glob() -> GlobSet {
        build_exclude_set(&["**/*Test.php".to_string(), "**/*.phpt".to_string()])
            .expect("build glob set")
    }

    #[test]
    fn accumulate_keeps_source_test_pair_drops_test_test_pair() {
        let now = 1_700_000_100;
        let commit = Commit {
            timestamp: 1_700_000_000,
            subject: "fix: thing".into(),
            changes: vec![],
            ..Commit::default()
        };
        let changes = [
            make_change("Repository.php"),
            make_change("RepositoryTest.php"),
            make_change("AnotherTest.php"),
        ];
        let kept: Vec<&FileChange> = changes.iter().collect();
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        accumulate(&mut files, &mut pairs, &commit, &kept, now, &test_glob());

        assert!(
            pairs.contains_key(&("Repository.php".into(), "RepositoryTest.php".into())),
            "source-test pair must be kept; got pairs: {:?}",
            pairs.keys().collect::<Vec<_>>()
        );
        assert!(
            pairs.contains_key(&("AnotherTest.php".into(), "Repository.php".into())),
            "source-test pair must be kept regardless of stem"
        );
        assert!(
            !pairs.contains_key(&("AnotherTest.php".into(), "RepositoryTest.php".into())),
            "test-test pair must be skipped"
        );
        assert!(files.contains_key("Repository.php"));
        assert!(files.contains_key("RepositoryTest.php"));
        assert!(files.contains_key("AnotherTest.php"));
    }

    #[test]
    fn accumulate_keeps_source_source_pairs_when_tests_present() {
        let now = 1_700_000_100;
        let commit = Commit {
            timestamp: 1_700_000_000,
            subject: "feat: x".into(),
            changes: vec![],
            ..Commit::default()
        };
        let changes = [
            make_change("Repository.php"),
            make_change("Service.php"),
            make_change("RepositoryTest.php"),
        ];
        let kept: Vec<&FileChange> = changes.iter().collect();
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        accumulate(&mut files, &mut pairs, &commit, &kept, now, &test_glob());

        assert!(
            pairs.contains_key(&("Repository.php".into(), "Service.php".into())),
            "source-source pair must always be kept"
        );
    }

    const DAY: i64 = SECONDS_PER_DAY as i64;

    /// Start of the fixed-epoch 90-day window containing `ts`.
    fn window_start(ts: i64) -> i64 {
        (ts / RECURRENCE_WINDOW_SECS) * RECURRENCE_WINDOW_SECS
    }

    /// Run `accumulate` over one two-file commit per timestamp and return
    /// the (a.rs, b.rs) pair stats.
    fn pair_over_commits(timestamps: &[i64]) -> PairStats {
        let now = *timestamps.iter().max().expect("at least one commit") + DAY;
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        let changes = [make_change("a.rs"), make_change("b.rs")];
        let kept: Vec<&FileChange> = changes.iter().collect();
        for &ts in timestamps {
            let commit = Commit {
                timestamp: ts,
                subject: "feat: x".into(),
                changes: vec![],
                ..Commit::default()
            };
            accumulate(&mut files, &mut pairs, &commit, &kept, now, &test_glob());
        }
        pairs
            .remove(&("a.rs".into(), "b.rs".into()))
            .expect("pair accumulated")
    }

    #[test]
    fn recurrence_is_one_for_a_single_mass_commit() {
        let ts = 1_750_000_000;
        let commit = Commit {
            timestamp: ts,
            subject: "feat: sweep".into(),
            changes: vec![],
            ..Commit::default()
        };
        let changes = [
            make_change("a.rs"),
            make_change("b.rs"),
            make_change("c.rs"),
        ];
        let kept: Vec<&FileChange> = changes.iter().collect();
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        accumulate(
            &mut files,
            &mut pairs,
            &commit,
            &kept,
            ts + DAY,
            &test_glob(),
        );
        assert_eq!(pairs.len(), 3);
        for (key, stats) in &pairs {
            assert_eq!(stats.window_mask, window_bit(ts), "{key:?}");
            assert_eq!(stats.window_mask.count_ones(), 1);
            assert_eq!(stats.first_observed_at, Some(ts));
            assert_eq!(stats.last_observed_at, Some(ts));
        }
    }

    #[test]
    fn recurrence_is_one_for_a_burst_inside_one_window() {
        // Three commits inside one fixed window: enough for the min-count
        // filter, but one burst.
        let base = window_start(1_750_000_000);
        let stats = pair_over_commits(&[base + DAY, base + 30 * DAY, base + 60 * DAY]);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.window_mask.count_ones(), 1);
        assert_eq!(stats.first_observed_at, Some(base + DAY));
        assert_eq!(stats.last_observed_at, Some(base + 60 * DAY));
    }

    #[test]
    fn recurrence_counts_distinct_ninety_day_windows() {
        let t = 1_750_000_000;
        // 100-day spacing always lands in distinct windows (100 > 90).
        let stats = pair_over_commits(&[t - 200 * DAY, t - 100 * DAY, t]);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.window_mask.count_ones(), 3);
        // Two commits 20 days apart may share a window; the mask has 3 or 4
        // bits depending on where the fixed boundary falls, never fewer.
        let stats = pair_over_commits(&[t - 200 * DAY, t - 120 * DAY, t - 100 * DAY, t]);
        assert_eq!(stats.count, 4);
        assert!((3..=4).contains(&stats.window_mask.count_ones()));
        // Reordering commits never changes the mask: it is a set, not a walk.
        let forward = pair_over_commits(&[t - 200 * DAY, t - 100 * DAY, t]);
        let reverse = pair_over_commits(&[t, t - 100 * DAY, t - 200 * DAY]);
        assert_eq!(forward.window_mask, reverse.window_mask);
        assert_eq!(forward.first_observed_at, reverse.first_observed_at);
    }

    #[test]
    fn recurrence_window_is_fixed_epoch_and_wraps_only_beyond_history() {
        let base = window_start(1_750_000_000);
        assert_eq!(recurrence_window(base), recurrence_window(base + 89 * DAY));
        assert_ne!(recurrence_window(base), recurrence_window(base + 90 * DAY));
        assert_eq!(
            (recurrence_window(base) + 1) % RECURRENCE_RING as u32,
            recurrence_window(base + 90 * DAY)
        );
        // Negative timestamps clamp to window 0 rather than going negative.
        assert_eq!(recurrence_window(-5), 0);
        // Within the full scan's history bound no two windows share a bit:
        // every 90-day step across 730 days maps to a distinct ring index.
        let steps = (HISTORY_WINDOW_DAYS / RECURRENCE_WINDOW_DAYS) as i64 + 1;
        let mut seen = std::collections::HashSet::new();
        for i in 0..=steps {
            assert!(
                seen.insert(recurrence_window(base - i * RECURRENCE_WINDOW_SECS)),
                "window {i} steps back aliased an earlier one"
            );
        }
        // The ring wraps only 64 windows (5760 days) apart.
        assert_eq!(
            recurrence_window(base),
            recurrence_window(base - RECURRENCE_RING * RECURRENCE_WINDOW_SECS)
        );
    }

    #[test]
    fn delta_mask_ors_onto_full_mask_exactly() {
        // The incremental contract: full(A ∪ B) == full(A) | delta(B) for the
        // mask, MIN for first_observed_at, MAX for last_observed_at.
        let t = 1_750_000_000;
        let old = [t - 300 * DAY, t - 299 * DAY, t - 298 * DAY];
        let new: Vec<i64> = (1..=10).map(|i| t - 300 * DAY + i * 30 * DAY).collect();
        let all: Vec<i64> = old.iter().chain(new.iter()).copied().collect();
        let full = pair_over_commits(&all);
        let base = pair_over_commits(&old);
        let delta = pair_over_commits(&new);
        assert_eq!(full.window_mask, base.window_mask | delta.window_mask);
        assert_eq!(full.count, base.count + delta.count);
        assert_eq!(
            full.first_observed_at,
            base.first_observed_at.min(delta.first_observed_at)
        );
        assert_eq!(
            full.last_observed_at,
            base.last_observed_at.max(delta.last_observed_at)
        );
        // 13 commits over 300 days span four 90-day windows (or five if a
        // boundary splits the first burst), never one.
        assert!(full.window_mask.count_ones() >= 4);
    }

    #[test]
    fn decay_git_history_between_anchors_scales_existing_weights() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_git_file("src/a.rs", 10.0, 0, 1, None).unwrap();
        db.upsert_git_co_change("src/a.rs", "src/b.rs", 6.0, 3, None)
            .unwrap();

        let now = (DECAY_TAU_DAYS * SECONDS_PER_DAY) as i64;
        decay_git_history_between(&db, 0, now).unwrap();

        let file = db.git_file("src/a.rs").unwrap().expect("git file");
        assert!(file.churn_score < 10.0);
        assert!(file.churn_score > 3.0);

        let pair = db.co_changes_for("src/a.rs", 1).unwrap();
        assert_eq!(pair.len(), 1);
        assert!(pair[0].weight < 6.0);
        assert!(pair[0].weight > 2.0);
    }

    #[test]
    fn decay_between_equal_anchors_is_a_no_op() {
        // Re-running a pass on an unchanged HEAD must not age stored weights.
        let db = Database::open_in_memory().unwrap();
        db.upsert_git_file("src/a.rs", 10.0, 0, 1, None).unwrap();
        decay_git_history_between(&db, 1_700_000_000, 1_700_000_000).unwrap();
        let file = db.git_file("src/a.rs").unwrap().expect("git file");
        assert_eq!(file.churn_score, 10.0);
    }

    #[test]
    fn window_cutoff_measures_back_from_the_anchor_and_clamps_at_the_epoch() {
        let anchor = 1_800_000_000;
        let span = (HISTORY_WINDOW_DAYS * SECONDS_PER_DAY) as i64;
        assert_eq!(history_window_cutoff(anchor), anchor - span);
        assert_eq!(
            HistoryAnchor {
                epoch: anchor,
                source: HistoryAnchorSource::HeadCommit,
            }
            .cutoff(),
            anchor - span
        );
        // A history that starts before 1972 must not hand git a negative second.
        assert_eq!(history_window_cutoff(DAY), 0);
    }

    #[test]
    fn window_stamp_distinguishes_the_two_regimes() {
        let head = HistoryAnchor {
            epoch: 1,
            source: HistoryAnchorSource::HeadCommit,
        };
        let wall = HistoryAnchor {
            epoch: 1,
            source: HistoryAnchorSource::WallClock,
        };
        assert_eq!(head.window_stamp(), "730d@HEAD");
        assert_eq!(wall.window_stamp(), "730d@now");
        assert_ne!(head.window_stamp(), wall.window_stamp());
    }

    #[test]
    fn incremental_provenance_survives_only_with_the_same_established_regime() {
        let db = Database::open_in_memory().unwrap();
        let head = HistoryAnchor {
            epoch: 1_500_000_000,
            source: HistoryAnchorSource::HeadCommit,
        };
        let wall = HistoryAnchor {
            epoch: 1_600_000_000,
            source: HistoryAnchorSource::WallClock,
        };
        head.persist(&db, "a", false).unwrap();
        assert_eq!(HistoryAnchor::indexed(&db).unwrap(), None);
        wall.persist(&db, "a", true).unwrap();
        assert_eq!(HistoryAnchor::indexed(&db).unwrap(), Some(wall));
        head.persist(&db, "b", false).unwrap();
        assert_eq!(HistoryAnchor::indexed(&db).unwrap(), None);
        head.persist(&db, "b", true).unwrap();
        let next = HistoryAnchor {
            epoch: head.epoch + 86_400,
            ..head
        };
        next.persist(&db, "c", false).unwrap();
        assert_eq!(HistoryAnchor::indexed(&db).unwrap(), Some(next));
    }

    #[test]
    fn history_predates_wall_clock_window_names_the_regime_that_matters() {
        let now = 1_800_000_000;
        let span = (HISTORY_WINDOW_DAYS * SECONDS_PER_DAY) as i64;
        assert!(history_predates_wall_clock_window(now - span - DAY, now));
        assert!(!history_predates_wall_clock_window(now - span + DAY, now));
        assert!(!history_predates_wall_clock_window(now, now));
    }

    #[test]
    fn anchor_falls_back_to_the_wall_clock_without_a_head() {
        // An initialized repository with an unborn HEAD: no panic, and the
        // fallback names itself rather than passing for a HEAD-anchored pass.
        let empty = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(empty.path())
            .status()
            .expect("git init runs");
        assert!(status.success());
        assert!(
            resolve_head_sha(empty.path()).is_err(),
            "fixture must have an unborn HEAD"
        );

        let anchor = anchor_at_commit(empty.path(), "HEAD");
        assert_eq!(anchor.source, HistoryAnchorSource::WallClock);
        assert!(
            (anchor.epoch - unix_now()).abs() < 60,
            "wall-clock fallback should be ~now, got {}",
            anchor.epoch
        );
        assert_eq!(anchor.window_stamp(), "730d@now");
    }

    #[test]
    fn commit_epoch_rejects_dash_prefixed_revisions() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(commit_epoch(dir.path(), "-O/etc/passwd"), None);
    }
}
