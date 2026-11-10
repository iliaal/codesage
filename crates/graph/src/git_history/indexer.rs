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
//! - rename continuity: commits are walked children-first and every change is
//!   keyed to the path its file carries at HEAD, so a `git mv` keeps churn,
//!   fixes, author events, and co-change pairs on the live path; an
//!   incremental pass re-keys stored rows for paths renamed inside its range.
//!   Copies are not followed (`-M` overrides `diff.renames=copies`).
//!
//! An edit to the old path on a parallel branch that `--topo-order` lists
//! before the rename is recovered afterwards: a change left on a path absent
//! from HEAD follows the pass's rename map to a path HEAD holds. An
//! incremental pass re-keys stored rows only while that stays exact; a range
//! that renames a file onto a path with recorded history, or across the
//! test-like boundary, is rescanned in full.
//!
//! Rename limits: a dead file whose path a later file reuses shares that
//! file's history, as it always has without renames, and a file created at a
//! renamed-away path and deleted again before HEAD is folded into the rename's
//! successor. An incremental pass sees only its own range's renames, so a
//! branch edit to the old path that arrives in a later pass than the rename
//! stays on the old path until `git-index --full`.
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

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use codesage_parser::discover::{
    DEFAULT_EXCLUDE_PATTERNS, TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set,
};
use codesage_protocol::GitIndexStats;
use codesage_storage::Database;
use codesage_storage::db::{CoChangeWrite, GitHistoryBuild, GitHistoryProgress};
use globset::GlobSet;

use super::bus_factor::normalized_author;
use super::source::HistorySource;
use crate::git::{HistoryDeadline, git_history_command, git_history_walk_command, history_output};

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
fn anchor_at_commit(root: &Path, sha: &str) -> Result<HistoryAnchor> {
    Ok(match commit_epoch(root, sha)? {
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
    })
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
struct PassAnchors<'a> {
    current: HistoryAnchor,
    previous: i64,
    exclusion_fingerprint: &'a str,
    source: &'a HistorySource,
    limits: WalkLimits,
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
///
/// `Ok(None)` means git could not name the commit (an unborn HEAD, a missing
/// object, no usable git). Output git produced but that is not an epoch is an
/// error: measuring from a guessed clock would index the wrong window.
fn commit_epoch(root: &Path, sha: &str) -> Result<Option<i64>> {
    // `--` would not neutralize an attached-value option such as `-O/path`.
    if sha.starts_with('-') {
        return Ok(None);
    }
    let key = (root.to_path_buf(), sha.to_string());
    let memo = commit_epoch_memo();
    if let Some(hit) = lock_memo(memo).get(&key) {
        return Ok(Some(*hit));
    }
    let Some(epoch) = read_commit_epoch(root, sha)? else {
        return Ok(None);
    };
    let mut guard = lock_memo(memo);
    if guard.len() >= COMMIT_EPOCH_MEMO_CAP {
        guard.clear();
    }
    guard.insert(key, epoch);
    Ok(Some(epoch))
}

fn read_commit_epoch(root: &Path, sha: &str) -> Result<Option<i64>> {
    // `log.showSignature` would print verification text ahead of the date.
    let Ok(out) = git_history_command(root)
        .args([
            "log",
            "--no-show-signature",
            "-1",
            "--format=%ct",
            sha,
            "--",
        ])
        .output()
    else {
        return Ok(None);
    };
    if !out.status.success() {
        return Ok(None);
    }
    parse_commit_epoch(&out.stdout)
        .map(Some)
        .with_context(|| format!("reading the committer date of {sha} in {}", root.display()))
}

fn parse_commit_epoch(stdout: &[u8]) -> Result<i64> {
    let text = std::str::from_utf8(stdout).context("git log output not UTF-8")?;
    text.trim().parse().map_err(|_| {
        anyhow!(
            "git log printed {:?} where a unix epoch was expected",
            text.trim()
        )
    })
}

/// Indexing mode. `Auto`, the recommended default, reuses prior state if valid
/// and falls back to a full rescan otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// Re-scan the whole history. Drops existing git_files/git_co_changes first.
    Full,
    /// Only scan commits after the last recorded SHA. Falls back to Full if state
    /// is missing, corrupted, or the prior SHA isn't an ancestor of HEAD, or if
    /// the recorded exclusion fingerprint is missing or differs from the
    /// effective one.
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
    index_history(db, root, extra_excludes, mode, WalkLimits::from_env()?)
}

/// Bounds on the git children of one `git-index` run.
#[derive(Debug, Clone, Copy)]
struct WalkLimits {
    /// Deadline for an incremental walk, and for all the slices one run of a
    /// full rebuild reads.
    budget: Duration,
    /// Commits in the first slice of a new full rebuild.
    first_slice: usize,
    /// Stop a full rebuild after this many slices in one run.
    max_slices: Option<usize>,
    /// Walks reported as past their deadline without running git, so tests
    /// can exhaust a budget deterministically.
    forced_deadline: Option<fn(HistoryWalk) -> bool>,
    /// Whether an incremental range that exceeds the budget falls back to a
    /// staged rebuild in the same run. Off for the pass that carries a
    /// just-completed rebuild to HEAD, so one run spends at most three
    /// budgets: an incremental walk, a staged rebuild, and that follow-up.
    may_stage: bool,
    /// Whether a saved rebuild may take precedence over the same-HEAD or
    /// incremental pass. Off for the pass a run falls back to when the
    /// rebuild it preferred could not fingerprint its environment.
    may_preempt: bool,
    /// Upper bound on the time a re-check may run past the deadline.
    max_recheck_allowance: Duration,
    /// Time the numstat environment fingerprint takes on top of its git
    /// children, so tests can make it slow.
    #[cfg(test)]
    environment_delay: Duration,
    /// Called with the repository root after each slice a rebuild saves, so
    /// tests can change the numstat environment between slices.
    #[cfg(test)]
    after_slice: Option<fn(&Path)>,
    /// Called with the repository root after a slice is read and before its
    /// re-check, so tests can change the environment during a slice or make
    /// the slice slow to finish.
    #[cfg(test)]
    before_recheck: Option<fn(&Path)>,
    /// Called with the repository root after the commits are listed and
    /// before the plan's re-check.
    #[cfg(test)]
    before_plan_recheck: Option<fn(&Path)>,
}

/// One budgeted git child of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryWalk {
    /// Fingerprinting what numstat output depends on besides the commit.
    Environment,
    /// Fingerprinting it again after a plan or slice is read, before it is saved.
    Recheck,
    /// Listing the commits a full rebuild reads.
    Plan,
    /// Reading one slice of that many commits.
    Slice(usize),
    /// Reading an incremental range.
    Incremental,
}

impl WalkLimits {
    fn from_env() -> Result<Self> {
        Ok(Self {
            budget: history_walk_budget()?,
            first_slice: FIRST_SLICE_COMMITS,
            max_slices: None,
            forced_deadline: None,
            may_stage: true,
            may_preempt: true,
            max_recheck_allowance: MAX_RECHECK_ALLOWANCE,
            #[cfg(test)]
            environment_delay: Duration::ZERO,
            #[cfg(test)]
            after_slice: None,
            #[cfg(test)]
            before_recheck: None,
            #[cfg(test)]
            before_plan_recheck: None,
        })
    }

    fn run<T>(&self, walk: HistoryWalk, child: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.forced_deadline.is_some_and(|forced| forced(walk)) {
            return Err(HistoryDeadline {
                budget: self.budget,
            }
            .into());
        }
        child()
    }
}

fn is_deadline(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HistoryDeadline>().is_some()
}

/// Why a pass rebuilds history in full, for the error an unfinished rebuild returns.
#[derive(Debug, Clone, Copy)]
enum FullReason<'a> {
    Rebuild,
    /// `git-index --full`. The rebuild it starts or resumes is recorded as
    /// requested against the stored head, so later passes continue it.
    Requested,
    /// The stored head is not an ancestor of HEAD, so its rows cannot be
    /// composed with the commits HEAD added.
    Diverged {
        stored: &'a str,
    },
    /// Reading the commits after the stored head took longer than the budget.
    RangeTooLong {
        stored: &'a str,
    },
}

/// Recorded cause of a staged rebuild: no history was indexed yet.
pub const CAUSE_FIRST_BUILD: &str = "first_build";
/// The stored history head is not an ancestor of HEAD.
pub const CAUSE_DIVERGED: &str = "diverged";
/// Reading the commits after the stored head took longer than the budget.
pub const CAUSE_RANGE_TOO_LONG: &str = "range_too_long";
/// `git-index --full`, a changed history policy, or a rename an incremental
/// pass cannot re-key.
pub const CAUSE_FULL: &str = "full";

/// The last run that worked on a staged rebuild saved commits, or shrank the
/// slice that ran out of time.
pub const OUTCOME_PROGRESSING: &str = "progressing";
/// The last run that worked on a staged rebuild saved nothing.
pub const OUTCOME_NO_PROGRESS: &str = "no_progress";
/// A later pass brought history up to date without the staged rebuild, which
/// is kept so a rebuild for its target reuses the saved commits.
pub const OUTCOME_PARKED: &str = "parked";
/// The last run that worked on a staged rebuild failed with an error other
/// than running out of time; `stalled` holds the error.
pub const OUTCOME_FAILED: &str = "failed";
/// A run discarded a plan or slice because the numstat environment changed
/// while it was read.
pub const DISCARD_ENVIRONMENT_CHANGED: &str = "environment_changed";
/// A run discarded a plan or slice because re-checking the numstat
/// environment took longer than the time it had.
pub const DISCARD_RECHECK_TIMEOUT: &str = "recheck_timeout";

/// The staged full history rebuild, for status readers; `None` when nothing
/// is staged or the index predates the staging tables.
pub fn history_rebuild(db: &Database) -> Result<Option<codesage_protocol::HistoryRebuild>> {
    Ok(db
        .git_history_build_status()?
        .map(|status| codesage_protocol::HistoryRebuild {
            state: status.outcome,
            cause: status.cause,
            target_sha: status.target_sha,
            stored_sha: status.stored_sha,
            commits_saved: status.staged,
            commits_total: status.total,
            next_slice_commits: status.slice_len,
            last_run_saved: status.run_saved,
            updated_at_unix: status.updated_at,
            stalled: status.stalled,
            note: status.note,
            last_discard_unix: status.discarded_at,
            discarding_since_unix: status.discarding_since,
            discard_cause: status.discard_cause,
            restarts: status.restarts,
        }))
}

/// A saved rebuild no run has touched for this long is deleted.
const STALE_BUILD_SECS: i64 = 30 * 24 * 60 * 60;

/// Delete a saved rebuild that no run will finish: one untouched for
/// [`STALE_BUILD_SECS`], or one whose target commit git no longer has. A run
/// that reached the rebuild but discarded what it read counts as touching
/// it, so a rebuild whose environment keeps moving is kept and reported by
/// doctor rather than deleted. A
/// target that only stopped being an ancestor of HEAD is kept, since
/// switching back to its branch resumes it. A lookup that cannot answer keeps
/// the rebuild.
fn prune_stale_build(db: &Database, root: &Path) -> Result<()> {
    let Some(status) = db.git_history_build_status()? else {
        return Ok(());
    };
    let touched = status
        .updated_at
        .max(status.discarded_at.unwrap_or(i64::MIN));
    let expired = unix_now() - touched > STALE_BUILD_SECS;
    let missing = !expired && {
        let mut exists = git_history_command(root);
        exists.args([
            "cat-file",
            "-e",
            "--end-of-options",
            &format!("{}^{{commit}}", status.target_sha),
        ]);
        history_output(exists, STATUS_LOOKUP_BUDGET)
            .is_ok_and(|out| out.status.code() == Some(1) || out.status.code() == Some(128))
    };
    if expired || missing {
        tracing::info!(
            root = %root.display(),
            target = %status.target_sha,
            expired,
            "deleting a saved git history rebuild no run will finish"
        );
        db.execute_batch(|db| db.clear_git_history_build())?;
    }
    Ok(())
}

/// Ancestry lookups for status readers stop after this long; an answer they
/// cannot get leaves the recorded state alone.
const STATUS_LOOKUP_BUDGET: Duration = Duration::from_secs(2);

/// [`history_rebuild`] as seen from the checkout at `root`. A rebuild whose
/// target is neither HEAD nor an ancestor of HEAD, including a target git no
/// longer has, is reported as `parked` whatever its last run did: no pass from
/// this HEAD resumes it, so its stall is not something to act on here, and
/// `git-index --full` would discard its commits.
pub fn history_rebuild_at(
    root: &Path,
    db: &Database,
) -> Result<Option<codesage_protocol::HistoryRebuild>> {
    let Some(mut rebuild) = history_rebuild(db)? else {
        return Ok(None);
    };
    if rebuild.state == OUTCOME_PARKED {
        return Ok(Some(rebuild));
    }
    let reachable = (|| -> Result<Option<bool>> {
        let mut head = git_history_command(root);
        head.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        let head = history_output(head, STATUS_LOOKUP_BUDGET)?;
        if !head.status.success() {
            return Ok(None);
        }
        let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        if head == rebuild.target_sha {
            return Ok(Some(true));
        }
        let mut ancestry = git_history_command(root);
        ancestry.args(["merge-base", "--is-ancestor", &rebuild.target_sha, &head]);
        let ancestry = history_output(ancestry, STATUS_LOOKUP_BUDGET)?;
        Ok(match ancestry.status.code() {
            Some(0) => Some(true),
            // 1: not an ancestor; 128: git cannot read the target.
            Some(1 | 128) => Some(false),
            _ => None,
        })
    })();
    match reachable {
        Ok(Some(false)) => rebuild.state = OUTCOME_PARKED.to_owned(),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            root = %root.display(),
            error = %format!("{error:#}"),
            "checking whether the saved git history rebuild is reachable from HEAD"
        ),
    }
    Ok(Some(rebuild))
}

impl<'a> FullReason<'a> {
    /// The recorded cause, and the stored head the rebuild started against.
    fn cause(self, stored_head: Option<&'a str>) -> (&'static str, Option<&'a str>) {
        match (self, stored_head) {
            (FullReason::Diverged { stored }, _) => (CAUSE_DIVERGED, Some(stored)),
            (FullReason::RangeTooLong { stored }, _) => (CAUSE_RANGE_TOO_LONG, Some(stored)),
            (FullReason::Rebuild | FullReason::Requested, Some(stored)) => {
                (CAUSE_FULL, Some(stored))
            }
            (FullReason::Rebuild | FullReason::Requested, None) => (CAUSE_FIRST_BUILD, None),
        }
    }
}

fn index_history(
    db: &Database,
    root: &Path,
    extra_excludes: &[String],
    mode: IndexMode,
    limits: WalkLimits,
) -> Result<GitIndexStats> {
    let (exclude_set, test_like_set) = compile_excludes(extra_excludes)?;
    let head_sha = resolve_head_sha(root)?;
    let source = HistorySource::read(root)?;
    let exclusion_fingerprint =
        source.fingerprint(&effective_exclusion_fingerprint(extra_excludes));
    // `None` is an index written before 0025 (or by a pre-0025 binary, whose
    // state upsert clears the column): its rows carry no exclusion provenance,
    // so they cannot be trusted to match the current policy either.
    let stored_exclusion = db.git_index_exclusion_fingerprint()?;
    let policy_changed = stored_exclusion.as_deref() != Some(exclusion_fingerprint.as_str());
    let anchor = anchor_at_commit(root, &head_sha)?;
    log_anchor(root, anchor);
    if limits.may_stage {
        prune_stale_build(db, root)?;
    }

    let mut diverged_from = None;
    let mut preempted = false;
    let effective_mode = match mode {
        IndexMode::Full => IndexMode::Full,
        IndexMode::Incremental | IndexMode::Auto => match db.get_git_index_state()? {
            // Rows built under another history policy cannot be composed.
            Some((last_sha, _)) if policy_changed => {
                if last_sha != head_sha && !is_ancestor(root, &last_sha, &head_sha)? {
                    diverged_from = Some(last_sha);
                }
                IndexMode::Full
            }
            Some((last_sha, _))
                if last_sha == head_sha
                    && limits.may_preempt
                    && continues_saved_rebuild(
                        db,
                        root,
                        &head_sha,
                        &last_sha,
                        &exclusion_fingerprint,
                    )? =>
            {
                preempted = true;
                IndexMode::Full
            }
            Some((last_sha, last_indexed_at)) if last_sha == head_sha => {
                // HEAD is unchanged, so the anchor is too: the decay below is a
                // no-op unless the last pass measured from a different clock.
                let previous = previous_anchor(root, &last_sha, last_indexed_at, anchor)?;
                db.execute_batch(|db| {
                    decay_git_history_between(db, previous, anchor.epoch)?;
                    db.prune_git_author_events(anchor.cutoff())?;
                    anchor.persist(db, &head_sha, false)?;
                    db.set_git_index_exclusion_fingerprint(&exclusion_fingerprint)?;
                    db.park_git_history_build(
                        OUTCOME_PARKED,
                        OUTCOME_PROGRESSING,
                        &[CAUSE_RANGE_TOO_LONG, CAUSE_DIVERGED],
                    )
                })?;
                return Ok(GitIndexStats {
                    commits_scanned: 0,
                    files_tracked: 0,
                    co_change_pairs: 0,
                    notes: source.notes(),
                });
            }
            Some((last_sha, _)) => {
                if !is_ancestor(root, &last_sha, &head_sha)? {
                    diverged_from = Some(last_sha);
                    IndexMode::Full
                } else if limits.may_preempt
                    && continues_saved_rebuild(
                        db,
                        root,
                        &head_sha,
                        &last_sha,
                        &exclusion_fingerprint,
                    )?
                {
                    preempted = true;
                    IndexMode::Full
                } else {
                    IndexMode::Incremental
                }
            }
            None => IndexMode::Full,
        },
    };
    // A pass that may not stage (the follow-up to a completed rebuild, or the
    // fallback from one that could not fingerprint) must not start another
    // rebuild, even if HEAD moved off its line meanwhile: that would spend a
    // further budget in the same run.
    if !limits.may_stage && effective_mode == IndexMode::Full {
        let indexed = db.get_git_index_state()?.map(|(sha, _)| sha);
        return Err(anyhow!(
            "git history is indexed at {}, and HEAD {} needs a full rebuild from there; the \
             next `codesage git-index` run starts it",
            indexed.as_deref().map_or("nothing", short_sha),
            short_sha(&head_sha)
        ));
    }

    let ran = match effective_mode {
        IndexMode::Full => run_full(
            db,
            root,
            &exclude_set,
            &test_like_set,
            &head_sha,
            PassAnchors {
                current: anchor,
                previous: anchor.epoch,
                exclusion_fingerprint: &exclusion_fingerprint,
                source: &source,
                limits,
            },
            match &diverged_from {
                Some(stored) => FullReason::Diverged { stored },
                None if mode == IndexMode::Full => FullReason::Requested,
                None => FullReason::Rebuild,
            },
        ),
        IndexMode::Incremental => {
            let (last_sha, last_at) = db
                .get_git_index_state()?
                .expect("incremental path checked state present above");
            let anchors = PassAnchors {
                current: anchor,
                previous: previous_anchor(root, &last_sha, last_at, anchor)?,
                exclusion_fingerprint: &exclusion_fingerprint,
                source: &source,
                limits,
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
    };
    let mut stats = match ran {
        // The saved rebuild was preferred over a pass that does not need it,
        // but its saved output cannot be trusted this run. Record that on the
        // rebuild, which then no longer takes precedence, and run that pass.
        Err(error) if preempted && error.downcast_ref::<FingerprintFailed>().is_some() => {
            let outcome = if is_deadline(&error) {
                OUTCOME_NO_PROGRESS
            } else {
                OUTCOME_FAILED
            };
            record_unfinished(db, None, outcome, Some(&one_line(&format!("{error:#}"))), 0)?;
            tracing::warn!(
                root = %root.display(),
                error = %format!("{error:#}"),
                "the saved git history rebuild could not fingerprint its environment; \
                 indexing without it"
            );
            return index_history(
                db,
                root,
                extra_excludes,
                mode,
                WalkLimits {
                    may_preempt: false,
                    may_stage: false,
                    ..limits
                },
            );
        }
        ran => ran?,
    };
    // A resumed rebuild completes at the commit it was planned for; HEAD may
    // have moved on since, along a range an incremental pass composes onto it.
    if db
        .get_git_index_state()?
        .is_some_and(|(indexed, _)| indexed != head_sha)
    {
        let follow_up = index_history(
            db,
            root,
            extra_excludes,
            IndexMode::Incremental,
            WalkLimits {
                may_stage: false,
                ..limits
            },
        )?;
        stats = GitIndexStats {
            commits_scanned: stats.commits_scanned + follow_up.commits_scanned,
            ..follow_up
        };
    }
    stats.notes = source.notes();
    Ok(stats)
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
) -> Result<i64> {
    Ok(match current.source {
        HistoryAnchorSource::HeadCommit => commit_epoch(root, last_sha)?.unwrap_or(last_indexed_at),
        HistoryAnchorSource::WallClock => last_indexed_at,
    })
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

/// Bump when repository selection or rules that decide stored history rows
/// change (commit soft-skips, fix-commit detection, pair accounting), so the next pass
/// rebuilds instead of composing rows written under two rule sets.
const HISTORY_POLICY_VERSION: u32 = 3;

/// Canonical, order-independent identity for the history and exclusion policy:
/// repository selection, hard exclusions, the test-like set, and per-commit
/// file cap decide which rows are stored. An empty user list is still a valid
/// fingerprint so legacy NULL state cannot silently reuse another policy's rows.
fn effective_exclusion_fingerprint(extra_excludes: &[String]) -> String {
    let mut encoded =
        format!("policy:{HISTORY_POLICY_VERSION}\0cap:{MAX_FILES_PER_COMMIT_FOR_COCHANGE}\0");
    for pattern in TEST_LIKE_EXCLUDE_PATTERNS {
        encoded.push_str("test:");
        encoded.push_str(&pattern.len().to_string());
        encoded.push(':');
        encoded.push_str(pattern);
        encoded.push('\0');
    }
    for pattern in DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .copied()
        .chain(extra_excludes.iter().map(String::as_str))
        .collect::<BTreeSet<_>>()
    {
        encoded.push_str(&pattern.len().to_string());
        encoded.push(':');
        encoded.push_str(pattern);
        encoded.push('\0');
    }
    encoded
}

fn run_full(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
    anchors: PassAnchors<'_>,
    reason: FullReason<'_>,
) -> Result<GitIndexStats> {
    let (target, mut commits) = match stage_history(db, root, head_sha, &anchors, reason)? {
        Staged::Complete { target, commits } => (target, commits),
        Staged::Incomplete(progress) => {
            return Err(rebuild_incomplete(
                &progress,
                reason,
                head_sha,
                anchors.limits.budget,
            ));
        }
    };
    let anchor = if target == head_sha {
        anchors.current
    } else {
        anchor_at_commit(root, &target)?
    };
    let successor = follow_renames(&mut commits, exclude_set);
    remap_dead_paths(root, &target, &mut commits, &successor)?;

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
        anchor.persist(db, &target, true)?;
        db.set_git_index_exclusion_fingerprint(anchors.exclusion_fingerprint)?;
        db.clear_git_history_build()
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
        notes: Vec::new(),
    })
}

/// Commits in the first slice of a new rebuild; later slices are sized from
/// the observed rate.
const FIRST_SLICE_COMMITS: usize = 512;
/// Upper bound on one slice, which passes every SHA as an argument.
const MAX_SLICE_COMMITS: usize = 4096;

/// Progress of a rebuild that did not finish in this run.
#[derive(Debug)]
struct BuildProgress {
    target: String,
    staged: usize,
    total: usize,
    /// Commits the next run reads in its first slice.
    slice_len: usize,
    /// Why this run saved nothing, when it saved nothing.
    stalled: Option<String>,
    /// Why the plan or slice this run read last was not saved, when its
    /// environment re-check did not confirm it; the rebuild is still in
    /// progress.
    discarded: Option<String>,
}

enum Staged {
    Complete {
        target: String,
        commits: Vec<Commit>,
    },
    Incomplete(BuildProgress),
}

/// The staged rebuild a pass at `head_sha` may finish: same policy and
/// window, planned at HEAD or at an ancestor an incremental pass can extend.
fn resumable_build(
    db: &Database,
    root: &Path,
    head_sha: &str,
    policy: &str,
) -> Result<Option<GitHistoryBuild>> {
    let Some(build) = db.git_history_build()? else {
        return Ok(None);
    };
    let resumable = resumable_target(
        root,
        head_sha,
        policy,
        &build.policy,
        &build.target_sha,
        build.cutoff,
    )?;
    Ok(resumable.then_some(build))
}

/// Read the full history in slices that each fit in what remains of the
/// run's budget, saving each commit's output by SHA, so a history whose
/// single walk exceeds the budget completes over several runs. The commits
/// are planned once, in the order and window a single walk would list them;
/// a re-plan for another target keeps the output of every commit it still
/// lists. Each slice reads the same per-commit output a single walk does, and
/// the commits are assembled in planned order, so they equal that walk's.
fn stage_history(
    db: &Database,
    root: &Path,
    head_sha: &str,
    anchors: &PassAnchors<'_>,
    reason: FullReason<'_>,
) -> Result<Staged> {
    let stored_head = db.get_git_index_state()?.map(|(sha, _)| sha);
    let (run_cause, run_stored) = reason.cause(stored_head.as_deref());
    // A run without a cause of its own leaves the recorded one in place.
    let relabel = (!matches!(reason, FullReason::Rebuild)).then_some((run_cause, run_stored));
    let run = RunTrace::default();
    let staged = stage_history_slices(db, root, head_sha, anchors, reason, &run);
    if let Err(error) = &staged {
        let outcome = if is_deadline(error) {
            OUTCOME_NO_PROGRESS
        } else {
            OUTCOME_FAILED
        };
        let error = format!("{error:#}");
        let recorded = (|| -> Result<()> {
            let Some(status) = db.git_history_build_status()? else {
                return Ok(());
            };
            // Only the rebuild this run resumed or started takes its outcome,
            // cause, and error. One saved for another target is unaffected,
            // so a failure elsewhere cannot stop a healthy rebuild from being
            // continued; it carries the failure as a note.
            if run.working_on.take().as_deref() == Some(status.target_sha.as_str()) {
                return record_unfinished(
                    db,
                    relabel,
                    outcome,
                    Some(&one_line(&error)),
                    run.saved.get(),
                );
            }
            let what = match run.planning.take() {
                Some(target) => format!("planning a rebuild toward {}", short_sha(&target)),
                None => format!(
                    "a `git-index` run at HEAD {} before reaching this rebuild",
                    short_sha(head_sha)
                ),
            };
            db.set_git_history_build_note(&one_line(&format!("{what} failed: {error}")))
        })();
        if let Err(record) = recorded {
            tracing::warn!(
                error = %format!("{record:#}"),
                "recording the failed git history rebuild run"
            );
        }
    }
    staged
}

/// `text` on one line of at most 500 characters, for a stored reason.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(500)
        .collect()
}

/// What a staging run did before it stopped, for recording a failure.
#[derive(Default)]
struct RunTrace {
    /// Commits saved.
    saved: std::cell::Cell<usize>,
    /// The target being planned, while no rebuild for it is saved yet.
    planning: std::cell::Cell<Option<String>>,
    /// Target of the saved rebuild this run resumed or started.
    working_on: std::cell::Cell<Option<String>>,
}

/// Record how a run left a staged rebuild it did not finish; a no-op when
/// nothing is staged. `relabel` replaces the recorded cause and stored head.
fn record_unfinished(
    db: &Database,
    relabel: Option<(&str, Option<&str>)>,
    outcome: &str,
    stalled: Option<&str>,
    run_saved: usize,
) -> Result<()> {
    let Some(status) = db.git_history_build_status()? else {
        return Ok(());
    };
    let (cause, stored_sha) =
        relabel.unwrap_or((status.cause.as_str(), status.stored_sha.as_deref()));
    db.set_git_history_progress(&GitHistoryProgress {
        slice_len: status.slice_len,
        cause,
        stored_sha,
        outcome,
        stalled,
        run_saved,
    })
}

/// Git prints paths as the bytes the filesystem holds, which need not be UTF-8.
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[cfg(test)]
thread_local! {
    static NUMSTAT_FINGERPRINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Identity of what `git log --numstat` output depends on besides the commit
/// itself, read from the repository, its configuration, and the worktree
/// rather than from each commit. An attribute such as `-diff`, or a
/// `core.bigFileThreshold` the file exceeds, turns its counts into `-`, so
/// output read under one setting is not the output a walk under another
/// reads. Covered: the git version; the effective configuration as
/// `git config --list` prints it for the walk, less the keys
/// [`numstat_config_ignored`] names, so a key nobody listed costs a re-read
/// rather than wrong history; `$GIT_DIR/info/attributes`, the global and
/// system attributes files (the system file only where `git var` names it,
/// from git 2.42); the `.gitattributes` entries of the trees `attr.tree` and
/// `GIT_ATTR_SOURCE` name (and of HEAD in a bare repository without
/// `attr.tree`); the `refs/replace/` refs, `GIT_NO_REPLACE_OBJECTS`, and
/// `info/grafts`; and, with a worktree, the index blob of every tracked
/// `.gitattributes`, the worktree `.gitattributes`, tracked or not, of every
/// directory that holds a tracked path, and the worktree and index
/// `.gitmodules`. A `.gitattributes` in a directory with no tracked path,
/// such as an ignored directory, a nested repository, or a submodule
/// worktree, is not read.
/// Every git child stops at `deadline`; `budget` names it in the error.
fn numstat_environment_fingerprint(
    root: &Path,
    deadline: Instant,
    budget: Duration,
) -> Result<String> {
    #[cfg(test)]
    NUMSTAT_FINGERPRINTS.with(|count| count.set(count.get() + 1));
    let mut encoded = Vec::new();
    for (label, bytes) in &numstat_environment_parts(root, deadline, budget)? {
        for part in [label, bytes] {
            encoded.extend_from_slice(&(part.len() as u64).to_le_bytes());
            encoded.extend_from_slice(part);
        }
    }
    Ok(codesage_parser::discover::content_hash(&encoded))
}

/// The first 1 MiB of a file the numstat environment depends on, or a marker
/// for one that is absent or unreadable.
fn read_environment_file(path: &Path) -> Vec<u8> {
    use std::io::Read;

    let mut bytes = Vec::new();
    match std::fs::File::open(path) {
        Ok(file) => match file.take(1 << 20).read_to_end(&mut bytes) {
            Ok(_) => bytes,
            Err(_) => b"<unreadable>".to_vec(),
        },
        Err(_) => b"<absent>".to_vec(),
    }
}

/// The worktree `.gitattributes` of each directory in `dirs` (relative to
/// `toplevel`, root as the empty path) that has one, labelled by path. The
/// deadline is checked every 256 directories, so a tree with many
/// directories cannot run the fingerprint past it.
fn directory_attributes(
    toplevel: &Path,
    dirs: BTreeSet<&[u8]>,
    deadline: Instant,
    budget: Duration,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut found = Vec::new();
    for (checked, dir) in dirs.into_iter().enumerate() {
        if checked % 256 == 255 && Instant::now() >= deadline {
            return Err(HistoryDeadline { budget }.into());
        }
        let file = toplevel.join(path_from_bytes(dir)).join(".gitattributes");
        if file.exists() {
            let mut label = dir.to_vec();
            label.extend_from_slice(b"/.gitattributes");
            found.push((label, read_environment_file(&file)));
        }
    }
    Ok(found)
}

/// The labelled parts [`numstat_environment_fingerprint`] hashes, in order.
fn numstat_environment_parts(
    root: &Path,
    deadline: Instant,
    budget: Duration,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let git = |dir: &Path, args: &[&str]| -> Result<Option<Vec<u8>>> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HistoryDeadline { budget }.into());
        }
        let mut command = git_history_walk_command(dir);
        command.args(args);
        let output = history_output(command, left)
            .with_context(|| format!("running git {} in {}", args.join(" "), dir.display()))?;
        Ok(output.status.success().then_some(output.stdout))
    };
    let read = read_environment_file;
    let trimmed = |bytes: Vec<u8>| -> Option<PathBuf> {
        let end = bytes
            .iter()
            .rposition(|&b| b != b'\n' && b != b'\r')
            .map_or(0, |last| last + 1);
        (end > 0).then(|| path_from_bytes(&bytes[..end]))
    };
    // The `.gitattributes` entries of a tree, as path and blob ID: the only
    // part of an attribute source tree that matters, so `attr.tree=HEAD`
    // survives commits that leave the attributes alone.
    let tree_attributes = |tree_ish: &[u8]| -> Result<Vec<u8>> {
        let spec = format!("{}^{{tree}}", String::from_utf8_lossy(tree_ish));
        let tree = git(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )?
        .map(|tree| String::from_utf8_lossy(&tree).trim().to_owned())
        .filter(|tree| !tree.is_empty());
        let Some(tree) = tree else {
            return Ok(b"<unresolved>".to_vec());
        };
        let entries = git(root, &["ls-tree", "-r", "-z", "--full-tree", &tree])?
            .context("listing an attribute source tree")?;
        let mut kept = Vec::new();
        for entry in entries.split(|&b| b == 0) {
            let path = entry
                .iter()
                .position(|&b| b == b'\t')
                .map_or(&b""[..], |tab| &entry[tab + 1..]);
            let name = path
                .iter()
                .rposition(|&b| b == b'/')
                .map_or(path, |slash| &path[slash + 1..]);
            if name == b".gitattributes" {
                kept.extend_from_slice(entry);
                kept.push(0);
            }
        }
        Ok(kept)
    };

    let mut parts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut push = |label: &[u8], bytes: &[u8]| parts.push((label.to_vec(), bytes.to_vec()));
    // Diff and rename heuristics change between git releases.
    let version = git(root, &["--version"])?.context("reading the git version")?;
    push(b"git-version", &version);
    let located = git(
        root,
        &[
            "rev-parse",
            "--is-bare-repository",
            "--git-path",
            "info/attributes",
            "--git-path",
            "info/grafts",
        ],
    )?
    .context("locating the repository's info/attributes and grafts")?;
    let mut lines = located.split(|&b| b == b'\n');
    let bare = lines.next() == Some(&b"true"[..]);
    let mut git_path = || {
        lines
            .next()
            .filter(|line| !line.is_empty())
            .map(path_from_bytes)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            })
    };
    let info = git_path();
    let grafts = git_path();
    push(b"info", &info.as_deref().map(read).unwrap_or_default());
    // Grafts and replace refs rewrite parents, so a commit's diff changes.
    push(b"grafts", &grafts.as_deref().map(read).unwrap_or_default());
    let replaced = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/replace/",
        ],
    )?
    .context("listing replace refs")?;
    push(b"replace-refs", &replaced);
    push(
        b"GIT_NO_REPLACE_OBJECTS",
        &env_bytes("GIT_NO_REPLACE_OBJECTS").unwrap_or_else(|| b"<unset>".to_vec()),
    );

    // A bare repository has no worktree attributes or `.gitmodules` to read.
    if !bare {
        let toplevel = git(root, &["rev-parse", "--show-toplevel"])?
            .and_then(trimmed)
            .context("locating the git worktree")?;
        // The walk command removes `GIT_LITERAL_PATHSPECS` and the other
        // pathspec variables, so this magic is read as written.
        let pathspec = ":(top,glob)**/.gitattributes";
        let ls_files = |args: &[&str]| {
            let mut full = vec!["-c", "core.fsmonitor=false"];
            full.extend_from_slice(args);
            git(&toplevel, &full)
        };
        let staged = ls_files(&["ls-files", "-z", "--stage", "--", pathspec])?
            .context("listing tracked .gitattributes files")?;
        push(b"index", &staged);
        // Git reads `<dir>/.gitattributes` from the worktree, tracked or not,
        // for every directory holding a path it diffs. Each directory with a
        // tracked path is checked directly, root included, so an untracked
        // file there is seen without a worktree scan; only a directory with
        // no tracked path at all goes unchecked. A directory without the
        // file adds nothing, so new directories leave the fingerprint alone.
        let tracked = ls_files(&["ls-files", "-z", "--"])?.context("listing tracked paths")?;
        let mut dirs = BTreeSet::from([&b""[..]]);
        for path in tracked.split(|&b| b == 0) {
            let mut rest = path;
            while let Some(slash) = rest.iter().rposition(|&b| b == b'/') {
                rest = &rest[..slash];
                if !dirs.insert(rest) {
                    break;
                }
            }
        }
        for (label, bytes) in directory_attributes(&toplevel, dirs, deadline, budget)? {
            push(&label, &bytes);
        }
        // A submodule's `ignore` setting hides its gitlink changes from numstat.
        let modules = ls_files(&[
            "ls-files",
            "-z",
            "--stage",
            "--",
            ":(top,literal).gitmodules",
        ])?
        .context("listing .gitmodules")?;
        push(b"gitmodules-index", &modules);
        push(b"gitmodules", &read(&toplevel.join(".gitmodules")));
    }

    let global = match git(root, &["var", "GIT_ATTR_GLOBAL"])? {
        Some(path) => trimmed(path),
        // `git var` gained the attribute paths in 2.42.
        None => match git(root, &["config", "--path", "--get", "core.attributesFile"])? {
            Some(path) => trimmed(path),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
                .map(|config| config.join("git/attributes")),
        },
    };
    push(b"global", &global.as_deref().map(read).unwrap_or_default());
    // Before 2.42, and under `GIT_ATTR_NOSYSTEM`, `git var` names no system
    // file. Its prefix-relative default is not derivable from `--exec-path`
    // (a `/usr` build reads `/etc/gitattributes`), so the file is recorded as
    // unknown rather than guessed.
    let system = match git(root, &["var", "GIT_ATTR_SYSTEM"])? {
        Some(path) => trimmed(path).as_deref().map(read).unwrap_or_default(),
        None => b"<unknown>".to_vec(),
    };
    push(b"system", &system);
    push(
        b"GIT_ATTR_NOSYSTEM",
        &env_bytes("GIT_ATTR_NOSYSTEM").unwrap_or_else(|| b"<unset>".to_vec()),
    );

    let listed =
        git(root, &["config", "--list", "-z"])?.context("listing the git configuration")?;
    let mut attr_tree = None;
    for entry in listed.split(|&b| b == 0).filter(|entry| !entry.is_empty()) {
        let (key, value) = match entry.iter().position(|&b| b == b'\n') {
            Some(newline) => (&entry[..newline], &entry[newline + 1..]),
            None => (entry, &b""[..]),
        };
        if key.eq_ignore_ascii_case(b"attr.tree") {
            attr_tree = Some(value.to_vec());
        }
        if !numstat_config_ignored(key) {
            push(key, value);
        }
    }
    // A bare repository reads attributes from HEAD when `attr.tree` is unset.
    let attr_tree = attr_tree.or_else(|| bare.then(|| b"HEAD".to_vec()));
    for (label, tree_ish) in [
        (&b"attr.tree"[..], attr_tree),
        (b"GIT_ATTR_SOURCE", env_bytes("GIT_ATTR_SOURCE")),
    ] {
        if let Some(tree_ish) = tree_ish {
            push(label, &tree_ish);
            push(label, &tree_attributes(&tree_ish)?);
        }
    }

    Ok(parts)
}

/// The process environment's value for `name`, as the history walk inherits it.
fn env_bytes(name: &str) -> Option<Vec<u8>> {
    let value = std::env::var_os(name)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(value.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Some(value.to_string_lossy().into_owned().into_bytes())
    }
}

/// Whether configuration key `key` (as `git config --list` prints it: section
/// and variable name lowercased, subsection as written) is left out of the
/// numstat fingerprint. Every other key is hashed, so a key nobody listed
/// here costs a re-read of saved output rather than wrong history. Two kinds
/// are left out: keys that never reach the slice reader's output, and keys
/// its command line (`log --no-show-signature --no-walk=unsorted --no-merges
/// --root --no-ext-diff --no-textconv -z -M --numstat --pretty=format:` with
/// `%H`, `%ct`, `%ae`, `%an`, and `%s`) overrides.
fn numstat_config_ignored(key: &[u8]) -> bool {
    let key = String::from_utf8_lossy(key);
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let section = section.to_ascii_lowercase();
    let (subsection, name) = match rest.rsplit_once('.') {
        Some((subsection, name)) => (Some(subsection), name.to_ascii_lowercase()),
        None => (None, rest.to_ascii_lowercase()),
    };
    // Sections that only tools, transports, the worktree, or other commands
    // read: what `git switch`, `push -u`, editors, and maintenance write.
    const VOLATILE_SECTIONS: &[&str] = &[
        "advice",
        "alias",
        "am",
        "author",
        "blame",
        "branch",
        "checkout",
        "clone",
        "color",
        "commit",
        "committer",
        "credential",
        "difftool",
        "fetch",
        "filter",
        "format",
        "fsmonitor",
        "gc",
        "gpg",
        "grep",
        "gui",
        "guitool",
        "help",
        "http",
        "imap",
        "include",
        "includeif",
        "index",
        "init",
        "instaweb",
        "interactive",
        "lfs",
        "maintenance",
        "man",
        "merge",
        "mergetool",
        "notes",
        "pack",
        "pager",
        "protocol",
        "pull",
        "push",
        "rebase",
        "receive",
        "remote",
        "repack",
        "rerere",
        "safe",
        "sendemail",
        "sequence",
        "stash",
        "status",
        "tag",
        "trace2",
        "transfer",
        "uploadarchive",
        "uploadpack",
        "url",
        "user",
        "web",
        "worktree",
    ];
    if VOLATILE_SECTIONS.contains(&section.as_str()) {
        return true;
    }
    match (section.as_str(), subsection, name.as_str()) {
        // Editors, credentials, hooks, and worktree bookkeeping; the
        // worktree `.gitattributes` listing these affect is hashed directly.
        (
            "core",
            None,
            "editor" | "pager" | "askpass" | "sshcommand" | "hookspath" | "excludesfile"
            | "untrackedcache" | "preloadindex" | "fscache" | "logallrefupdates"
            | "sharedrepository" | "autocrlf" | "eol" | "safecrlf" | "filemode" | "symlinks"
            | "commentchar" | "commentstring" | "splitindex" | "multipackindex" | "commitgraph",
        ) => true,
        ("core", None, name) if name.starts_with("fsmonitor") => true,
        // `-z` prints paths verbatim.
        ("core", None, "quotepath") => true,
        // `%H` prints the full object name.
        ("core", None, "abbrev") => true,
        // `--no-show-signature`.
        ("log", None, "showsignature") => true,
        // `--root`.
        ("log", None, "showroot") => true,
        // The format prints no date but `%ct`.
        ("log", None, "date") => true,
        // The format has no `%d`.
        ("log", None, "decorate" | "excludedecoration" | "initialdecorationset") => true,
        // `%H`.
        ("log", None, "abbrevcommit") => true,
        // `%ae` and `%an` ignore the mailmap; `%aE`/`%aN` would not.
        ("log", None, "mailmap") => true,
        // Needs a single pathspec, and the walk passes none.
        ("log", None, "follow") => true,
        // `--no-merges` lists no merge to diff.
        ("log", None, "diffmerges") => true,
        // No graph is drawn.
        ("log", None, "graphcolors") => true,
        // `-M` turns rename detection on whatever `diff.renames` says.
        ("diff", None, "renames") => true,
        // `--no-ext-diff`.
        ("diff", None, "external") => true,
        // Tools `git difftool` runs, not `git log`.
        ("diff", None, "tool" | "guitool") => true,
        // Numstat prints paths without `a/` and `b/` prefixes.
        ("diff", None, "mnemonicprefix" | "noprefix" | "srcprefix" | "dstprefix") => true,
        // Patch, stat, dirstat, and color rendering, none of which numstat prints.
        (
            "diff",
            None,
            "statgraphwidth" | "statnamewidth" | "dirstat" | "colormoved" | "colormovedws"
            | "wserrorhighlight" | "suppressblankempty",
        ) => true,
        // `--no-textconv` and `--no-ext-diff` turn these driver settings off;
        // hunk headers and word diffs are not part of numstat.
        (
            "diff",
            Some(_),
            "textconv" | "cachetextconv" | "command" | "trustexitcode" | "xfuncname" | "funcname"
            | "wordregex",
        ) => true,
        // How `git submodule` clones and updates, not whether numstat shows a
        // gitlink change; `submodule.<name>.ignore` is still hashed.
        (
            "submodule",
            _,
            "url"
            | "branch"
            | "update"
            | "active"
            | "fetchrecursesubmodules"
            | "shallow"
            | "recurse"
            | "fetchjobs"
            | "propagatebranches"
            | "alternatelocation"
            | "alternateerrorstrategy",
        ) => true,
        _ => false,
    }
}

const NUMSTAT_ENVIRONMENT: &str = "\0numstat-environment:";

/// What staged output must share to be reused or resumed: the history policy
/// and the environment the output was read under. Its git children stop at
/// `deadline`.
fn staging_key(root: &Path, policy: &str, deadline: Instant, limits: WalkLimits) -> Result<String> {
    environment_key(root, policy, deadline, limits, HistoryWalk::Environment).map_err(|error| {
        let budget = is_deadline(&error).then_some(limits.budget);
        error.context(FingerprintFailed { budget })
    })
}

fn environment_key(
    root: &Path,
    policy: &str,
    deadline: Instant,
    limits: WalkLimits,
    walk: HistoryWalk,
) -> Result<String> {
    let fingerprint = limits.run(walk, || {
        #[cfg(test)]
        std::thread::sleep(limits.environment_delay);
        numstat_environment_fingerprint(root, deadline, limits.budget)
    })?;
    Ok(format!("{policy}{NUMSTAT_ENVIRONMENT}{fingerprint}"))
}

/// Time a re-check of the numstat environment may run past the run's
/// deadline, given how long the last fingerprint took: twice that, at least
/// 250 ms, at most `cap` ([`MAX_RECHECK_ALLOWANCE`] outside tests). Killing
/// and reaping a slice the deadline stopped cannot starve a re-check, which
/// has this allowance of its own; a staged rebuild therefore runs at most one
/// allowance past its budget.
fn recheck_allowance(last: Duration, cap: Duration) -> Duration {
    last.saturating_mul(2)
        .max(Duration::from_millis(250))
        .min(cap)
}

const MAX_RECHECK_ALLOWANCE: Duration = Duration::from_secs(5);

/// A plan or slice whose re-check of the numstat environment did not confirm
/// the key it was read under, so it was not saved.
#[derive(Debug)]
struct ReadNotSaved {
    target: String,
    /// Commits in the slice; zero for the plan.
    slice: usize,
    why: NotSavedWhy,
}

#[derive(Debug, Clone, Copy)]
enum NotSavedWhy {
    /// The environment changed while it was read.
    Changed,
    /// The re-check took longer than the time it was given: the rest of the
    /// run's budget or its re-check allowance, whichever ended later.
    OutOfTime {
        given: Duration,
        allowance: Duration,
    },
    /// The re-check failed; the error it wraps says why.
    Failed,
}

impl NotSavedWhy {
    /// The discard cause recorded on the rebuild.
    fn cause(self) -> &'static str {
        match self {
            NotSavedWhy::Changed => DISCARD_ENVIRONMENT_CHANGED,
            NotSavedWhy::OutOfTime { .. } => DISCARD_RECHECK_TIMEOUT,
            NotSavedWhy::Failed => "recheck_failed",
        }
    }
}

impl std::fmt::Display for ReadNotSaved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let target = short_sha(&self.target);
        let what = match self.slice {
            0 => format!("the commit list toward {target}"),
            1 => format!("a slice of 1 commit toward {target}"),
            slice => format!("a slice of {slice} commits toward {target}"),
        };
        match self.why {
            NotSavedWhy::Changed => write!(
                f,
                "the numstat environment (git version, configuration, attributes, replace refs, \
                 grafts, submodules) changed while this run read {what}, so it was not saved; \
                 the next run reads it again under the current environment"
            ),
            NotSavedWhy::OutOfTime { given, allowance } => write!(
                f,
                "re-checking the numstat environment after this run read {what} took longer \
                 than the {} ms it had (its {} ms re-check allowance or the rest of the run's \
                 budget, whichever ended later), so it was not saved",
                given.as_millis(),
                allowance.as_millis()
            ),
            NotSavedWhy::Failed => write!(
                f,
                "re-checking the numstat environment after this run read {what} failed, so it \
                 was not saved"
            ),
        }
    }
}

/// The numstat environment could not be fingerprinted, so no saved output
/// can be trusted this run. `budget` is set when it ran out of time.
#[derive(Debug)]
struct FingerprintFailed {
    budget: Option<Duration>,
}

impl std::fmt::Display for FingerprintFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fingerprinting the numstat environment (configuration, attributes, replace refs, \
             grafts, submodules) for the staged rebuild"
        )?;
        if let Some(budget) = self.budget {
            write!(f, " within the {} ms budget", budget.as_millis())?;
        }
        Ok(())
    }
}

/// Whether the saved rebuild's target, policy, and window let a pass at
/// `head_sha` finish it.
fn resumable_target(
    root: &Path,
    head_sha: &str,
    key: &str,
    policy: &str,
    target: &str,
    cutoff: i64,
) -> Result<bool> {
    Ok(policy == key
        && (target == head_sha || is_ancestor(root, target, head_sha)?)
        && anchor_at_commit(root, target)?.cutoff() == cutoff)
}

/// Whether a pass whose stored head is `last_sha` continues the saved rebuild
/// instead of taking the same-HEAD or incremental path. Only a rebuild
/// started against this stored head for a reason other than divergence
/// qualifies: one left by an incremental range or rename re-key that failed,
/// which would fail the same way again, or by `git-index --full`. A rebuild
/// started because the stored head had diverged is kept for its commits, but
/// HEAD has since returned to the stored head's line. A rebuild whose last
/// run saved nothing or failed does not qualify either, so it cannot hold
/// history at the stored head on every run; it stays saved and reported, and
/// `git-index --full`, or a run that makes progress on it, resumes it.
///
/// The numstat environment is not compared: the run that continues the
/// rebuild re-plans it when the environment changed, and the reason to
/// prefer it over the shortcut does not depend on that environment. The
/// status row and the ancestry of its target decide.
fn continues_saved_rebuild(
    db: &Database,
    root: &Path,
    head_sha: &str,
    last_sha: &str,
    policy: &str,
) -> Result<bool> {
    let Some(status) = db.git_history_build_status()? else {
        return Ok(false);
    };
    if status.cause == CAUSE_DIVERGED
        || status.stored_sha.as_deref() != Some(last_sha)
        || status.outcome == OUTCOME_NO_PROGRESS
        || status.outcome == OUTCOME_FAILED
        || !status
            .policy
            .strip_prefix(policy)
            .is_some_and(|rest| rest.starts_with(NUMSTAT_ENVIRONMENT))
    {
        return Ok(false);
    }
    Ok(
        (status.target_sha == head_sha || is_ancestor(root, &status.target_sha, head_sha)?)
            && anchor_at_commit(root, &status.target_sha)?.cutoff() == status.cutoff,
    )
}

fn stage_history_slices(
    db: &Database,
    root: &Path,
    head_sha: &str,
    anchors: &PassAnchors<'_>,
    reason: FullReason<'_>,
    run: &RunTrace,
) -> Result<Staged> {
    let limits = anchors.limits;
    let source = anchors.source;
    let stored_head = db.get_git_index_state()?.map(|(sha, _)| sha);
    let (run_cause, run_stored) = reason.cause(stored_head.as_deref());
    // The environment fingerprint, the commit listing, and every slice share
    // this one deadline; each re-check may run a bounded allowance past it.
    let deadline = Instant::now() + limits.budget;
    let fingerprinted = Instant::now();
    let key = staging_key(root, anchors.exclusion_fingerprint, deadline, limits)?;
    let fingerprint_took = fingerprinted.elapsed();
    if fingerprint_took > limits.budget / 2 {
        return stage_without_slices(
            db,
            root,
            head_sha,
            anchors,
            reason,
            run,
            &key,
            deadline,
            fingerprint_took,
        );
    }
    let mut last_fingerprint = fingerprint_took;
    // Fingerprints the environment again after a plan or slice was read and
    // before it is saved, so output read under an environment that moved
    // since the run's first fingerprint never reaches the staging tables.
    let mut recheck = |target: &str, slice: usize| -> Result<Option<ReadNotSaved>> {
        let started = Instant::now();
        let allowance = recheck_allowance(last_fingerprint, limits.max_recheck_allowance);
        let until = deadline.max(started + allowance);
        let rechecked = environment_key(
            root,
            anchors.exclusion_fingerprint,
            until,
            limits,
            HistoryWalk::Recheck,
        );
        last_fingerprint = started.elapsed();
        let not_saved = |why| ReadNotSaved {
            target: target.to_owned(),
            slice,
            why,
        };
        match rechecked {
            Ok(now) if now == key => Ok(None),
            Ok(_) => Ok(Some(not_saved(NotSavedWhy::Changed))),
            Err(error) if is_deadline(&error) => Ok(Some(not_saved(NotSavedWhy::OutOfTime {
                given: until.saturating_duration_since(started),
                allowance,
            }))),
            Err(error) => Err(error.context(not_saved(NotSavedWhy::Failed))),
        }
    };
    let resumed = resumable_build(db, root, head_sha, &key)?;
    if let Some(build) = &resumed {
        run.working_on.set(Some(build.target_sha.clone()));
    }
    let planned = resumed.is_none();
    let (build, cause, stored_sha) = match resumed {
        // A rebuild resumed without a cause of its own keeps the one it
        // was started for.
        Some(build) if matches!(reason, FullReason::Rebuild) => {
            let (cause, stored) = (build.cause.clone(), build.stored_sha.clone());
            (build, cause, stored)
        }
        Some(build) => (build, run_cause.to_owned(), run_stored.map(str::to_owned)),
        None => {
            run.planning.set(Some(head_sha.to_owned()));
            let cutoff = anchors.current.cutoff();
            let listed = limits.run(HistoryWalk::Plan, || {
                plan_history(
                    root,
                    head_sha,
                    cutoff,
                    deadline.saturating_duration_since(Instant::now()),
                )
            });
            let commits = match listed {
                Ok(commits) => commits,
                Err(error) if is_deadline(&error) => {
                    // A rebuild saved for another target keeps its state and
                    // commits; the failed listing is only noted on it.
                    db.set_git_history_build_note(&format!(
                        "listing the commits toward {} ran out of time",
                        short_sha(head_sha)
                    ))?;
                    return Ok(Staged::Incomplete(BuildProgress {
                        target: head_sha.to_owned(),
                        staged: 0,
                        total: 0,
                        slice_len: 0,
                        stalled: Some(
                            "listing the commits in the history window ran out of time".into(),
                        ),
                        discarded: None,
                    }));
                }
                Err(error) => return Err(error),
            };
            let slice_len = limits.first_slice.clamp(1, MAX_SLICE_COMMITS);
            #[cfg(test)]
            if let Some(before_plan_recheck) = limits.before_plan_recheck {
                before_plan_recheck(root);
            }
            // Grafts and replace refs decide which commits the plan lists.
            if let Some(not_saved) = recheck(head_sha, 0)? {
                let why = not_saved.to_string();
                db.record_git_history_discard(&one_line(&why), not_saved.why.cause(), None)?;
                return Ok(Staged::Incomplete(BuildProgress {
                    target: head_sha.to_owned(),
                    staged: 0,
                    total: commits.len(),
                    slice_len,
                    stalled: None,
                    discarded: Some(why),
                }));
            }
            let mut staged = 0;
            db.execute_batch(|db| {
                staged = db.start_git_history_build(
                    head_sha,
                    &key,
                    cutoff,
                    &commits,
                    &GitHistoryProgress {
                        slice_len,
                        cause: run_cause,
                        stored_sha: run_stored,
                        outcome: OUTCOME_PROGRESSING,
                        stalled: None,
                        run_saved: 0,
                    },
                )?;
                Ok(())
            })?;
            run.planning.set(None);
            run.working_on.set(Some(head_sha.to_owned()));
            if staged > 0 {
                tracing::info!(
                    root = %root.display(),
                    staged,
                    total = commits.len(),
                    "reusing staged git history commits for a new rebuild target"
                );
            }
            let build = GitHistoryBuild {
                target_sha: head_sha.to_owned(),
                policy: key.clone(),
                cutoff,
                commits,
                slice_len,
                staged,
                cause: run_cause.to_owned(),
                stored_sha: run_stored.map(str::to_owned),
            };
            (build, run_cause.to_owned(), run_stored.map(str::to_owned))
        }
    };

    let saved = db.git_history_build_staged_shas()?;
    let pending: Vec<String> = build
        .commits
        .iter()
        .filter(|sha| !saved.contains(*sha))
        .cloned()
        .collect();
    drop(saved);
    let base = GitHistoryProgress {
        slice_len: build.slice_len,
        cause: &cause,
        stored_sha: stored_sha.as_deref(),
        outcome: OUTCOME_PROGRESSING,
        stalled: None,
        run_saved: 0,
    };

    let total = build.commits.len();
    let mut read = 0usize;
    let mut slice_len = build.slice_len.clamp(1, MAX_SLICE_COMMITS);
    let mut slices = 0usize;
    let mut advanced = planned;
    let mut stalled = None;
    while read < pending.len() {
        if limits.max_slices.is_some_and(|max| slices >= max) {
            break;
        }
        let started = Instant::now();
        let given = deadline.saturating_duration_since(started);
        if given.is_zero() {
            break;
        }
        let shas = &pending[read..(read + slice_len).min(pending.len())];
        let output = limits.run(HistoryWalk::Slice(shas.len()), || {
            read_history_slice(root, shas, source.tree_only, given)
        });
        match output {
            Ok(output) => {
                // Headers, not parsed commits: a single walk also drops a
                // commit whose date does not parse, and so must the slice.
                let commits = split_history(&output, source)?;
                anyhow::ensure!(
                    commits.iter().map(|(sha, _)| sha).eq(shas.iter()),
                    "git log returned other commits than the {} requested from {}",
                    shas.len(),
                    shas[0]
                );
                let next = next_slice_len(shas.len(), started.elapsed(), limits.budget);
                #[cfg(test)]
                if let Some(before_recheck) = limits.before_recheck {
                    before_recheck(root);
                }
                if let Some(not_saved) = recheck(&build.target_sha, shas.len())? {
                    // The rebuild keeps the state its last saved slice or run
                    // left; only the reason is recorded. A smaller slice ends
                    // earlier, leaving its re-check more of the budget.
                    let why = not_saved.to_string();
                    let shrunk = matches!(not_saved.why, NotSavedWhy::OutOfTime { .. })
                        .then(|| (shas.len() / 2).max(1));
                    db.record_git_history_discard(&one_line(&why), not_saved.why.cause(), shrunk)?;
                    return Ok(Staged::Incomplete(BuildProgress {
                        target: build.target_sha,
                        staged: total - (pending.len() - read),
                        total,
                        slice_len: shrunk.unwrap_or(slice_len),
                        stalled: None,
                        discarded: Some(why),
                    }));
                }
                let outputs: Vec<(&str, &str)> = commits
                    .iter()
                    .map(|(sha, output)| (sha.as_str(), *output))
                    .collect();
                db.execute_batch(|db| {
                    db.stage_git_history_commits(
                        &outputs,
                        &GitHistoryProgress {
                            slice_len: next,
                            run_saved: read + shas.len(),
                            ..base
                        },
                    )?;
                    // Resumed means planned under this same environment, so
                    // the re-plans that kept starting over have stopped.
                    if !planned {
                        db.clear_git_history_restarts()?;
                    }
                    Ok(())
                })?;
                read += shas.len();
                run.saved.set(read);
                slice_len = next;
                slices += 1;
                advanced = true;
                #[cfg(test)]
                if let Some(after_slice) = limits.after_slice {
                    after_slice(root);
                }
            }
            Err(error) if is_deadline(&error) => {
                // Only a slice that had most of the budget says the slice is too large.
                // Time the fingerprint took before the slice is not the slice's.
                if slices == 0 && given + fingerprint_took >= limits.budget / 2 {
                    if shas.len() > 1 {
                        slice_len = shas.len() / 2;
                        db.set_git_history_progress(&GitHistoryProgress {
                            slice_len,
                            run_saved: read,
                            ..base
                        })?;
                        advanced = true;
                    } else {
                        stalled = Some(format!(
                            "commit {} alone takes longer than the budget",
                            short_sha(&shas[0])
                        ));
                    }
                }
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if read < pending.len() {
        let stalled = (!advanced).then(|| stalled.unwrap_or_else(|| "no slice was read".into()));
        if stalled.is_some() {
            db.set_git_history_progress(&GitHistoryProgress {
                slice_len,
                outcome: OUTCOME_NO_PROGRESS,
                stalled: stalled.as_deref(),
                run_saved: read,
                ..base
            })?;
        }
        return Ok(Staged::Incomplete(BuildProgress {
            target: build.target_sha,
            staged: total - (pending.len() - read),
            total,
            slice_len,
            stalled,
            discarded: None,
        }));
    }

    let assembled = (|| -> Result<Vec<Commit>> {
        let outputs = db.git_history_build_outputs()?;
        anyhow::ensure!(
            outputs.len() == total,
            "staged git history holds {} of {total} planned commits",
            outputs.len()
        );
        let mut commits = Vec::with_capacity(total);
        for sha in &build.commits {
            let output = outputs
                .get(sha)
                .with_context(|| format!("staged git history lacks planned commit {sha}"))?;
            let (parsed, headers) = parse_history_with_headers(output, source)?;
            anyhow::ensure!(
                headers.len() == 1 && headers[0].sha == *sha,
                "staged git history for commit {} holds {} commit headers",
                short_sha(sha),
                headers.len()
            );
            commits.extend(parsed);
        }
        Ok(commits)
    })();
    match assembled {
        Ok(commits) => Ok(Staged::Complete {
            target: build.target_sha,
            commits,
        }),
        Err(error) => {
            // Re-plan rather than fail on the same staged rows every run.
            db.clear_git_history_build()?;
            Err(error)
        }
    }
}

/// A staged rebuild run whose environment fingerprint took more than half the
/// budget: every slice's re-check would take as long again, so no slice is
/// read. The rebuild this run resumes, or a new plan for `head_sha`, records
/// `no_progress` with the budget that would fit, so doctor and the error
/// name the recovery instead of each run discarding what it read.
#[allow(clippy::too_many_arguments)]
fn stage_without_slices(
    db: &Database,
    root: &Path,
    head_sha: &str,
    anchors: &PassAnchors<'_>,
    reason: FullReason<'_>,
    run: &RunTrace,
    key: &str,
    deadline: Instant,
    took: Duration,
) -> Result<Staged> {
    let limits = anchors.limits;
    let needed = (took.as_millis() * 4).div_ceil(1000) * 1000;
    let stalled = format!(
        "fingerprinting the numstat environment took {} ms, more than half the {} ms budget, \
         so no slice's re-check fits in it; set CODESAGE_GIT_HISTORY_TIMEOUT_MS to at least {}",
        took.as_millis(),
        limits.budget.as_millis(),
        needed.min(600_000)
    );
    let stored_head = db.get_git_index_state()?.map(|(sha, _)| sha);
    let (run_cause, run_stored) = reason.cause(stored_head.as_deref());
    let progress = |slice_len, cause, stored_sha| GitHistoryProgress {
        slice_len,
        cause,
        stored_sha,
        outcome: OUTCOME_NO_PROGRESS,
        stalled: Some(&stalled),
        run_saved: 0,
    };
    if let Some(build) = resumable_build(db, root, head_sha, key)? {
        run.working_on.set(Some(build.target_sha.clone()));
        let (cause, stored) = if matches!(reason, FullReason::Rebuild) {
            (build.cause.as_str(), build.stored_sha.as_deref())
        } else {
            (run_cause, run_stored)
        };
        db.set_git_history_progress(&progress(build.slice_len, cause, stored))?;
        return Ok(Staged::Incomplete(BuildProgress {
            target: build.target_sha,
            staged: build.staged,
            total: build.commits.len(),
            slice_len: build.slice_len,
            stalled: Some(stalled),
            discarded: None,
        }));
    }
    // Only grafts and replace refs decide which commits the plan lists, and
    // they are read quickly, so the plan is checked against them rather than
    // against a second slow fingerprint.
    run.planning.set(Some(head_sha.to_owned()));
    let before = plan_dependencies(root, deadline, limits.budget)?;
    let cutoff = anchors.current.cutoff();
    let listed = limits.run(HistoryWalk::Plan, || {
        plan_history(
            root,
            head_sha,
            cutoff,
            deadline.saturating_duration_since(Instant::now()),
        )
    });
    let commits = match listed {
        Ok(commits) => commits,
        Err(error) if is_deadline(&error) => {
            db.set_git_history_build_note(&format!(
                "listing the commits toward {} ran out of time",
                short_sha(head_sha)
            ))?;
            return Ok(Staged::Incomplete(BuildProgress {
                target: head_sha.to_owned(),
                staged: 0,
                total: 0,
                slice_len: 0,
                stalled: Some(stalled),
                discarded: None,
            }));
        }
        Err(error) => return Err(error),
    };
    if plan_dependencies(root, deadline, limits.budget)? != before {
        let why = ReadNotSaved {
            target: head_sha.to_owned(),
            slice: 0,
            why: NotSavedWhy::Changed,
        }
        .to_string();
        db.record_git_history_discard(&one_line(&why), DISCARD_ENVIRONMENT_CHANGED, None)?;
        return Ok(Staged::Incomplete(BuildProgress {
            target: head_sha.to_owned(),
            staged: 0,
            total: commits.len(),
            slice_len: 0,
            stalled: None,
            discarded: Some(why),
        }));
    }
    let slice_len = limits.first_slice.clamp(1, MAX_SLICE_COMMITS);
    let mut staged = 0;
    db.execute_batch(|db| {
        staged = db.start_git_history_build(
            head_sha,
            key,
            cutoff,
            &commits,
            &progress(slice_len, run_cause, run_stored),
        )?;
        Ok(())
    })?;
    run.planning.set(None);
    run.working_on.set(Some(head_sha.to_owned()));
    Ok(Staged::Incomplete(BuildProgress {
        target: head_sha.to_owned(),
        staged,
        total: commits.len(),
        slice_len,
        stalled: Some(stalled),
        discarded: None,
    }))
}

/// What decides the commits a plan lists besides HEAD and the window:
/// `info/grafts`, the `refs/replace/` refs, and `GIT_NO_REPLACE_OBJECTS`.
fn plan_dependencies(root: &Path, deadline: Instant, budget: Duration) -> Result<Vec<u8>> {
    let git = |args: &[&str]| -> Result<Vec<u8>> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HistoryDeadline { budget }.into());
        }
        let mut command = git_history_walk_command(root);
        command.args(args);
        let output = history_output(command, left)
            .with_context(|| format!("running git {} in {}", args.join(" "), root.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed in {}",
            args.join(" "),
            root.display()
        );
        Ok(output.stdout)
    };
    let grafts = git(&["rev-parse", "--git-path", "info/grafts"])?;
    let grafts = path_from_bytes(grafts.trim_ascii_end());
    let grafts = if grafts.is_absolute() {
        grafts
    } else {
        root.join(grafts)
    };
    let mut parts = std::fs::read(&grafts).unwrap_or_else(|_| b"<absent>".to_vec());
    parts.push(0);
    parts.extend(git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/replace/",
    ])?);
    parts.push(0);
    parts.extend(env_bytes("GIT_NO_REPLACE_OBJECTS").unwrap_or_else(|| b"<unset>".to_vec()));
    Ok(parts)
}

/// Size the next slice so it takes about a quarter of the budget, growing at
/// most fourfold per slice.
fn next_slice_len(len: usize, took: Duration, budget: Duration) -> usize {
    let target = budget.as_secs_f64() / 4.0;
    let scaled = len as f64 * target / took.as_secs_f64().max(1e-3);
    let ceiling = len.saturating_mul(4).clamp(1, MAX_SLICE_COMMITS);
    (scaled as usize).clamp(1, ceiling)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The error for a full rebuild this run could not finish. It names how to
/// finish it; a rebuild forced by a diverged stored head also names that cause,
/// since rerunning the hook alone repeats it otherwise.
fn rebuild_incomplete(
    progress: &BuildProgress,
    reason: FullReason<'_>,
    head_sha: &str,
    budget: Duration,
) -> anyhow::Error {
    let budget_ms = budget.as_millis();
    let cause = match reason {
        FullReason::Diverged { stored } => format!(
            "stored git history head {} is not an ancestor of HEAD {}, so history must be rebuilt in full; ",
            short_sha(stored),
            short_sha(head_sha)
        ),
        FullReason::RangeTooLong { stored } => format!(
            "reading the commits from stored git history head {} to HEAD {} took longer than \
             the {budget_ms} ms budget, so history is rebuilt in full; ",
            short_sha(stored),
            short_sha(head_sha)
        ),
        FullReason::Rebuild | FullReason::Requested => String::new(),
    };
    match &progress.stalled {
        Some(stall) => anyhow!(
            "{cause}git history rebuild made no progress within the {budget_ms} ms budget: \
             {stall}; the previous history index is unchanged. Rerun with a larger budget: \
             `CODESAGE_GIT_HISTORY_TIMEOUT_MS=<ms> codesage git-index --full` (at most 600000)"
        ),
        None => anyhow!(
            "{cause}git history rebuild in progress: {} of {} commits saved toward {} within \
             the {budget_ms} ms budget, next slice {} commits{}; the previous history index is \
             unchanged until the rebuild completes. Rerun `codesage git-index` to continue; \
             `CODESAGE_GIT_HISTORY_TIMEOUT_MS=600000 codesage git-index --full` finishes in one \
             run when the remaining slices fit in that budget",
            progress.staged,
            progress.total,
            short_sha(&progress.target),
            progress.slice_len,
            progress
                .discarded
                .as_deref()
                .map(|why| format!("; {why}"))
                .unwrap_or_default()
        ),
    }
}

/// What a pass that may not stage says about the indexed head it starts
/// from: either a rebuild this run just completed there, or the history it
/// fell back to after the saved rebuild could not fingerprint its
/// environment.
fn no_stage_context(limits: WalkLimits, last_sha: &str) -> String {
    if limits.may_preempt {
        format!("git history rebuilt at {}", short_sha(last_sha))
    } else {
        format!(
            "the saved rebuild could not fingerprint its environment, so this run indexed \
             incrementally from {}",
            short_sha(last_sha)
        )
    }
}

fn run_incremental(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
    last_sha: &str,
    anchors: PassAnchors<'_>,
) -> Result<GitIndexStats> {
    let anchor = anchors.current;
    let range = format!("{last_sha}..{head_sha}");
    let walked = anchors.limits.run(HistoryWalk::Incremental, || {
        history_commits(
            root,
            Some(&range),
            anchor.cutoff(),
            anchors.source,
            anchors.limits.budget,
        )
    });
    let mut commits = match walked {
        Ok(commits) => commits,
        // Retrying the same range would fail the same way every run; a staged
        // rebuild saves progress and leaves the current rows in place meanwhile.
        Err(error) if is_deadline(&error) && !anchors.limits.may_stage => {
            return Err(anyhow!(
                "{}; reading the commits from there to HEAD {} took longer than the {} ms \
                 budget, so the next `codesage git-index` run rebuilds them",
                no_stage_context(anchors.limits, last_sha),
                short_sha(head_sha),
                anchors.limits.budget.as_millis()
            ));
        }
        Err(error) if is_deadline(&error) => {
            tracing::info!(
                root = %root.display(),
                "the incremental range exceeds the history budget; rebuilding in slices"
            );
            return run_full(
                db,
                root,
                exclude_set,
                test_like_set,
                head_sha,
                anchors,
                FullReason::RangeTooLong { stored: last_sha },
            );
        }
        Err(error) => return Err(error),
    };
    let successor = follow_renames(&mut commits, exclude_set);
    remap_dead_paths(root, head_sha, &mut commits, &successor)?;
    // Stored rows are keyed by the names files had at `last_sha`; a rename in
    // this range moves them, exactly as a full scan would key those commits.
    let moves: Vec<(String, Option<String>)> = successor
        .into_iter()
        .map(|(from, to)| {
            let kept = !is_excluded(exclude_set, &from) && !is_excluded(exclude_set, &to);
            (from, kept.then_some(to))
        })
        .collect();
    if rekey_diverges_from_full_scan(db, &moves, test_like_set)? {
        anyhow::ensure!(
            anchors.limits.may_stage,
            "{}; a rename between there and HEAD {} needs a full rescan, which the next \
             `codesage git-index` run performs",
            no_stage_context(anchors.limits, last_sha),
            short_sha(head_sha)
        );
        tracing::info!(
            root = %root.display(),
            "a rename in the incremental range cannot be re-keyed exactly; rescanning in full"
        );
        return run_full(
            db,
            root,
            exclude_set,
            test_like_set,
            head_sha,
            anchors,
            FullReason::Rebuild,
        );
    }

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
        db.rekey_git_history(&moves)?;
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
        db.set_git_index_exclusion_fingerprint(anchors.exclusion_fingerprint)?;
        db.park_git_history_build(
            OUTCOME_PARKED,
            OUTCOME_PROGRESSING,
            &[CAUSE_RANGE_TOO_LONG, CAUSE_DIVERGED],
        )
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
        notes: Vec::new(),
    })
}

/// Oldest commit timestamp a pass measuring from `anchor` admits. Clamped at
/// the epoch so `git log --since=@<n>` never receives a negative second.
fn history_window_cutoff(anchor: i64) -> i64 {
    (anchor - (HISTORY_WINDOW_DAYS * SECONDS_PER_DAY) as i64).max(0)
}

fn write_author_events(db: &Database, commits: &[Commit], excludes: &GlobSet) -> Result<()> {
    for commit in commits {
        if commit.boundary {
            continue;
        }
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

/// Key every change to the path its file carries at the newest commit in
/// `commits`, so history survives `git mv`. `commits` must list children before
/// parents, as `run_git_log` does: a rename then applies to everything older.
///
/// A change whose historical path is excluded is dropped, since a pass that
/// never stored it could not re-key it later. Changes a rename folds onto one
/// path within a commit merge, so the commit counts once for that path.
/// Returns each superseded path's current name.
fn follow_renames(commits: &mut [Commit], exclude_set: &GlobSet) -> HashMap<String, String> {
    let mut successor: HashMap<String, String> = HashMap::new();
    for commit in commits.iter_mut() {
        let keyed: Vec<FileChange> = std::mem::take(&mut commit.changes)
            .into_iter()
            .filter(|change| !is_excluded(exclude_set, &change.path))
            .map(|change| FileChange {
                path: successor.get(&change.path).cloned().unwrap_or(change.path),
                ..change
            })
            .collect();
        commit.changes = merge_same_path(keyed);
        // Resolve every rename against the state before this commit, so a swap
        // inside one commit does not chase its own entries.
        let moves: Vec<(String, String)> = commit
            .renames
            .iter()
            .map(|(old, new)| {
                let current = successor.get(new).cloned().unwrap_or_else(|| new.clone());
                (old.clone(), current)
            })
            .collect();
        for (old, current) in moves {
            if old == current {
                successor.remove(&old);
            } else {
                successor.insert(old, current);
            }
        }
    }
    successor
}

/// Whether re-keying stored rows by `moves` would disagree with a full scan.
/// A successor that already holds history of its own would be merged with it,
/// counting a commit that touched both paths twice; a move across the test-like
/// boundary changes which co-change pairs a full scan admits. Two recorded
/// sources folding onto one successor double-count the same way. Renames are
/// rare, so a rescan costs little and keeps the rows exact.
fn rekey_diverges_from_full_scan(
    db: &Database,
    moves: &[(String, Option<String>)],
    test_like_set: &GlobSet,
) -> Result<bool> {
    let sources: std::collections::HashSet<&str> =
        moves.iter().map(|(from, _)| from.as_str()).collect();
    let mut recorded_sources: HashMap<&str, usize> = HashMap::new();
    for (from, to) in moves {
        let Some(to) = to else {
            continue;
        };
        if test_like_set.is_match(from) != test_like_set.is_match(to) {
            return Ok(true);
        }
        if !sources.contains(to.as_str()) && db.git_file(to)?.is_some() {
            return Ok(true);
        }
        if db.git_file(from)?.is_some() {
            let count = recorded_sources.entry(to.as_str()).or_default();
            *count += 1;
            if *count > 1 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Re-key changes left on a path HEAD no longer holds. `--topo-order` can list
/// a parallel branch's edit to a file before the rename that happened on the
/// other side of a merge, so `follow_renames` saw the edit first and kept the
/// old path. Following the pass's final rename map (chains included) to a
/// path HEAD does hold recovers it. Only runs when the pass saw a rename.
fn remap_dead_paths(
    root: &Path,
    head_sha: &str,
    commits: &mut [Commit],
    successor: &HashMap<String, String>,
) -> Result<()> {
    if successor.is_empty() {
        return Ok(());
    }
    let live = head_paths(root, head_sha)?;
    for commit in commits.iter_mut() {
        let mut remapped = false;
        for change in &mut commit.changes {
            if live.contains(&change.path) {
                continue;
            }
            let mut path = &change.path;
            for _ in 0..successor.len() {
                let Some(next) = successor.get(path) else {
                    break;
                };
                path = next;
                if live.contains(path) {
                    break;
                }
            }
            if live.contains(path) {
                change.path = path.clone();
                remapped = true;
            }
        }
        if remapped {
            commit.changes = merge_same_path(std::mem::take(&mut commit.changes));
        }
    }
    Ok(())
}

/// Every path in `sha`'s tree, repository-relative like `git log` paths.
fn head_paths(root: &Path, sha: &str) -> Result<std::collections::HashSet<String>> {
    let out = git_history_command(root)
        .args(["ls-tree", "-r", "-z", "--name-only", "--full-tree", sha])
        .output()
        .with_context(|| format!("git ls-tree {sha} in {}", root.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git ls-tree {sha} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // `run_git_log` only yields UTF-8 paths, so a non-UTF-8 tree entry can
    // never match a change and is skipped rather than failing the pass.
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .map(str::to_owned)
        .collect())
}

/// Merge changes that share a path, so the commit counts once for it.
fn merge_same_path(changes: Vec<FileChange>) -> Vec<FileChange> {
    let mut merged: Vec<FileChange> = Vec::with_capacity(changes.len());
    let mut slot: HashMap<String, usize> = HashMap::new();
    for change in changes {
        match slot.get(&change.path) {
            Some(&i) => {
                merged[i].added = merged[i].added.saturating_add(change.added);
                merged[i].deleted = merged[i].deleted.saturating_add(change.deleted);
            }
            None => {
                slot.insert(change.path.clone(), merged.len());
                merged.push(change);
            }
        }
    }
    merged
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
        if is_fix && !commit.boundary {
            stats.fix_count += 1;
        }
        if stats.last_commit_at.is_none_or(|t| t < commit.timestamp) {
            stats.last_commit_at = Some(commit.timestamp);
        }
        let churn_units = ((change.added + change.deleted) as f64) / CHURN_DIVISOR;
        let weight = decay * churn_units.min(CHURN_CLAMP);
        if !commit.boundary {
            stats.churn_score += weight;
        }
    }

    // Source-test pairs support test recommendations; test-test pairs add noise.
    if !commit.boundary && kept_changes.len() <= MAX_FILES_PER_COMMIT_FOR_COCHANGE {
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
    let out = git_history_command(root)
        .args(["rev-parse", "HEAD"])
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
    let out = git_history_command(root)
        // `--` terminates option parsing so `range` is always read as a
        // revision range, never as flags. A rename touches both paths, so
        // rename detection must not collapse the pair to its destination.
        .args([
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            "--relative",
            &range,
            "--",
        ])
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
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
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
    let status = git_history_command(root)
        .args(["merge-base", "--is-ancestor", old, new])
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
    /// `(old, new)` for every rename git detected in this commit, binary
    /// files included.
    renames: Vec<(String, String)>,
    boundary: bool,
}

#[derive(Debug)]
struct FileChange {
    path: String,
    added: u32,
    deleted: u32,
}

fn history_commits(
    root: &Path,
    range: Option<&str>,
    since_epoch: i64,
    source: &HistorySource,
    budget: Duration,
) -> Result<Vec<Commit>> {
    let raw = run_git_log(root, range, since_epoch, source.tree_only, budget)?;
    parse_history(&raw, source)
}

fn parse_history(raw: &str, source: &HistorySource) -> Result<Vec<Commit>> {
    Ok(parse_history_with_headers(raw, source)?.0)
}

/// A commit header record in raw `git log -z` output.
#[derive(Debug, PartialEq, Eq)]
struct Header {
    sha: String,
    /// Byte offset of the header record.
    offset: usize,
}

/// `raw.split('\0')` with each record's byte offset.
fn nul_records(raw: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    raw.split('\0').map(move |record| {
        let start = offset;
        offset += record.len() + 1;
        (start, record)
    })
}

/// [`parse_history`] plus every commit header in `raw`, in order, including
/// headers whose commit the parse dropped.
fn parse_history_with_headers(
    raw: &str,
    source: &HistorySource,
) -> Result<(Vec<Commit>, Vec<Header>)> {
    let (mut commits, headers) = if source.tree_only {
        // The tree-only parse fails on a header it cannot read, so it drops none.
        parse_name_log_with_headers(raw)?
    } else {
        parse_log_with_headers(raw)?
    };
    for commit in &mut commits {
        commit.boundary = source.shallow.contains(&commit.sha);
    }
    Ok((commits, headers))
}

/// Split slice output into `(sha, output)` per commit header. Each part runs
/// from its header record to the next one, so it parses alone exactly as it
/// does inside `raw`: both parsers reset at a header they recognize, and the
/// parsers themselves locate the headers, so a path shaped like a header does
/// not split a commit. Only record separators may precede the first header.
fn split_history<'a>(raw: &'a str, source: &HistorySource) -> Result<Vec<(String, &'a str)>> {
    let (_, headers) = parse_history_with_headers(raw, source)?;
    let first = headers.first().map_or(raw.len(), |header| header.offset);
    anyhow::ensure!(
        raw[..first].bytes().all(|b| b == b'\0' || b == b'\n'),
        "git log printed output ahead of its first commit header"
    );
    let ends = headers
        .iter()
        .skip(1)
        .map(|header| header.offset)
        .chain([raw.len()]);
    Ok(headers
        .iter()
        .zip(ends)
        .map(|(header, end)| (header.sha.clone(), &raw[header.offset..end]))
        .collect())
}

/// The commits a full walk of `target` lists, in its order, without reading
/// any diff. Selection and order options must match [`run_git_log`].
fn plan_history(
    root: &Path,
    target: &str,
    since_epoch: i64,
    budget: Duration,
) -> Result<Vec<String>> {
    let since_arg = format!("--since=@{since_epoch}");
    let mut command = git_history_walk_command(root);
    command.args([
        "log",
        "--no-show-signature",
        "--no-merges",
        "--topo-order",
        &since_arg,
        "--pretty=format:%H",
        target,
        "--",
    ]);
    let output = history_output(command, budget)
        .with_context(|| format!("listing git history in {}", root.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git log failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8(output.stdout).context("git log output not UTF-8")?;
    text.split_whitespace()
        .map(|sha| {
            anyhow::ensure!(
                matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "git log listed {sha:?} where a commit SHA was expected"
            );
            Ok(sha.to_owned())
        })
        .collect()
}

/// [`run_git_log`] output for exactly `shas`, in the given order.
fn read_history_slice(
    root: &Path,
    shas: &[String],
    tree_only: bool,
    budget: Duration,
) -> Result<String> {
    let mut command = git_history_walk_command(root);
    command
        .args([
            "log",
            "--no-show-signature",
            "--no-walk=unsorted",
            "--no-merges",
            "--root",
            "--no-ext-diff",
            "--no-textconv",
            "-z",
        ])
        .args(history_format_args(tree_only))
        .args(shas)
        .arg("--");
    let output = history_output(command, budget)
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

fn history_format_args(tree_only: bool) -> [&'static str; 3] {
    if tree_only {
        [
            "--no-renames",
            "--name-only",
            "--pretty=format:%x00commit\x09%H\x09%ct\x09%ae%x1f%an%x1f%s%x00",
        ]
    } else {
        [
            "-M",
            "--numstat",
            "--pretty=format:commit\x09%H\x09%ct\x09%ae%x1f%an%x1f%s%x00",
        ]
    }
}

fn history_walk_budget() -> Result<std::time::Duration> {
    let millis = match std::env::var("CODESAGE_GIT_HISTORY_TIMEOUT_MS") {
        Ok(value) => value
            .parse::<u64>()
            .context("CODESAGE_GIT_HISTORY_TIMEOUT_MS must be an integer from 1 to 600000")?,
        Err(std::env::VarError::NotPresent) => 20_000,
        Err(error) => return Err(error).context("reading CODESAGE_GIT_HISTORY_TIMEOUT_MS"),
    };
    anyhow::ensure!(
        (1..=600_000).contains(&millis),
        "CODESAGE_GIT_HISTORY_TIMEOUT_MS must be an integer from 1 to 600000"
    );
    Ok(std::time::Duration::from_millis(millis))
}

fn run_git_log(
    root: &Path,
    range: Option<&str>,
    since_epoch: i64,
    tree_only: bool,
    budget: Duration,
) -> Result<String> {
    // `@<unix>` is git's epoch-format specifier; locale-independent.
    let since_arg = format!("--since=@{since_epoch}");
    let mut args: Vec<&str> = vec![
        "log",
        // `log.showSignature` would interleave verification text with records.
        "--no-show-signature",
        "--no-merges",
        // `log.showRoot=false` would drop the root commit's numstat.
        "--root",
        // Children before parents regardless of committer-date skew, which
        // `follow_renames` needs to apply a rename to everything older.
        "--topo-order",
        "--no-ext-diff",
        "--no-textconv",
        "-z",
        &since_arg,
    ];
    args.extend(history_format_args(tree_only));
    if let Some(r) = range {
        args.push(r);
    }
    args.push("--");
    let mut command = git_history_walk_command(root);
    command.args(&args);
    let output = history_output(command, budget)
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

#[cfg(test)]
fn parse_name_log(raw: &str) -> Result<Vec<Commit>> {
    Ok(parse_name_log_with_headers(raw)?.0)
}

fn parse_name_log_with_headers(raw: &str) -> Result<(Vec<Commit>, Vec<Header>)> {
    let mut commits = Vec::new();
    let mut headers = Vec::new();
    let mut current: Option<Commit> = None;
    let mut expect_header = true;
    let mut first_path = false;
    for (offset, record) in nul_records(raw) {
        if record.is_empty() {
            expect_header = true;
            continue;
        }
        if expect_header {
            if let Some(commit) = current.take() {
                commits.push(commit);
            }
            anyhow::ensure!(
                record.starts_with("commit\t"),
                "git history tree-only output has no commit header"
            );
            let mut parsed = parse_log(record)?;
            anyhow::ensure!(
                parsed.len() == 1,
                "invalid git history tree-only commit header"
            );
            current = parsed.pop();
            if let Some(commit) = &current {
                headers.push(Header {
                    sha: commit.sha.clone(),
                    offset,
                });
            }
            expect_header = false;
            first_path = true;
        } else {
            let path = if first_path {
                record.strip_prefix('\n').unwrap_or(record)
            } else {
                record
            };
            first_path = false;
            anyhow::ensure!(
                !path.is_empty(),
                "git history tree-only output has an empty path"
            );
            if let Some(commit) = &mut current {
                commit.changes.push(FileChange {
                    path: path.to_owned(),
                    added: 0,
                    deleted: 0,
                });
            }
        }
    }
    if let Some(commit) = current {
        commits.push(commit);
    }
    Ok((commits, headers))
}

/// Parse `run_git_log` output. A record that is neither a commit header nor a
/// numstat line is an error rather than a skip: text injected ahead of a header
/// hides that commit and attributes its files to the previous one.
fn parse_log(raw: &str) -> Result<Vec<Commit>> {
    Ok(parse_log_with_headers(raw)?.0)
}

/// [`parse_log`] plus every commit header, kept or skipped.
fn parse_log_with_headers(raw: &str) -> Result<(Vec<Commit>, Vec<Header>)> {
    let mut commits = Vec::new();
    let mut headers = Vec::new();
    let mut current: Option<Commit> = None;
    let mut skipped_commits = 0usize;
    let mut skipped_changes = 0usize;
    // Keep a bounded sample of dropped commit SHAs so the warning can name
    // which commits were skipped, not just how many.
    let mut skipped_shas: Vec<String> = Vec::new();

    let mut records = nul_records(raw);
    while let Some((offset, record)) = records.next() {
        // Git inserts newlines before numstat records and between commits. Strip
        // them only at record boundaries, never from the filename field itself.
        let line = record.trim_start_matches('\n');
        if let Some(rest) = line.strip_prefix("commit\t") {
            if let Some(prev) = current.take() {
                commits.push(prev);
            }
            let mut parts = rest.splitn(3, '\t');
            let _sha = parts.next().unwrap_or("");
            headers.push(Header {
                sha: _sha.to_owned(),
                offset,
            });
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
                renames: Vec::new(),
                boundary: false,
            });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        // With -z, ordinary records contain the raw path after two tabs. Rename
        // records leave that field empty and append old\0new\0 instead. Consume
        // both paths even for binary files or commits skipped above.
        let mut parts = line.splitn(3, '\t');
        let added_s = parts.next().unwrap_or("-");
        let deleted_s = parts.next().unwrap_or("-");
        if !is_numstat_count(added_s) {
            let excerpt: String = line.chars().take(120).collect();
            return Err(anyhow!(
                "git log printed a record that is neither a commit header nor a numstat line: {excerpt:?}"
            ));
        }
        let Some(path) = parts.next() else {
            continue;
        };
        let (path, renamed_from) = if path.is_empty() {
            let Some((_, old_path)) = records.next() else {
                break;
            };
            let Some((_, new_path)) = records.next() else {
                break;
            };
            (new_path, Some(old_path))
        } else {
            (path, None)
        };
        let Some(commit) = current.as_mut() else {
            continue;
        };
        if let Some(old_path) = renamed_from
            && !old_path.is_empty()
            && !path.is_empty()
        {
            commit.renames.push((old_path.to_owned(), path.to_owned()));
        }
        if path.is_empty() || added_s == "-" || deleted_s == "-" {
            continue;
        }
        // Malformed counts must not become zero-churn history entries.
        let (Ok(added), Ok(deleted)) = (added_s.parse::<u32>(), deleted_s.parse::<u32>()) else {
            skipped_changes += 1;
            continue;
        };
        commit.changes.push(FileChange {
            path: path.to_owned(),
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
    Ok((commits, headers))
}

/// Numstat's added-lines field: a decimal count, or `-` for a binary file.
fn is_numstat_count(field: &str) -> bool {
    field == "-" || (!field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()))
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
    fn changed_exclusions_rebuild_same_head_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("ignored.rs"), "fn ignored() {}\n").unwrap();
        run(&["add", "ignored.rs"]);
        run(&["commit", "-qm", "initial"]);

        let db = Database::open_in_memory().unwrap();
        git_history_index_with_options(&db, root, &[], IndexMode::Auto).unwrap();
        assert!(db.git_file("ignored.rs").unwrap().is_some());
        assert!(db.git_index_exclusion_fingerprint().unwrap().is_some());

        git_history_index_with_options(&db, root, &["ignored.rs".to_string()], IndexMode::Auto)
            .unwrap();
        assert!(db.git_file("ignored.rs").unwrap().is_none());
        assert_eq!(
            db.git_index_exclusion_fingerprint().unwrap().as_deref(),
            Some(effective_exclusion_fingerprint(&["ignored.rs".to_string()]).as_str())
        );
    }

    #[test]
    fn exclusion_fingerprint_is_order_independent() {
        assert_eq!(
            effective_exclusion_fingerprint(&["b".into(), "a".into(), "b".into()]),
            effective_exclusion_fingerprint(&["a".into(), "b".into()])
        );
        assert_ne!(
            effective_exclusion_fingerprint(&["a\nb".into()]),
            effective_exclusion_fingerprint(&["a".into(), "b".into()])
        );
    }

    fn policy_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for round in 0..3 {
            std::fs::write(dir.path().join("a.rs"), format!("fn a() {{ {round} }}\n")).unwrap();
            std::fs::write(
                dir.path().join("skip.rs"),
                format!("fn s() {{ {round} }}\n"),
            )
            .unwrap();
            if round == 0 {
                policy_git(dir.path(), &["init", "-q"]);
                policy_git(dir.path(), &["config", "user.email", "test@example.com"]);
                policy_git(dir.path(), &["config", "user.name", "Test"]);
            }
            policy_git(dir.path(), &["add", "-A"]);
            policy_git(dir.path(), &["commit", "-qm", &format!("round {round}")]);
        }
        dir
    }

    fn policy_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn skip_rows_present(db: &Database) -> (bool, bool, bool) {
        (
            db.git_file("skip.rs").unwrap().is_some(),
            db.co_change_pair_exists("a.rs", "skip.rs").unwrap(),
            !db.git_author_events("skip.rs").unwrap().is_empty(),
        )
    }

    /// A pre-0025 index (or one last written by a pre-0025 binary, whose state
    /// upsert clears the column) records no exclusion provenance, so its rows
    /// may predate the current policy. Both explicit `--incremental` (what
    /// installed hooks pass) and Auto rebuild once, then compose again.
    #[test]
    fn unattributed_index_rebuilds_under_the_current_policy() {
        for mode in [IndexMode::Incremental, IndexMode::Auto] {
            let dir = policy_repo();
            let root = dir.path();
            let db = Database::open_in_memory().unwrap();
            git_history_index_with_options(&db, root, &[], IndexMode::Full).unwrap();
            assert_eq!(skip_rows_present(&db), (true, true, true));
            let (sha, _) = db.get_git_index_state().unwrap().unwrap();
            db.set_git_index_state(&sha).unwrap();
            assert_eq!(db.git_index_exclusion_fingerprint().unwrap(), None);

            let excludes = ["skip.rs".to_string()];
            let stats = git_history_index_with_options(&db, root, &excludes, mode).unwrap();
            assert_eq!(stats.commits_scanned, 3, "{mode:?} must rescan in full");
            assert_eq!(skip_rows_present(&db), (false, false, false), "{mode:?}");
            assert_eq!(
                db.git_index_exclusion_fingerprint().unwrap().as_deref(),
                Some(effective_exclusion_fingerprint(&excludes).as_str())
            );

            let again = git_history_index_with_options(&db, root, &excludes, mode).unwrap();
            assert_eq!(
                again.commits_scanned, 0,
                "{mode:?} must compose once attributed"
            );
        }
    }

    /// A recorded, different policy rebuilds in every non-full mode, whether
    /// or not HEAD moved since the last pass.
    #[test]
    fn recorded_policy_change_rebuilds_with_and_without_new_commits() {
        for (mode, move_head) in [
            (IndexMode::Incremental, false),
            (IndexMode::Incremental, true),
            (IndexMode::Auto, false),
            (IndexMode::Auto, true),
        ] {
            let dir = policy_repo();
            let root = dir.path();
            let db = Database::open_in_memory().unwrap();
            git_history_index_with_options(&db, root, &[], IndexMode::Full).unwrap();
            assert_eq!(skip_rows_present(&db), (true, true, true));
            if move_head {
                std::fs::write(root.join("a.rs"), "fn a() { 9 }\n").unwrap();
                policy_git(root, &["commit", "-qam", "moves head"]);
            }

            let excludes = ["skip.rs".to_string()];
            git_history_index_with_options(&db, root, &excludes, mode).unwrap();
            assert_eq!(
                skip_rows_present(&db),
                (false, false, false),
                "{mode:?} move_head={move_head}"
            );
            assert!(db.git_file("a.rs").unwrap().is_some());
            assert_eq!(
                db.git_index_exclusion_fingerprint().unwrap().as_deref(),
                Some(effective_exclusion_fingerprint(&excludes).as_str())
            );
        }
    }

    /// An unchanged policy, spelled in another order, keeps composing.
    #[test]
    fn unchanged_policy_is_a_no_op() {
        let dir = policy_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        let excludes = ["skip.rs".to_string(), "other.rs".to_string()];
        git_history_index_with_options(&db, root, &excludes, IndexMode::Full).unwrap();
        let reordered = [
            "other.rs".to_string(),
            "skip.rs".to_string(),
            "skip.rs".to_string(),
        ];
        for mode in [IndexMode::Incremental, IndexMode::Auto] {
            let stats = git_history_index_with_options(&db, root, &reordered, mode).unwrap();
            assert_eq!(stats.commits_scanned, 0, "{mode:?}");
        }
        assert!(db.git_file("a.rs").unwrap().is_some());
    }

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
    fn parse_log_handles_basic_format() {
        let raw = "commit\tabc\t1700000000\tfix: x\0\n10\t2\tsrc/a.rs\x005\t1\tsrc/b.rs\0\
                   \0commit\tdef\t1700001000\tfeat: y\0\n3\t0\tsrc/c.rs\0-\t-\tbinary.bin\0";
        let commits = parse_log(raw).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "fix: x");
        assert_eq!(commits[0].changes.len(), 2);
        assert_eq!(commits[1].changes.len(), 1, "binary file skipped");
        assert_eq!(commits[1].changes[0].path, "src/c.rs");
    }

    #[test]
    fn tree_only_paths_preserve_tabs_newlines_and_header_shaped_names() {
        let raw = "\0commit\tabc\t1700000000\ta@b\u{1f}A\u{1f}change\0\n\nfirst.rs\0commit\tfake\t123\0tab\tpath.rs\0\0\0commit\tdef\t1700000001\ta@b\u{1f}A\u{1f}next\0\nlast.rs\0";
        let commits = parse_name_log(raw).unwrap();
        assert_eq!(commits.len(), 2);
        let paths: Vec<_> = commits[0]
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(paths, ["\nfirst.rs", "commit\tfake\t123", "tab\tpath.rs"]);
        assert_eq!(commits[1].changes[0].path, "last.rs");
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

        let anchor = anchor_at_commit(empty.path(), "HEAD").unwrap();
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
        assert_eq!(commit_epoch(dir.path(), "-O/etc/passwd").unwrap(), None);
    }

    #[test]
    fn commit_epoch_output_that_is_not_an_epoch_is_an_error() {
        assert_eq!(parse_commit_epoch(b"1790424195\n").unwrap(), 1_790_424_195);
        let err = parse_commit_epoch(b"No signature\n1790424195\n").unwrap_err();
        assert!(err.to_string().contains("No signature"), "{err}");
        assert!(parse_commit_epoch(b"").is_err());
    }

    #[test]
    fn parse_log_rejects_text_injected_ahead_of_a_commit_header() {
        // `log.showSignature` output shape: the second commit's header rides
        // behind verification text, so a lenient parser would fold c.rs into
        // the first commit.
        let raw = "commit\tabc\t1700000000\tfeat: x\0\n1\t0\ta.rs\0\0\
                   No signature\ncommit\tdef\t1700001000\tfeat: y\0\n2\t0\tc.rs\0";
        let err = parse_log(raw).unwrap_err();
        assert!(err.to_string().contains("No signature"), "{err}");
    }

    #[test]
    fn parse_log_records_renames_including_binary_ones() {
        let raw = "commit\tabc\t1700000000\tfeat: x\0\n0\t0\t\0old.rs\0new.rs\0\
                   -\t-\t\0logo.png\0img/logo.png\x003\t1\tother.rs\0";
        let commits = parse_log(raw).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].renames,
            vec![
                ("old.rs".to_string(), "new.rs".to_string()),
                ("logo.png".to_string(), "img/logo.png".to_string()),
            ]
        );
        let paths: Vec<&str> = commits[0].changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["new.rs", "other.rs"], "binary change skipped");
    }

    /// Newest-first commit carrying `changes` (paths) and `renames`.
    fn commit_with(changes: &[&str], renames: &[(&str, &str)]) -> Commit {
        Commit {
            subject: "feat: x".into(),
            changes: changes.iter().map(|p| make_change(p)).collect(),
            renames: renames
                .iter()
                .map(|(old, new)| (old.to_string(), new.to_string()))
                .collect(),
            ..Commit::default()
        }
    }

    fn paths_of(commit: &Commit) -> Vec<&str> {
        commit.changes.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn follow_renames_keys_older_commits_to_the_newest_name() {
        let none = GlobSet::empty();
        // Newest first: b -> c, then a -> b, then edits under a.
        let mut commits = vec![
            commit_with(&["c.rs"], &[("b.rs", "c.rs")]),
            commit_with(&["b.rs", "peer.rs"], &[("a.rs", "b.rs")]),
            commit_with(&["a.rs", "peer.rs"], &[]),
        ];
        let successors = follow_renames(&mut commits, &none);
        assert_eq!(paths_of(&commits[1]), ["c.rs", "peer.rs"]);
        assert_eq!(paths_of(&commits[2]), ["c.rs", "peer.rs"]);
        assert_eq!(successors.get("a.rs").map(String::as_str), Some("c.rs"));
        assert_eq!(successors.get("b.rs").map(String::as_str), Some("c.rs"));
        assert!(!successors.contains_key("c.rs"));
    }

    #[test]
    fn follow_renames_applies_only_to_commits_older_than_the_rename() {
        let none = GlobSet::empty();
        // A new a.rs created after a.rs moved to c.rs is a different file.
        let mut commits = vec![
            commit_with(&["a.rs"], &[]),
            commit_with(&["c.rs"], &[("a.rs", "c.rs")]),
            commit_with(&["a.rs"], &[]),
        ];
        follow_renames(&mut commits, &none);
        assert_eq!(paths_of(&commits[0]), ["a.rs"]);
        assert_eq!(paths_of(&commits[2]), ["c.rs"]);
    }

    #[test]
    fn follow_renames_rename_back_leaves_no_alias() {
        let none = GlobSet::empty();
        let mut commits = vec![
            commit_with(&["a.rs"], &[("b.rs", "a.rs")]),
            commit_with(&["b.rs"], &[("a.rs", "b.rs")]),
            commit_with(&["a.rs"], &[]),
        ];
        let successors = follow_renames(&mut commits, &none);
        assert!(commits.iter().all(|c| paths_of(c) == ["a.rs"]));
        assert_eq!(successors.len(), 1);
        assert_eq!(successors.get("b.rs").map(String::as_str), Some("a.rs"));
    }

    #[test]
    fn follow_renames_resolves_a_swap_against_the_state_before_the_commit() {
        let none = GlobSet::empty();
        let mut commits = vec![
            commit_with(&["a.rs", "b.rs"], &[("a.rs", "b.rs"), ("b.rs", "a.rs")]),
            commit_with(&["a.rs"], &[]),
            commit_with(&["b.rs"], &[]),
        ];
        follow_renames(&mut commits, &none);
        assert_eq!(paths_of(&commits[1]), ["b.rs"]);
        assert_eq!(paths_of(&commits[2]), ["a.rs"]);
    }

    #[test]
    fn follow_renames_merges_paths_folded_onto_one_name_and_drops_excluded_origins() {
        let vendor = build_exclude_set(&["vendor/**".to_string()]).unwrap();
        let mut commits = vec![
            commit_with(&["c.rs"], &[("a.rs", "c.rs"), ("vendor/v.rs", "src/v.rs")]),
            commit_with(&["a.rs", "c.rs", "vendor/v.rs"], &[]),
        ];
        follow_renames(&mut commits, &vendor);
        assert_eq!(paths_of(&commits[1]), ["c.rs"], "one change per path");
        assert_eq!(commits[1].changes[0].added, 10);
        assert_eq!(commits[1].changes[0].deleted, 2);
    }

    #[test]
    fn numstat_count_field_accepts_digits_and_binary_marker_only() {
        assert!(is_numstat_count("0"));
        assert!(is_numstat_count("123"));
        assert!(is_numstat_count("-"));
        assert!(!is_numstat_count(""));
        assert!(!is_numstat_count("-1"));
        assert!(!is_numstat_count("gpg: Good signature"));
    }

    fn commit_all(root: &Path, files: &[(&str, &str)], subject: &str) {
        for (path, body) in files {
            std::fs::write(root.join(path), body).unwrap();
        }
        policy_git(root, &["add", "-A"]);
        policy_git(root, &["commit", "-qm", subject]);
    }

    /// `main` and `side` share two commits; HEAD is left on `side`. `main`
    /// then gains commits `side` lacks, including a rename and a fix.
    fn diverging_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        policy_git(root, &["init", "-q", "-b", "main"]);
        policy_git(root, &["config", "user.email", "test@example.com"]);
        policy_git(root, &["config", "user.name", "Test"]);
        commit_all(root, &[("a.rs", "a0\n"), ("b.rs", "b0\n")], "base one");
        commit_all(root, &[("a.rs", "a1\n"), ("b.rs", "b1\n")], "base two");
        policy_git(root, &["checkout", "-qb", "side"]);
        commit_all(root, &[("side.rs", "s0\n"), ("a.rs", "s-a\n")], "side work");
        dir
    }

    fn advance_main(root: &Path) {
        policy_git(root, &["checkout", "-q", "main"]);
        for round in 2..6 {
            commit_all(
                root,
                &[
                    ("a.rs", &format!("a{round}\n")),
                    ("b.rs", &format!("b{round}\n")),
                ],
                &format!("main {round}"),
            );
        }
        policy_git(root, &["mv", "b.rs", "c.rs"]);
        commit_all(root, &[("a.rs", "a6\n")], "fix: rename b");
        commit_all(root, &[("c.rs", "c7\nmore\n"), ("a.rs", "a7\n")], "main 7");
    }

    fn head(root: &Path) -> String {
        resolve_head_sha(root).unwrap()
    }

    type FileRow = (String, u32, u32, Option<i64>, i64);
    type PairRow = (String, String, u32, u64, Option<i64>, Option<i64>, i64);
    type AuthorRow = (String, String, i64);
    type HistoryRows = (Vec<FileRow>, Vec<PairRow>, Vec<AuthorRow>);

    /// Every stored history row, floats rounded so composed decay compares.
    fn history_rows(db: &Database) -> HistoryRows {
        let mut paths: Vec<String> = db.churn_percentiles().unwrap().into_keys().collect();
        paths.sort();
        let mut files = Vec::new();
        let mut pairs = Vec::new();
        let mut authors = Vec::new();
        for path in &paths {
            let row = db.git_file(path).unwrap().unwrap();
            files.push((
                path.clone(),
                row.fix_count,
                row.total_commits,
                row.last_commit_at,
                (row.churn_score * 1e6).round() as i64,
            ));
            for pair in db.co_changes_for(path, 1000).unwrap() {
                pairs.push((
                    path.clone(),
                    pair.file,
                    pair.count,
                    pair.window_mask,
                    pair.first_observed_at,
                    pair.last_observed_at,
                    (pair.weight * 1e6).round() as i64,
                ));
            }
            for (author, at) in db.git_author_events(path).unwrap() {
                authors.push((path.clone(), author, at));
            }
        }
        authors.sort();
        (files, pairs, authors)
    }

    fn sliced(first_slice: usize, max_slices: Option<usize>) -> WalkLimits {
        WalkLimits {
            budget: Duration::from_secs(60),
            first_slice,
            max_slices,
            forced_deadline: None,
            may_stage: true,
            may_preempt: true,
            max_recheck_allowance: MAX_RECHECK_ALLOWANCE,
            environment_delay: Duration::ZERO,
            after_slice: None,
            before_recheck: None,
            before_plan_recheck: None,
        }
    }

    fn forcing(limits: WalkLimits, forced: fn(HistoryWalk) -> bool) -> WalkLimits {
        WalkLimits {
            forced_deadline: Some(forced),
            ..limits
        }
    }

    fn assert_finish_command_is_scoped(message: &str) {
        assert!(
            message.contains(
                "`CODESAGE_GIT_HISTORY_TIMEOUT_MS=600000 codesage git-index --full` finishes in \
                 one run when the remaining slices fit in that budget"
            ),
            "{message}"
        );
    }

    #[test]
    fn diverged_stored_head_rebuilds_across_bounded_runs_without_exposing_partial_rows() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        let before = history_rows(&db);
        assert!(db.git_file("side.rs").unwrap().is_some());
        advance_main(root);
        assert!(!is_ancestor(root, &stored, &head(root)).unwrap());

        let mut runs = 0;
        let mut last_staged = 0;
        loop {
            runs += 1;
            assert!(runs <= 20, "the rebuild never completed");
            match index_history(&db, root, &[], IndexMode::Incremental, sliced(2, Some(1))) {
                Ok(_) => break,
                Err(error) => {
                    let message = format!("{error:#}");
                    assert!(
                        message.contains(&format!(
                            "stored git history head {} is not an ancestor of HEAD",
                            short_sha(&stored)
                        )),
                        "{message}"
                    );
                    assert!(
                        message.contains("git history rebuild in progress"),
                        "{message}"
                    );
                    assert_finish_command_is_scoped(&message);
                    let staged = db.git_history_build().unwrap().unwrap().staged;
                    assert!(staged > last_staged, "run {runs} saved nothing: {message}");
                    last_staged = staged;
                    assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
                    assert_eq!(history_rows(&db), before, "partial rows became visible");
                    if runs == 1 {
                        // HEAD moves on mid-rebuild, as a post-commit hook sees it.
                        commit_all(root, &[("a.rs", "a8\n"), ("d.rs", "d8\n")], "main 8");
                    }
                }
            }
        }
        assert!(
            runs >= 2,
            "a two-commit first slice must not finish in one run"
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        assert!(db.git_history_build().unwrap().is_none());
        assert!(db.git_file("side.rs").unwrap().is_none());

        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn staged_rebuild_assembles_exactly_the_single_walk() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let source = HistorySource::read(root).unwrap();
        let anchor = anchor_at_commit(root, &head(root)).unwrap();
        let walk = history_commits(
            root,
            None,
            anchor.cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        let project = |commits: &[Commit]| {
            commits
                .iter()
                .map(|commit| {
                    let changes: Vec<_> = commit
                        .changes
                        .iter()
                        .map(|change| (change.path.clone(), change.added, change.deleted))
                        .collect();
                    (
                        commit.sha.clone(),
                        commit.timestamp,
                        commit.author.clone(),
                        commit.subject.clone(),
                        changes,
                        commit.renames.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(walk.iter().any(|commit| !commit.renames.is_empty()));

        let db = Database::open_in_memory().unwrap();
        let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
        let anchors = PassAnchors {
            current: anchor,
            previous: anchor.epoch,
            exclusion_fingerprint: &fingerprint,
            source: &source,
            limits: sliced(1, Some(1)),
        };
        let head_sha = head(root);
        let mut runs = 0;
        let staged = loop {
            runs += 1;
            assert!(runs <= 4 * walk.len(), "staging never completed");
            match stage_history(&db, root, &head_sha, &anchors, FullReason::Rebuild).unwrap() {
                Staged::Complete { target, commits } => {
                    assert_eq!(target, head_sha);
                    break commits;
                }
                Staged::Incomplete(progress) => assert!(progress.stalled.is_none()),
            }
        };
        assert!(runs > 2);
        assert_eq!(project(&staged), project(&walk));
    }

    type CommitView = (
        String,
        i64,
        String,
        String,
        Vec<(String, u32, u32)>,
        Vec<(String, String)>,
        bool,
    );

    fn commit_view(commits: &[Commit]) -> Vec<CommitView> {
        commits
            .iter()
            .map(|commit| {
                (
                    commit.sha.clone(),
                    commit.timestamp,
                    commit.author.clone(),
                    commit.subject.clone(),
                    commit
                        .changes
                        .iter()
                        .map(|change| (change.path.clone(), change.added, change.deleted))
                        .collect(),
                    commit.renames.clone(),
                    commit.boundary,
                )
            })
            .collect()
    }

    #[test]
    fn retargeted_rebuild_reuses_the_commits_both_targets_list() {
        for tree_only in [false, true] {
            let dir = diverging_repo();
            let root = dir.path();
            advance_main(root);
            let main_head = head(root);
            // `other` forks from main below its newest commit, so main's head
            // is not an ancestor of it, and shares the rename commit.
            policy_git(root, &["checkout", "-qb", "other", "HEAD~1"]);
            commit_all(root, &[("o.rs", "o\n")], "other work");
            let other_head = head(root);
            assert!(!is_ancestor(root, &main_head, &other_head).unwrap());

            let source = HistorySource {
                tree_only,
                shallow: BTreeSet::new(),
            };
            let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
            let db = Database::open_in_memory().unwrap();
            let stage = |sha: &str, limits: WalkLimits| {
                let current = anchor_at_commit(root, sha).unwrap();
                let anchors = PassAnchors {
                    current,
                    previous: current.epoch,
                    exclusion_fingerprint: &fingerprint,
                    source: &source,
                    limits,
                };
                stage_history(&db, root, sha, &anchors, FullReason::Rebuild).unwrap()
            };

            // A one-commit first slice, then a four-commit one: main 7, then
            // the rename through main 3.
            for expected in [1, 5] {
                match stage(&main_head, sliced(1, Some(1))) {
                    Staged::Incomplete(progress) => {
                        assert_eq!((progress.staged, progress.total), (expected, 8));
                    }
                    Staged::Complete { .. } => panic!("main staged in full"),
                }
            }
            match stage(&other_head, sliced(1, Some(0))) {
                Staged::Incomplete(progress) => {
                    assert_eq!(progress.target, other_head);
                    assert_eq!(
                        (progress.staged, progress.total),
                        (4, 8),
                        "tree_only={tree_only}: the commits main and other share were read again"
                    );
                    assert!(progress.stalled.is_none());
                }
                Staged::Complete { .. } => panic!("other staged without reading its own commit"),
            }
            let saved = db.git_history_build_staged_shas().unwrap();
            assert_eq!(saved.len(), 4);
            assert!(
                !saved.contains(&main_head),
                "a commit the new plan does not list stays staged"
            );

            let staged = match stage(&other_head, sliced(1, None)) {
                Staged::Complete { target, commits } => {
                    assert_eq!(target, other_head);
                    commits
                }
                Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
            };
            let walk = history_commits(
                root,
                None,
                anchor_at_commit(root, &other_head).unwrap().cutoff(),
                &source,
                Duration::from_secs(60),
            )
            .unwrap();
            assert!(tree_only || walk.iter().any(|commit| !commit.renames.is_empty()));
            assert_eq!(
                commit_view(&staged),
                commit_view(&walk),
                "tree_only={tree_only}"
            );
        }
    }

    #[test]
    fn branch_switches_between_runs_keep_the_staged_rebuild() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let main_head = head(root);
        let run = || index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)));
        let message = format!("{:#}", run().unwrap_err());
        assert!(message.contains("1 of 8 commits saved"), "{message}");

        // Back on the stored head, then one commit along its line: neither
        // pass needs the rebuild, and neither discards it.
        policy_git(root, &["checkout", "-q", "side"]);
        run().unwrap();
        let status = db.git_history_build_status().unwrap().unwrap();
        assert_eq!(
            (
                status.staged,
                status.outcome.as_str(),
                status.target_sha.as_str()
            ),
            (1, OUTCOME_PARKED, main_head.as_str())
        );
        commit_all(root, &[("side.rs", "s1\n")], "side more");
        run().unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        assert_eq!(db.git_history_build().unwrap().unwrap().staged, 1);

        policy_git(root, &["checkout", "-q", "main"]);
        let message = format!("{:#}", run().unwrap_err());
        assert!(message.contains("5 of 8 commits saved"), "{message}");
        let status = db.git_history_build_status().unwrap().unwrap();
        assert_eq!(
            (status.outcome.as_str(), status.cause.as_str()),
            (OUTCOME_PROGRESSING, CAUSE_DIVERGED)
        );
        let mut runs = 0;
        while let Err(error) = run() {
            runs += 1;
            assert!(runs <= 4, "the rebuild never completed: {error:#}");
        }
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, main_head);
        assert!(db.git_history_build().unwrap().is_none());
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn a_rebuild_kept_for_another_branch_does_not_preempt_an_incremental_pass() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let run = || index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)));
        assert!(run().is_err());
        // HEAD returns to the stored head's line and merges main, so both the
        // stored head and the staged target are ancestors of HEAD.
        policy_git(root, &["checkout", "-q", "side"]);
        policy_git(root, &["merge", "-q", "--no-edit", "-X", "ours", "main"]);
        run().unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        assert_eq!(
            db.git_history_build_status().unwrap().unwrap().outcome,
            OUTCOME_PARKED
        );
    }

    #[test]
    fn rebuild_status_tells_progress_from_a_run_that_saved_nothing() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        advance_main(root);
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, sliced(4, Some(1))).unwrap_err()
        );
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(
            (
                rebuild.state.as_str(),
                rebuild.cause.as_str(),
                rebuild.stored_sha.as_deref(),
                rebuild.target_sha.as_str()
            ),
            (
                OUTCOME_PROGRESSING,
                CAUSE_DIVERGED,
                Some(stored.as_str()),
                head(root).as_str()
            )
        );
        assert_eq!(
            (
                rebuild.commits_saved,
                rebuild.commits_total,
                rebuild.last_run_saved
            ),
            (4, 8, 4)
        );
        assert!(
            message.contains(&format!(
                "next slice {} commits",
                rebuild.next_slice_commits
            )),
            "{message}"
        );
        assert!(rebuild.stalled.is_none());

        let slices_time_out = forcing(sliced(4, None), |walk| {
            matches!(walk, HistoryWalk::Slice(_))
        });
        let mut runs = 0;
        let stalled = loop {
            runs += 1;
            assert!(runs <= 6, "the slice never shrank to one commit");
            assert!(index_history(&db, root, &[], IndexMode::Auto, slices_time_out).is_err());
            let rebuild = history_rebuild(&db).unwrap().unwrap();
            assert_eq!(rebuild.commits_saved, 4);
            assert_eq!(rebuild.last_run_saved, 0);
            if rebuild.state == OUTCOME_NO_PROGRESS {
                break rebuild;
            }
            assert_eq!(rebuild.state, OUTCOME_PROGRESSING);
        };
        assert!(runs >= 2, "halving the slice is progress");
        assert_eq!(stalled.next_slice_commits, 1);
        assert!(
            stalled
                .stalled
                .as_deref()
                .is_some_and(|why| why.contains("alone takes longer than the budget")),
            "{stalled:?}"
        );
    }

    fn rev(root: &Path, spec: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", spec])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "rev-parse {spec}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Main and two branches off its tip: `attrs` marks `*.foo` `-diff`,
    /// `plain` does not. Every shared commit changes `data.foo`.
    fn attributes_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        policy_git(root, &["init", "-q", "-b", "main"]);
        policy_git(root, &["config", "user.email", "test@example.com"]);
        policy_git(root, &["config", "user.name", "Test"]);
        for round in 0..3 {
            commit_all(
                root,
                &[
                    ("data.foo", &format!("d{round}\n")),
                    ("a.rs", &format!("a{round}\n")),
                ],
                &format!("base {round}"),
            );
        }
        policy_git(root, &["checkout", "-qb", "plain"]);
        commit_all(root, &[("a.rs", "plain\n")], "plain work");
        policy_git(root, &["checkout", "-qb", "attrs", "main"]);
        commit_all(
            root,
            &[(".gitattributes", "*.foo -diff\n"), ("a.rs", "attrs\n")],
            "attrs work",
        );
        dir
    }

    fn fingerprint_now(root: &Path) -> String {
        let budget = Duration::from_secs(60);
        numstat_environment_fingerprint(root, Instant::now() + budget, budget).unwrap()
    }

    fn foo_counted(commits: &[Commit]) -> bool {
        commits
            .iter()
            .flat_map(|commit| &commit.changes)
            .any(|change| change.path == "data.foo")
    }

    #[test]
    fn staged_output_is_not_reused_under_other_attributes() {
        let dir = attributes_repo();
        let root = dir.path();
        let source = HistorySource {
            tree_only: false,
            shallow: BTreeSet::new(),
        };
        let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
        let db = Database::open_in_memory().unwrap();
        let stage = |sha: &str, limits: WalkLimits| {
            let current = anchor_at_commit(root, sha).unwrap();
            let anchors = PassAnchors {
                current,
                previous: current.epoch,
                exclusion_fingerprint: &fingerprint,
                source: &source,
                limits,
            };
            stage_history(&db, root, sha, &anchors, FullReason::Rebuild).unwrap()
        };
        let walk = |sha: &str| {
            history_commits(
                root,
                None,
                anchor_at_commit(root, sha).unwrap().cutoff(),
                &source,
                Duration::from_secs(60),
            )
            .unwrap()
        };
        let finish = |sha: &str| match stage(sha, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };

        // Stage every shared commit under `-diff`, then retarget to `plain`.
        let attrs_head = head(root);
        for _ in 0..2 {
            stage(&attrs_head, sliced(1, Some(1)));
        }
        assert_eq!(db.git_history_build_staged_shas().unwrap().len(), 4);
        assert!(!foo_counted(&walk(&attrs_head)), "-diff hides the counts");
        policy_git(root, &["checkout", "-q", "plain"]);
        let plain_head = head(root);
        let single = walk(&plain_head);
        assert!(foo_counted(&single));
        assert_eq!(commit_view(&finish(&plain_head)), commit_view(&single));

        // Back on `attrs`, every commit keeps the output saved under `-diff`:
        // the `plain` plan dropped only output saved under its own attributes.
        policy_git(root, &["checkout", "-q", "attrs"]);
        match stage(&attrs_head, sliced(1, Some(0))) {
            Staged::Complete { commits, .. } => {
                assert_eq!(commit_view(&commits), commit_view(&walk(&attrs_head)));
            }
            Staged::Incomplete(progress) => panic!("saved output was dropped: {progress:?}"),
        }
        // Resuming the same target after the attributes change re-reads.
        // Git falls back to the index copy of a deleted file, so empty it.
        std::fs::write(root.join(".gitattributes"), "").unwrap();
        let single = walk(&attrs_head);
        assert!(foo_counted(&single));
        assert_eq!(commit_view(&finish(&attrs_head)), commit_view(&single));
    }

    #[test]
    fn attributes_fingerprint_covers_every_attribute_file_git_reads() {
        let dir = attributes_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        commit_all(root, &[("sub/lib.rs", "lib\n")], "a tracked directory");
        let mut seen = vec![fingerprint_now(root)];
        // A directory with no tracked path is not checked; it is documented
        // as not fingerprinted.
        std::fs::create_dir_all(root.join("untracked")).unwrap();
        std::fs::write(root.join("untracked/.gitattributes"), "*.rs -diff\n").unwrap();
        assert_eq!(
            fingerprint_now(root),
            seen[0],
            "a directory without tracked paths was read"
        );
        let mut changed = |what: &str| {
            let next = fingerprint_now(root);
            assert!(
                !seen.contains(&next),
                "{what} left the fingerprint unchanged"
            );
            seen.push(next);
        };
        std::fs::write(root.join("sub/.gitattributes"), "*.rs -diff\n").unwrap();
        changed("an untracked .gitattributes in a directory with a tracked path");
        policy_git(root, &["add", "sub/.gitattributes"]);
        changed("tracking that .gitattributes");
        std::fs::write(root.join("sub/.gitattributes"), "*.md -diff\n").unwrap();
        changed("the worktree content of a tracked .gitattributes");
        policy_git(root, &["checkout", "-q", "plain"]);
        changed("checking out a branch without the root .gitattributes");
        std::fs::write(root.join(".gitattributes"), "*.foo -diff\n").unwrap();
        changed("an untracked root .gitattributes");
        std::fs::remove_file(root.join(".gitattributes")).unwrap();
        policy_git(root, &["checkout", "-q", "attrs"]);
        std::fs::write(root.join(".git/info/attributes"), "*.rs -diff\n").unwrap();
        changed("info/attributes");
        let global = root.join("global-attributes");
        std::fs::write(&global, "*.txt -diff\n").unwrap();
        policy_git(
            root,
            &["config", "core.attributesFile", global.to_str().unwrap()],
        );
        changed("core.attributesFile");
        std::fs::write(&global, "*.md -diff\n").unwrap();
        changed("the core.attributesFile content");
        let base = rev(root, "main");
        std::fs::write(root.join(".git/info/grafts"), format!("{base}\n")).unwrap();
        changed("info/grafts");
        policy_git(root, &["config", "core.useReplaceRefs", "false"]);
        changed("core.useReplaceRefs");
        policy_git(root, &["config", "branch.main.remote", "origin"]);
        assert_eq!(
            fingerprint_now(root),
            seen[seen.len() - 1],
            "branch tracking config is not numstat input"
        );
    }

    #[test]
    fn a_graft_added_mid_rebuild_rereads_saved_commits() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        // `main 5` now sits directly on the root commit, so its diff and the
        // commits a walk lists both change.
        let grafted = rev(root, "main~2");
        let root_commit = rev(root, "main~7");
        policy_git(root, &["replace", "--graft", &grafted, &root_commit]);
        let source = HistorySource::read(root).unwrap();
        let walk = history_commits(
            root,
            None,
            anchor_at_commit(root, &head(root)).unwrap().cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        let staged = match stage_main(&db, root, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&walk));
    }

    #[test]
    fn attr_tree_head_keeps_saved_commits_until_the_attributes_change() {
        let dir = diverging_repo();
        let root = dir.path();
        policy_git(root, &["config", "attr.tree", "HEAD"]);
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        for _ in 0..2 {
            stage_main(&db, root, sliced(1, Some(1)));
        }
        commit_all(root, &[("unrelated.txt", "u\n")], "unrelated");
        assert_eq!(
            kept_after_a_resume(&db, root),
            5,
            "a commit that leaves .gitattributes alone discarded saved output"
        );
        commit_all(root, &[(".gitattributes", "*.rs -diff\n")], "attributes");
        assert_eq!(kept_after_a_resume(&db, root), 0);
    }

    #[test]
    fn a_config_change_rereads_saved_output() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let source = HistorySource {
            tree_only: false,
            shallow: BTreeSet::new(),
        };
        let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
        let db = Database::open_in_memory().unwrap();
        let head_sha = head(root);
        let stage = |limits: WalkLimits| {
            let current = anchor_at_commit(root, &head_sha).unwrap();
            let anchors = PassAnchors {
                current,
                previous: current.epoch,
                exclusion_fingerprint: &fingerprint,
                source: &source,
                limits,
            };
            stage_history(&db, root, &head_sha, &anchors, FullReason::Rebuild).unwrap()
        };
        for _ in 0..2 {
            assert!(matches!(stage(sliced(1, Some(1))), Staged::Incomplete(_)));
        }
        assert_eq!(db.git_history_build_staged_shas().unwrap().len(), 5);

        // Every file now exceeds the threshold, so numstat prints `-` counts.
        policy_git(root, &["config", "core.bigFileThreshold", "1"]);
        let walk = history_commits(
            root,
            None,
            anchor_at_commit(root, &head_sha).unwrap().cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(
            walk.iter().all(|commit| commit.changes.is_empty()),
            "the threshold must hide every count"
        );
        let staged = match stage(sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&walk));
    }

    #[test]
    fn a_non_utf8_attributes_path_is_fingerprinted() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let dir = attributes_repo();
            let root = dir.path();
            let odd = root.join(std::ffi::OsStr::from_bytes(b"odd-\xff-dir"));
            if let Err(error) = std::fs::create_dir(&odd) {
                eprintln!("skipped: the filesystem rejects a non-UTF-8 name: {error}");
                return;
            }
            std::fs::write(odd.join(".gitattributes"), "*.rs -diff\n").unwrap();
            policy_git(root, &["add", "-A"]);
            let first = fingerprint_now(root);
            std::fs::write(odd.join(".gitattributes"), "*.md -diff\n").unwrap();
            assert_ne!(
                fingerprint_now(root),
                first,
                "the file under the non-UTF-8 directory was not read"
            );
        }
        #[cfg(not(unix))]
        eprintln!("skipped: non-UTF-8 paths are a Unix filesystem property");
    }

    #[test]
    fn passes_beside_a_rebuild_parked_for_another_branch_read_no_numstat_environment() {
        let fingerprints = || NUMSTAT_FINGERPRINTS.with(std::cell::Cell::get);
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let run = || index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)));
        assert!(run().is_err());
        policy_git(root, &["checkout", "-q", "side"]);

        let before = fingerprints();
        run().unwrap();
        commit_all(root, &[("side.rs", "s1\n")], "side more");
        run().unwrap();
        assert_eq!(
            fingerprints(),
            before,
            "a same-HEAD or incremental pass fingerprinted for a diverged rebuild"
        );
        assert_eq!(history_rebuild(&db).unwrap().unwrap().state, OUTCOME_PARKED);
    }

    /// A rebuild of `main` with five of its eight commits saved, and a
    /// function that runs one resume that reads no slice and returns how many
    /// saved commits it kept.
    fn half_staged_main() -> (tempfile::TempDir, Database) {
        let dir = diverging_repo();
        advance_main(dir.path());
        let db = Database::open_in_memory().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                stage_main(&db, dir.path(), sliced(1, Some(1))),
                Staged::Incomplete(_)
            ));
        }
        assert_eq!(db.git_history_build_staged_shas().unwrap().len(), 5);
        (dir, db)
    }

    fn stage_main(db: &Database, root: &Path, limits: WalkLimits) -> Staged {
        try_stage_main(db, root, limits).unwrap()
    }

    fn try_stage_main(db: &Database, root: &Path, limits: WalkLimits) -> Result<Staged> {
        let source = HistorySource::read(root).unwrap();
        let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
        let head_sha = head(root);
        let current = anchor_at_commit(root, &head_sha).unwrap();
        let anchors = PassAnchors {
            current,
            previous: current.epoch,
            exclusion_fingerprint: &fingerprint,
            source: &source,
            limits,
        };
        stage_history(db, root, &head_sha, &anchors, FullReason::Rebuild)
    }

    fn kept_after_a_resume(db: &Database, root: &Path) -> usize {
        match stage_main(db, root, sliced(1, Some(0))) {
            Staged::Incomplete(progress) => progress.staged,
            Staged::Complete { .. } => panic!("a run that reads no slice completed"),
        }
    }

    #[test]
    fn branch_tracking_config_keeps_saved_commits() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        // What `git switch <remote branch>`, `push -u`, and editors write.
        for (key, value) in [
            ("branch.main.remote", "origin"),
            ("branch.main.merge", "refs/heads/main"),
            ("branch.main.vscode-merge-base", "origin/main"),
            ("remote.origin.url", "https://example.invalid/repo.git"),
        ] {
            policy_git(root, &["config", key, value]);
        }
        assert_eq!(kept_after_a_resume(&db, root), 5);
    }

    #[test]
    fn a_diff_algorithm_change_rereads_saved_commits() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        policy_git(root, &["config", "diff.algorithm", "patience"]);
        assert_eq!(kept_after_a_resume(&db, root), 0);
    }

    fn git_stdin(root: &Path, args: &[&str], input: &str) -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Point `refs/heads/attrsrc` at a root commit whose only file is a
    /// `.gitattributes` holding `body`.
    fn set_attribute_source(root: &Path, body: &str) {
        let blob = git_stdin(root, &["hash-object", "-w", "--stdin"], body);
        let tree = git_stdin(
            root,
            &["mktree"],
            &format!("100644 blob {blob}\t.gitattributes\n"),
        );
        let commit = git_stdin(root, &["commit-tree", &tree], "attributes\n");
        policy_git(root, &["update-ref", "refs/heads/attrsrc", &commit]);
    }

    #[test]
    fn a_change_to_the_attr_tree_branch_rereads_saved_commits() {
        let dir = diverging_repo();
        let root = dir.path();
        set_attribute_source(root, "*.rs -diff\n");
        policy_git(root, &["config", "attr.tree", "attrsrc"]);
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        for _ in 0..2 {
            stage_main(&db, root, sliced(1, Some(1)));
        }
        assert_eq!(kept_after_a_resume(&db, root), 5, "nothing changed yet");
        // The configured value is still `attrsrc`; the tree it names is not.
        set_attribute_source(root, "*.md -diff\n");
        assert_eq!(kept_after_a_resume(&db, root), 0);
    }

    #[test]
    fn a_rebuild_that_saves_nothing_does_not_hold_history_at_the_stored_head() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let slices_time_out = forcing(sliced(4, None), |walk| {
            matches!(walk, HistoryWalk::Slice(_))
        });
        let mut runs = 0;
        while history_rebuild(&db)
            .unwrap()
            .is_none_or(|rebuild| rebuild.state != OUTCOME_NO_PROGRESS)
        {
            runs += 1;
            assert!(runs <= 6, "the slice never shrank to one commit");
            assert!(index_history(&db, root, &[], IndexMode::Full, slices_time_out).is_err());
        }
        assert_eq!(history_rebuild(&db).unwrap().unwrap().cause, CAUSE_FULL);

        // The same-HEAD pass and an incremental pass still run, under the
        // same budget, and the stalled rebuild stays reported.
        index_history(&db, root, &[], IndexMode::Auto, slices_time_out).unwrap();
        commit_all(root, &[("side.rs", "s1\n")], "side more");
        index_history(&db, root, &[], IndexMode::Auto, slices_time_out).unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_NO_PROGRESS, "{rebuild:?}");

        // An explicit `--full` resumes it.
        index_history(&db, root, &[], IndexMode::Full, sliced(4, None)).unwrap();
        assert!(db.git_history_build().unwrap().is_none());
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
    }

    #[test]
    fn a_resumed_run_fingerprints_the_numstat_environment_once_before_and_once_after_reading() {
        let fingerprints = || NUMSTAT_FINGERPRINTS.with(std::cell::Cell::get);
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert!(index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).is_err());
        let before = fingerprints();
        // The same-HEAD pass is preempted, and the resumed rebuild finishes.
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).unwrap();
        assert!(db.git_history_build().unwrap().is_none());
        assert_eq!(fingerprints() - before, 2);
    }

    #[test]
    fn a_failure_before_reaching_a_saved_rebuild_keeps_its_cause() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        advance_main(root);
        let main_head = head(root);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());

        policy_git(root, &["checkout", "-qb", "other", "HEAD~1"]);
        commit_all(root, &[("o.rs", "o\n")], "other work");
        let environment_times_out =
            forcing(sliced(1, Some(1)), |walk| walk == HistoryWalk::Environment);
        assert!(index_history(&db, root, &[], IndexMode::Full, environment_times_out).is_err());
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.target_sha, main_head);
        assert_eq!(
            (rebuild.cause.as_str(), rebuild.stored_sha.as_deref()),
            (CAUSE_DIVERGED, Some(stored.as_str())),
            "another target's run relabelled this rebuild"
        );
        assert_eq!(
            (rebuild.state.as_str(), rebuild.stalled.as_deref()),
            (OUTCOME_PROGRESSING, None),
            "another target's run changed this rebuild's outcome"
        );
        assert!(
            rebuild.note.as_deref().is_some_and(|why| {
                why.contains(&format!(
                "run at HEAD {} before reaching this rebuild failed: fingerprinting the numstat \
                 environment",
                short_sha(&head(root))
            ))
            }),
            "{rebuild:?}"
        );
    }

    #[test]
    fn a_completed_rebuild_whose_follow_up_runs_out_of_time_stops_there() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let main_head = head(root);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
        commit_all(root, &[("a.rs", "a8\n")], "main 8");

        let range_times_out = forcing(sliced(512, None), |walk| walk == HistoryWalk::Incremental);
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, range_times_out).unwrap_err()
        );
        assert!(
            message.contains(&format!(
                "git history rebuilt at {}; reading the commits from there to HEAD",
                short_sha(&main_head)
            )),
            "{message}"
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, main_head);
        assert!(
            db.git_history_build().unwrap().is_none(),
            "the follow-up started another staged rebuild in the same run"
        );
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
    }

    #[test]
    fn another_targets_failure_does_not_stop_a_requested_rebuild_being_continued() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        assert!(index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).is_err());
        assert_eq!(history_rebuild(&db).unwrap().unwrap().cause, CAUSE_FULL);

        // A branch the stored head is not on; listing its commits times out.
        policy_git(root, &["checkout", "-qb", "elsewhere", "HEAD~1"]);
        commit_all(root, &[("x.rs", "x\n")], "elsewhere");
        let plan_times_out = forcing(sliced(1, Some(1)), |walk| walk == HistoryWalk::Plan);
        assert!(index_history(&db, root, &[], IndexMode::Auto, plan_times_out).is_err());
        assert_eq!(
            history_rebuild(&db).unwrap().unwrap().state,
            OUTCOME_PROGRESSING
        );

        policy_git(root, &["checkout", "-q", "side"]);
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "the requested rebuild was not continued: {:?}",
            history_rebuild(&db).unwrap()
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
    }

    #[test]
    fn a_fingerprint_over_half_the_budget_reports_no_progress_with_the_budget_that_fits() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        // 11 s of a 20 s budget, scaled down.
        let slow = WalkLimits {
            budget: Duration::from_millis(2_000),
            environment_delay: Duration::from_millis(1_100),
            max_recheck_allowance: Duration::from_millis(500),
            ..sliced(512, None)
        };
        for _ in 0..2 {
            let message = format!(
                "{:#}",
                index_history(&db, root, &[], IndexMode::Auto, slow).unwrap_err()
            );
            assert!(
                message.contains("made no progress")
                    && message.contains("set CODESAGE_GIT_HISTORY_TIMEOUT_MS to at least")
                    && !message.contains("Rerun `codesage git-index` to continue"),
                "{message}"
            );
            let rebuild = history_rebuild(&db).unwrap().unwrap();
            assert_eq!(
                (rebuild.state.as_str(), rebuild.commits_total),
                (OUTCOME_NO_PROGRESS, 8),
                "{rebuild:?}"
            );
        }
        // The budget it names finishes the rebuild.
        let named = WalkLimits {
            budget: Duration::from_millis(5_000),
            ..slow
        };
        index_history(&db, root, &[], IndexMode::Auto, named).unwrap();
        let single = Database::open_in_memory().unwrap();
        index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(history_rows(&db), history_rows(&single));
    }

    #[test]
    fn a_fingerprint_near_the_allowance_cap_still_saves_progress_every_run() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        // About 7 s of a 20 s budget with the 5 s cap, scaled down, leaving
        // the fingerprint's own git children room under a loaded test run.
        let slow = WalkLimits {
            budget: Duration::from_millis(4_000),
            environment_delay: Duration::from_millis(1_400),
            max_recheck_allowance: Duration::from_millis(1_000),
            ..sliced(1, None)
        };
        let mut saved = 0;
        for run in 0..12 {
            match index_history(&db, root, &[], IndexMode::Auto, slow) {
                Ok(_) => {
                    let single = Database::open_in_memory().unwrap();
                    index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
                    assert_eq!(history_rows(&db), history_rows(&single));
                    return;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    assert!(message.contains("rebuild in progress"), "{message}");
                    let now = history_rebuild(&db).unwrap().unwrap().commits_saved;
                    assert!(
                        now > saved || run == 0,
                        "run {run} saved nothing: {message}"
                    );
                    saved = now;
                }
            }
        }
        panic!("the rebuild did not finish in 12 runs");
    }

    #[test]
    fn the_recheck_allowance_is_capped() {
        assert_eq!(
            recheck_allowance(Duration::from_secs(10), MAX_RECHECK_ALLOWANCE),
            MAX_RECHECK_ALLOWANCE
        );
        assert_eq!(
            recheck_allowance(Duration::from_millis(10), MAX_RECHECK_ALLOWANCE),
            Duration::from_millis(250)
        );
        assert_eq!(
            recheck_allowance(Duration::from_secs(1), MAX_RECHECK_ALLOWANCE),
            Duration::from_secs(2)
        );
    }

    /// A threshold every file exceeds turns the counts later slices read
    /// into `-`.
    fn hide_counts(root: &Path) {
        policy_git(root, &["config", "core.bigFileThreshold", "1"]);
    }

    fn show_counts(root: &Path) {
        policy_git(root, &["config", "--unset", "core.bigFileThreshold"]);
    }

    fn single_walk(root: &Path) -> Vec<Commit> {
        let source = HistorySource::read(root).unwrap();
        history_commits(
            root,
            None,
            anchor_at_commit(root, &head(root)).unwrap().cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap()
    }

    /// [`half_staged_main`] with one commit per slice, so slices follow a
    /// change between them.
    fn half_staged_main_in_single_commits() -> (tempfile::TempDir, Database) {
        let (dir, db) = half_staged_main();
        let status = db.git_history_build_status().unwrap().unwrap();
        db.set_git_history_progress(&GitHistoryProgress {
            slice_len: 1,
            cause: &status.cause,
            stored_sha: status.stored_sha.as_deref(),
            outcome: OUTCOME_PROGRESSING,
            stalled: None,
            run_saved: 0,
        })
        .unwrap();
        (dir, db)
    }

    #[test]
    fn a_config_change_between_slices_discards_the_slice_read_after_it() {
        let (dir, db) = half_staged_main_in_single_commits();
        let root = dir.path();
        let before = db.git_history_build_staged_shas().unwrap();
        let changing = WalkLimits {
            after_slice: Some(hide_counts),
            ..sliced(1, None)
        };
        let progress = match stage_main(&db, root, changing) {
            Staged::Incomplete(progress) => progress,
            Staged::Complete { .. } => panic!("the run saved a slice read across the change"),
        };
        let discarded = progress.discarded.unwrap_or_default();
        assert!(
            discarded.contains("changed while this run read a slice of 2 commits toward"),
            "{discarded}"
        );
        assert_eq!((progress.staged, progress.stalled), (6, None));
        let after = db.git_history_build_staged_shas().unwrap();
        assert!(
            after.len() == 6 && before.is_subset(&after),
            "the slice read before the change is kept, the one after it is not"
        );
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_PROGRESSING, "{rebuild:?}");
        assert_eq!(rebuild.note.as_deref(), Some(discarded.as_str()));
        assert!(rebuild.last_discard_unix.is_some(), "{rebuild:?}");

        let walk = single_walk(root);
        assert!(walk.iter().all(|commit| commit.changes.is_empty()));
        let staged = match stage_main(&db, root, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&walk));
    }

    #[test]
    fn a_config_change_during_one_uninterrupted_rebuild_leaves_the_rows_of_a_single_walk() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        let changing = WalkLimits {
            after_slice: Some(hide_counts),
            ..sliced(1, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, changing).unwrap_err()
        );
        assert!(
            message.contains("git history rebuild in progress: 1 of 8 commits saved")
                && message.contains("changed while this run read a slice of"),
            "{message}"
        );
        assert!(db.get_git_index_state().unwrap().is_none());

        index_history(&db, root, &[], IndexMode::Auto, sliced(1, None)).unwrap();
        let single = Database::open_in_memory().unwrap();
        index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(history_rows(&db), history_rows(&single));
    }

    thread_local! {
        static SLICES_SAVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Hides counts after the first slice and shows them again after the
    /// second, so only the second is read under the changed setting.
    fn hide_counts_for_one_slice(root: &Path) {
        let saved = SLICES_SAVED.with(|count| {
            count.set(count.get() + 1);
            count.get()
        });
        match saved {
            1 => hide_counts(root),
            2 => show_counts(root),
            _ => {}
        }
    }

    #[test]
    fn a_config_change_reverted_before_the_next_slice_is_still_caught() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        let flickering = WalkLimits {
            after_slice: Some(hide_counts_for_one_slice),
            ..sliced(1, None)
        };
        let ran = index_history(&db, root, &[], IndexMode::Auto, flickering);
        assert!(
            ran.as_ref().is_err_and(
                |error| format!("{error:#}").contains("changed while this run read a slice of")
            ),
            "{:?}",
            ran.map(|_| ())
        );
        // The revert the run did not reach.
        if SLICES_SAVED.with(std::cell::Cell::get) == 1 {
            show_counts(root);
        }
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, None)).unwrap();
        let single = Database::open_in_memory().unwrap();
        index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(history_rows(&db), history_rows(&single));
    }

    #[test]
    fn a_hook_run_whose_environment_moves_leaves_the_rebuild_for_the_next_plain_run() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        // A requested rebuild at the stored head, which plain runs continue
        // only while it takes precedence over the same-HEAD pass.
        assert!(index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).is_err());
        assert_eq!(
            history_rebuild(&db).unwrap().unwrap().state,
            OUTCOME_PROGRESSING
        );

        let changing = WalkLimits {
            before_recheck: Some(hide_counts),
            ..sliced(1, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, changing).unwrap_err()
        );
        assert!(
            message.contains("git history rebuild in progress")
                && message.contains("changed while this run read"),
            "{message}"
        );
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_PROGRESSING, "{rebuild:?}");
        assert!(
            rebuild
                .note
                .as_deref()
                .is_some_and(|note| note.contains("changed while this run read")),
            "{rebuild:?}"
        );

        index_history(&db, root, &[], IndexMode::Auto, sliced(1, None)).unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "the next plain run did not finish the rebuild: {:?}",
            history_rebuild(&db).unwrap()
        );
        let single = Database::open_in_memory().unwrap();
        index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(history_rows(&db), history_rows(&single));
    }

    #[test]
    fn branches_whose_attributes_differ_each_keep_their_saved_commits_across_switches() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        // A line that matches no file: the same output, another environment.
        policy_git(root, &["checkout", "-q", "side"]);
        commit_all(
            root,
            &[(".gitattributes", "*.nomatch -diff\n")],
            "side attributes",
        );
        let db = Database::open_in_memory().unwrap();
        let mut saved = HashMap::new();
        let mut counts = Vec::new();
        for run in 0..24 {
            let branch = if run % 2 == 0 { "main" } else { "side" };
            policy_git(root, &["checkout", "-q", branch]);
            match index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))) {
                Ok(_) => {
                    let single = Database::open_in_memory().unwrap();
                    index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
                    assert_eq!(history_rows(&db), history_rows(&single), "{counts:?}");
                    assert!(run >= 7, "{counts:?}");
                    return;
                }
                Err(error) => {
                    let rebuild = history_rebuild(&db).unwrap().unwrap();
                    let before = saved.insert(branch, rebuild.commits_saved).unwrap_or(0);
                    counts.push((branch, rebuild.commits_saved));
                    assert!(
                        rebuild.commits_saved > before,
                        "run {run} on {branch} did not grow its saved count: {counts:?}: \
                         {error:#}"
                    );
                    assert_eq!(rebuild.restarts, usize::from(run == 1), "{rebuild:?}");
                }
            }
        }
        panic!("neither branch finished: {counts:?}");
    }

    /// Writes an untracked root `.gitattributes` that hides `.foo` counts.
    fn hide_foo_counts(root: &Path) {
        std::fs::write(root.join(".gitattributes"), "*.foo -diff\n").unwrap();
    }

    #[test]
    fn an_untracked_root_attributes_file_added_mid_rebuild_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        policy_git(root, &["init", "-q", "-b", "main"]);
        policy_git(root, &["config", "user.email", "test@example.com"]);
        policy_git(root, &["config", "user.name", "Test"]);
        for round in 0..11 {
            commit_all(
                root,
                &[
                    ("data.foo", &format!("d{round}\n")),
                    ("a.rs", &format!("a{round}\n")),
                ],
                &format!("round {round}"),
            );
        }
        let db = Database::open_in_memory().unwrap();
        let adding = WalkLimits {
            after_slice: Some(hide_foo_counts),
            ..sliced(1, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, adding).unwrap_err()
        );
        assert!(
            message.contains("changed while this run read a slice of"),
            "{message}"
        );
        std::fs::remove_file(root.join(".gitattributes")).unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, None)).unwrap();
        assert_eq!(db.git_file("data.foo").unwrap().unwrap().total_commits, 11);
        let single = Database::open_in_memory().unwrap();
        index_history(&single, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(history_rows(&db), history_rows(&single));
    }

    #[test]
    fn restarts_reset_once_a_resumed_run_saves_under_a_stable_environment() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        let mut restarts = Vec::new();
        // A threshold no file reaches: other environments, the same counts.
        for threshold in ["11m", "12m", "13m", "14m", "15m", "15m"] {
            policy_git(root, &["config", "core.bigFileThreshold", threshold]);
            assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
            restarts.push(history_rebuild(&db).unwrap().unwrap().restarts);
        }
        assert_eq!(restarts, [0, 1, 2, 3, 4, 0]);
    }

    #[test]
    fn an_untracked_attributes_file_in_an_intermediate_directory_is_fingerprinted() {
        let dir = attributes_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("deep/mid")).unwrap();
        commit_all(
            root,
            &[("deep/mid/file.rs", "x\n")],
            "a path two levels deep",
        );
        let before = fingerprint_now(root);
        std::fs::write(root.join("deep/.gitattributes"), "*.rs -diff\n").unwrap();
        assert_ne!(fingerprint_now(root), before);
    }

    #[test]
    fn the_directory_attributes_check_stops_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..300).map(|i| format!("d{i:03}")).collect();
        let dirs = |count: usize| -> BTreeSet<&[u8]> {
            names[..count].iter().map(String::as_bytes).collect()
        };
        let budget = Duration::from_millis(1);
        let past = Instant::now();
        let stopped = directory_attributes(dir.path(), dirs(300), past, budget).unwrap_err();
        assert!(is_deadline(&stopped), "{stopped:#}");
        // Fewer than 256 directories finish before the first check.
        assert!(directory_attributes(dir.path(), dirs(255), past, budget).is_ok());
    }

    #[test]
    fn far_diverged_branches_under_one_environment_are_not_restarts() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        let mut restarts = Vec::new();
        for run in 0..6 {
            let branch = if run % 2 == 0 { "main" } else { "side" };
            policy_git(root, &["checkout", "-q", branch]);
            assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
            restarts.push(history_rebuild(&db).unwrap().unwrap().restarts);
        }
        assert_eq!(restarts, [0; 6]);
    }

    #[test]
    fn a_discarded_plan_is_recorded_on_the_saved_rebuild() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        policy_git(root, &["checkout", "-q", "side"]);
        let changing = WalkLimits {
            before_plan_recheck: Some(hide_counts),
            ..sliced(1, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, changing).unwrap_err()
        );
        assert!(message.contains("the commit list toward"), "{message}");
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(
            (
                rebuild.state.as_str(),
                rebuild.commits_saved,
                rebuild.discard_cause.as_deref()
            ),
            (OUTCOME_PROGRESSING, 5, Some(DISCARD_ENVIRONMENT_CHANGED)),
            "{rebuild:?}"
        );
        assert!(
            rebuild
                .note
                .as_deref()
                .is_some_and(|note| note.contains("the commit list toward"))
                && rebuild.last_discard_unix.is_some()
                && rebuild.discarding_since_unix.is_some(),
            "{rebuild:?}"
        );
    }

    #[test]
    fn a_discarded_slice_leaves_a_parked_rebuild_parked() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        db.park_git_history_build(OUTCOME_PARKED, OUTCOME_PROGRESSING, &[])
            .unwrap();
        let parked_at = db.git_history_build_status().unwrap().unwrap().updated_at;
        let changing = WalkLimits {
            before_recheck: Some(hide_counts),
            ..sliced(1, None)
        };
        let progress = match stage_main(&db, root, changing) {
            Staged::Incomplete(progress) => progress,
            Staged::Complete { .. } => panic!("the run saved a slice read across the change"),
        };
        assert!(progress.discarded.is_some(), "{progress:?}");
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_PARKED, "{rebuild:?}");
        assert_eq!(rebuild.commits_saved, 5);
        assert_eq!(rebuild.updated_at_unix, parked_at);
        assert!(rebuild.note.is_some() && rebuild.last_discard_unix.is_some());
    }

    /// A slice whose process takes past the run's deadline to finish.
    fn finish_past_the_deadline(_: &Path) {
        std::thread::sleep(Duration::from_millis(1_100));
    }

    #[test]
    fn a_slice_finished_past_the_deadline_is_rechecked_within_its_own_allowance() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        let late = WalkLimits {
            budget: Duration::from_millis(1_000),
            before_recheck: Some(finish_past_the_deadline),
            ..sliced(1, None)
        };
        let progress = match stage_main(&db, root, late) {
            Staged::Incomplete(progress) => progress,
            Staged::Complete { .. } => panic!("one slice finished the rebuild"),
        };
        assert_eq!(
            (progress.staged, progress.discarded.as_deref()),
            (1, None),
            "{progress:?}"
        );
        assert_eq!(db.git_history_build_staged_shas().unwrap().len(), 1);
    }

    #[test]
    fn a_recheck_that_runs_out_of_time_discards_only_its_slice() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        let before = db.git_history_build_staged_shas().unwrap();
        let recheck_times_out = forcing(sliced(1, None), |walk| walk == HistoryWalk::Recheck);
        let progress = match stage_main(&db, root, recheck_times_out) {
            Staged::Incomplete(progress) => progress,
            Staged::Complete { .. } => panic!("the run saved a slice it could not re-check"),
        };
        let discarded = progress.discarded.unwrap_or_default();
        assert!(
            discarded.contains(
                "re-checking the numstat environment after this run read a slice of 3 commits"
            ) && discarded.contains("ms re-check allowance"),
            "{discarded}"
        );
        assert_eq!(db.git_history_build_staged_shas().unwrap(), before);
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_PROGRESSING, "{rebuild:?}");
        assert_eq!(rebuild.note.as_deref(), Some(discarded.as_str()));
        assert_eq!(
            (
                rebuild.discard_cause.as_deref(),
                rebuild.next_slice_commits,
                progress.slice_len
            ),
            (Some(DISCARD_RECHECK_TIMEOUT), 1, 1),
            "a slice of 3 whose re-check timed out is halved"
        );
        assert!(rebuild.discarding_since_unix.is_some());

        let staged = match stage_main(&db, root, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&single_walk(root)));
    }

    #[test]
    fn a_fingerprint_that_runs_out_of_time_is_named() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        let slow = WalkLimits {
            budget: Duration::from_millis(200),
            environment_delay: Duration::from_millis(300),
            ..sliced(4, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, slow).unwrap_err()
        );
        assert!(
            message.contains("fingerprinting the numstat environment"),
            "{message}"
        );
    }

    #[test]
    fn a_follow_up_that_needs_a_rename_rescan_stops_there() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let main_head = head(root);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
        // Across the test-like boundary, which a re-key cannot reproduce.
        std::fs::create_dir_all(root.join("tests")).unwrap();
        policy_git(root, &["mv", "a.rs", "tests/a.rs"]);
        policy_git(root, &["commit", "-qm", "move a into tests"]);

        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap_err()
        );
        assert!(
            message.contains(&format!(
                "git history rebuilt at {}; a rename between there and HEAD",
                short_sha(&main_head)
            )),
            "{message}"
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, main_head);
        assert!(db.git_history_build().unwrap().is_none());

        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn a_rebuild_unreachable_from_head_is_reported_parked() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let slices_time_out = forcing(sliced(1, None), |walk| {
            matches!(walk, HistoryWalk::Slice(_))
        });
        for _ in 0..2 {
            assert!(index_history(&db, root, &[], IndexMode::Auto, slices_time_out).is_err());
        }
        assert_eq!(
            history_rebuild_at(root, &db).unwrap().unwrap().state,
            OUTCOME_NO_PROGRESS,
            "a stall resumable from HEAD keeps its diagnosis"
        );
        // Back on the stored head: main's rebuild is not reachable from here.
        policy_git(root, &["checkout", "-q", "side"]);
        let parked = history_rebuild_at(root, &db).unwrap().unwrap();
        assert_eq!(parked.state, OUTCOME_PARKED);
        assert!(
            parked.stalled.is_some(),
            "the last reason stays for information"
        );
        // A target git no longer has.
        db.execute_raw_for_tests(&format!(
            "UPDATE git_history_build_v2 SET target_sha = '{}'",
            "d".repeat(40)
        ))
        .unwrap();
        policy_git(root, &["checkout", "-q", "main"]);
        assert_eq!(
            history_rebuild_at(root, &db).unwrap().unwrap().state,
            OUTCOME_PARKED
        );
    }

    /// Child-process entry for tests that need the real process environment:
    /// a parent test sets `GIT_*` variables on this test binary, so every git
    /// child inherits them exactly as a hook's children would. It runs the
    /// action named in `CODESAGE_TEST_CHILD` against `CODESAGE_TEST_ROOT` and
    /// the index at `CODESAGE_TEST_DB`, and writes the result to
    /// `CODESAGE_TEST_OUT`.
    #[test]
    #[ignore = "entry point for child-process tests"]
    fn child_process_entry() {
        let Ok(action) = std::env::var("CODESAGE_TEST_CHILD") else {
            return;
        };
        let var = |name: &str| PathBuf::from(std::env::var(name).unwrap());
        let root = var("CODESAGE_TEST_ROOT");
        let db = Database::open(&var("CODESAGE_TEST_DB")).unwrap();
        let result = match action.as_str() {
            "stage-one" => match stage_main(&db, &root, sliced(1, Some(1))) {
                Staged::Complete { .. } => "complete".to_owned(),
                Staged::Incomplete(progress) => progress.staged.to_string(),
            },
            "kept" => kept_after_a_resume(&db, &root).to_string(),
            "finish" => match stage_main(&db, &root, sliced(1, None)) {
                Staged::Complete { commits, .. } => format!("{:?}", commit_view(&commits)),
                Staged::Incomplete(progress) => format!("incomplete: {progress:?}"),
            },
            "index" => match index_history(&db, &root, &[], IndexMode::Auto, sliced(512, None)) {
                Ok(_) => "ok".to_owned(),
                Err(error) => format!("error: {error:#}"),
            },
            "stage-killed" => {
                let killed = WalkLimits {
                    before_recheck: Some(kill_at_the_second_slice),
                    ..sliced(1, None)
                };
                stage_main(&db, &root, killed);
                "the child was not killed".to_owned()
            }
            "system-part" => {
                let deadline = Instant::now() + Duration::from_secs(60);
                numstat_environment_parts(&root, deadline, Duration::from_secs(60))
                    .unwrap()
                    .into_iter()
                    .find(|(label, _)| label == b"system")
                    .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap()
            }
            other => panic!("unknown child action {other}"),
        };
        std::fs::write(var("CODESAGE_TEST_OUT"), result).unwrap();
    }

    static SLICES_READ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// SIGKILL this process once the second slice has been read and before it
    /// is saved.
    fn kill_at_the_second_slice(_: &Path) {
        if SLICES_READ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            Command::new("kill")
                .args(["-KILL", &std::process::id().to_string()])
                .status()
                .unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    /// Run `action` in a child copy of this test binary whose process
    /// environment adds `env`.
    fn in_child(action: &str, root: &Path, db: &Path, env: &[(&str, &str)]) -> String {
        let out = tempfile::tempdir().unwrap();
        let out_file = out.path().join("result");
        let status = child_status(action, root, db, env, &out_file);
        assert!(status.success(), "child {action} failed");
        std::fs::read_to_string(out_file).unwrap()
    }

    fn child_status(
        action: &str,
        root: &Path,
        db: &Path,
        env: &[(&str, &str)],
        out_file: &Path,
    ) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_history::indexer::tests::child_process_entry",
                "--ignored",
                "--test-threads=1",
                "-q",
            ])
            .env("CODESAGE_TEST_CHILD", action)
            .env("CODESAGE_TEST_ROOT", root)
            .env("CODESAGE_TEST_DB", db)
            .env("CODESAGE_TEST_OUT", out_file)
            .envs(env.iter().copied())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn a_run_killed_mid_slice_keeps_its_plan_and_the_slices_it_saved() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("index.db");
        drop(Database::open(&db).unwrap());
        let out = tempfile::tempdir().unwrap();
        let status = child_status("stage-killed", root, &db, &[], &out.path().join("result"));
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9), "{status:?}");
        }
        let db = Database::open(&db).unwrap();
        let build = db.git_history_build().unwrap().expect("the plan was lost");
        assert_eq!((build.commits.len(), build.staged), (8, 1), "{build:?}");
    }

    /// A directory holding a `git` that runs the real one except when its
    /// first two arguments are `first second`, which it fails.
    #[cfg(unix)]
    fn git_failing(first: &str, second: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let search = std::env::var_os("PATH").unwrap();
        let real = std::env::split_paths(&search)
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let bin = tempfile::tempdir().unwrap();
        let wrapper = bin.path().join("git");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = {first} ] && [ \"$2\" = {second} ]; then exit 1; fi\n\
                 exec '{}' \"$@\"\n",
                real.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin.path().to_path_buf()).chain(std::env::split_paths(&search)),
        )
        .unwrap();
        (bin, path.into_string().unwrap())
    }

    #[test]
    fn a_failing_configuration_listing_is_a_fingerprint_error() {
        #[cfg(unix)]
        {
            let dir = diverging_repo();
            let root = dir.path();
            let db_dir = tempfile::tempdir().unwrap();
            let db = db_dir.path().join("index.db");
            drop(Database::open(&db).unwrap());
            let (_bin, path) = git_failing("config", "--list");
            let failed = in_child("index", root, &db, &[("PATH", &path)]);
            assert!(
                failed.starts_with("error: ") && failed.contains("listing the git configuration"),
                "{failed}"
            );
        }
        #[cfg(not(unix))]
        eprintln!("skipped: the git wrapper is a shell script");
    }

    #[test]
    fn a_system_attributes_file_git_cannot_name_is_recorded_as_unknown() {
        #[cfg(unix)]
        {
            let dir = diverging_repo();
            let root = dir.path();
            let db_dir = tempfile::tempdir().unwrap();
            let db = db_dir.path().join("index.db");
            drop(Database::open(&db).unwrap());
            // What git before 2.42 does: `git var` knows no attribute paths.
            let (_bin, path) = git_failing("var", "GIT_ATTR_SYSTEM");
            assert_eq!(
                in_child("system-part", root, &db, &[("PATH", &path)]),
                "<unknown>"
            );
            assert_ne!(in_child("system-part", root, &db, &[]), "<unknown>");
        }
        #[cfg(not(unix))]
        eprintln!("skipped: the git wrapper is a shell script");
    }

    /// [`half_staged_main`] with the index in a file, for child processes.
    fn half_staged_main_on_disk() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let (dir, staged) = half_staged_main();
        let db_dir = tempfile::tempdir().unwrap();
        let path = db_dir.path().join("index.db");
        let db = Database::open(&path).unwrap();
        let build = staged.git_history_build().unwrap().unwrap();
        let outputs = staged.git_history_build_outputs().unwrap();
        let progress = GitHistoryProgress {
            slice_len: build.slice_len,
            cause: &build.cause,
            stored_sha: build.stored_sha.as_deref(),
            outcome: OUTCOME_PROGRESSING,
            stalled: None,
            run_saved: 0,
        };
        let pairs: Vec<(&str, &str)> = outputs
            .iter()
            .map(|(sha, output)| (sha.as_str(), output.as_str()))
            .collect();
        db.execute_batch(|db| {
            db.start_git_history_build(
                &build.target_sha,
                &build.policy,
                build.cutoff,
                &build.commits,
                &progress,
            )?;
            db.stage_git_history_commits(&pairs, &progress)
        })
        .unwrap();
        (dir, db_dir, path)
    }

    #[test]
    fn inherited_command_line_config_of_ignored_keys_keeps_saved_commits() {
        let (dir, _db_dir, db) = half_staged_main_on_disk();
        let root = dir.path();
        // What `git -c ... commit` exports to the hooks it runs.
        let ide = [(
            "GIT_CONFIG_PARAMETERS",
            "'core.quotepath'='false' 'log.showsignature'='false' 'branch.x.remote'='origin'",
        )];
        for round in 0..2 {
            assert_eq!(
                in_child("kept", root, &db, &ide),
                "5",
                "round {round}, with -c"
            );
            assert_eq!(
                in_child("kept", root, &db, &[]),
                "5",
                "round {round}, without"
            );
        }
        // A hashed key passed the same way does change the fingerprint.
        let algorithm = [("GIT_CONFIG_PARAMETERS", "'diff.algorithm'='patience'")];
        assert_eq!(in_child("kept", root, &db, &algorithm), "0");
    }

    #[test]
    fn inherited_literal_pathspecs_do_not_hide_an_attributes_change() {
        let dir = attributes_repo();
        let root = dir.path();
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("index.db");
        drop(Database::open(&db).unwrap());
        let literal = [("GIT_LITERAL_PATHSPECS", "1")];
        assert_eq!(in_child("stage-one", root, &db, &literal), "1");
        assert_eq!(in_child("stage-one", root, &db, &literal), "complete");
        // Git falls back to the index copy of a deleted file, so empty it.
        std::fs::write(root.join(".gitattributes"), "").unwrap();
        let source = HistorySource::read(root).unwrap();
        let walk = history_commits(
            root,
            None,
            anchor_at_commit(root, &head(root)).unwrap().cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(foo_counted(&walk));
        assert_eq!(
            in_child("finish", root, &db, &literal),
            format!("{:?}", commit_view(&walk))
        );
    }

    #[test]
    fn safe_directory_from_command_line_config_reaches_the_history_children() {
        let dir = diverging_repo();
        let root = dir.path();
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("index.db");
        drop(Database::open(&db).unwrap());
        // Git's own switch for a repository owned by someone else; real
        // foreign ownership needs a second user.
        let foreign = [("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")];
        let refused = in_child("index", root, &db, &foreign);
        assert!(refused.contains("dubious ownership"), "{refused}");
        let trusted = [
            ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", "*"),
        ];
        assert_eq!(in_child("index", root, &db, &trusted), "ok");
    }

    #[test]
    fn an_exported_git_config_file_does_not_hide_the_configuration_the_walk_reads() {
        let dir = diverging_repo();
        let root = dir.path();
        advance_main(root);
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("index.db");
        drop(Database::open(&db).unwrap());
        // `git config --list` would read only this file; `git log` ignores it.
        let exported = [("GIT_CONFIG", "/dev/null")];
        assert_eq!(in_child("stage-one", root, &db, &exported), "1");
        assert_eq!(
            in_child("kept", root, &db, &[]),
            "1",
            "the walk reads the same configuration with and without GIT_CONFIG"
        );
        policy_git(root, &["config", "core.bigFileThreshold", "1"]);
        assert_eq!(in_child("kept", root, &db, &exported), "0");
    }

    #[test]
    fn a_git_version_change_rereads_saved_commits() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (dir, _db_dir, db) = half_staged_main_on_disk();
            let root = dir.path();
            let search = std::env::var_os("PATH").unwrap();
            let real = std::env::split_paths(&search)
                .map(|dir| dir.join("git"))
                .find(|path| path.is_file())
                .unwrap();
            let bin = tempfile::tempdir().unwrap();
            let wrapper = bin.path().join("git");
            std::fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'git version 0.0.0.test'; \
                     exit 0; fi\nexec '{}' \"$@\"\n",
                    real.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::join_paths(
                std::iter::once(bin.path().to_path_buf()).chain(std::env::split_paths(&search)),
            )
            .unwrap();
            assert_eq!(in_child("kept", root, &db, &[]), "5");
            let other_git = [("PATH", path.to_str().unwrap())];
            assert_eq!(in_child("kept", root, &db, &other_git), "0");
        }
        #[cfg(not(unix))]
        eprintln!("skipped: the git wrapper is a shell script");
    }

    #[test]
    fn a_driver_diff_algorithm_change_rereads_saved_commits() {
        let dir = diverging_repo();
        let root = dir.path();
        std::fs::write(root.join(".gitattributes"), "*.rs diff=cdrv\n").unwrap();
        advance_main(root);
        let db = Database::open_in_memory().unwrap();
        for _ in 0..2 {
            stage_main(&db, root, sliced(1, Some(1)));
        }
        assert_eq!(kept_after_a_resume(&db, root), 5);
        policy_git(root, &["config", "diff.cdrv.algorithm", "histogram"]);
        assert_eq!(kept_after_a_resume(&db, root), 0);
        let source = HistorySource::read(root).unwrap();
        let walk = history_commits(
            root,
            None,
            anchor_at_commit(root, &head(root)).unwrap().cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        let staged = match stage_main(&db, root, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&walk));
    }

    #[test]
    fn a_bare_repository_rebuilds_across_bounded_runs() {
        let dir = diverging_repo();
        advance_main(dir.path());
        let bare_dir = tempfile::tempdir().unwrap();
        let bare = bare_dir.path().join("repo.git");
        policy_git(
            dir.path(),
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        let db = Database::open_in_memory().unwrap();
        let mut runs = 0;
        while let Err(error) = index_history(&db, &bare, &[], IndexMode::Auto, sliced(1, Some(1))) {
            runs += 1;
            assert!(runs <= 6, "{error:#}");
            assert!(
                format!("{error:#}").contains("rebuild in progress"),
                "{error:#}"
            );
        }
        assert!(runs >= 1, "a one-commit slice finished in one run");
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, &bare).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn a_stalled_range_rebuild_is_parked_once_an_incremental_pass_catches_up() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        for round in 0..4 {
            commit_all(root, &[("e.rs", &format!("e{round}\n"))], "side more");
        }
        let stuck = forcing(sliced(1, None), |walk| {
            matches!(walk, HistoryWalk::Incremental | HistoryWalk::Slice(_))
        });
        for _ in 0..2 {
            assert!(index_history(&db, root, &[], IndexMode::Auto, stuck).is_err());
        }
        let rebuild = history_rebuild_at(root, &db).unwrap().unwrap();
        assert_eq!(
            (rebuild.cause.as_str(), rebuild.state.as_str()),
            (CAUSE_RANGE_TOO_LONG, OUTCOME_NO_PROGRESS)
        );

        index_history(&db, root, &[], IndexMode::Auto, sliced(1, None)).unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        assert_eq!(
            history_rebuild_at(root, &db).unwrap().unwrap().state,
            OUTCOME_PARKED,
            "a range rebuild the incremental pass made unnecessary still warns"
        );
    }

    #[test]
    fn fingerprint_and_fallback_messages_name_only_what_happened() {
        let timed_out = FingerprintFailed {
            budget: Some(Duration::from_millis(250)),
        }
        .to_string();
        assert!(
            timed_out.ends_with("within the 250 ms budget"),
            "{timed_out}"
        );
        let failed = FingerprintFailed { budget: None }.to_string();
        assert!(!failed.contains("budget"), "{failed}");

        let follow_up = WalkLimits {
            may_stage: false,
            ..sliced(1, None)
        };
        let sha = "a".repeat(40);
        assert!(no_stage_context(follow_up, &sha).starts_with("git history rebuilt at"));
        let fallback = WalkLimits {
            may_preempt: false,
            ..follow_up
        };
        let message = no_stage_context(fallback, &sha);
        assert!(!message.contains("rebuilt"), "{message}");
        assert!(message.contains("could not fingerprint"), "{message}");
    }

    /// Commits on `main` that move gitlink `sub`, with `.gitmodules` setting
    /// `ignore = all` for it.
    fn submodule_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        policy_git(root, &["init", "-q", "-b", "main"]);
        policy_git(root, &["config", "user.email", "test@example.com"]);
        policy_git(root, &["config", "user.name", "Test"]);
        commit_all(
            root,
            &[(
                ".gitmodules",
                "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n\tignore = all\n",
            )],
            "modules",
        );
        for round in 0..4 {
            let pointed = rev(root, "HEAD");
            policy_git(
                root,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{pointed},sub"),
                ],
            );
            // Not `add -A`: with no `sub` directory it would stage the
            // gitlink's deletion.
            std::fs::write(root.join("a.rs"), format!("a{round}\n")).unwrap();
            policy_git(root, &["add", "a.rs"]);
            policy_git(root, &["commit", "-qm", &format!("move {round}")]);
        }
        dir
    }

    #[test]
    fn removing_a_submodule_ignore_rereads_saved_commits() {
        let dir = submodule_repo();
        let root = dir.path();
        let source = HistorySource::read(root).unwrap();
        let walk = || {
            history_commits(
                root,
                None,
                anchor_at_commit(root, &head(root)).unwrap().cutoff(),
                &source,
                Duration::from_secs(60),
            )
            .unwrap()
        };
        let sub_counted = |commits: &[Commit]| {
            commits
                .iter()
                .flat_map(|commit| &commit.changes)
                .any(|change| change.path == "sub")
        };
        assert!(!sub_counted(&walk()), "ignore = all hides the gitlink");
        let db = Database::open_in_memory().unwrap();
        for _ in 0..2 {
            stage_main(&db, root, sliced(1, Some(1)));
        }
        assert_eq!(db.git_history_build_staged_shas().unwrap().len(), 5);
        std::fs::write(
            root.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n",
        )
        .unwrap();
        let single = walk();
        assert!(sub_counted(&single));
        let staged = match stage_main(&db, root, sliced(1, None)) {
            Staged::Complete { commits, .. } => commits,
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        };
        assert_eq!(commit_view(&staged), commit_view(&single));
    }

    #[test]
    fn a_fingerprint_failure_falls_back_to_the_pass_the_rebuild_preempted() {
        let fingerprints = || NUMSTAT_FINGERPRINTS.with(std::cell::Cell::get);
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        assert!(index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).is_err());

        let environment_times_out =
            forcing(sliced(1, Some(1)), |walk| walk == HistoryWalk::Environment);
        index_history(&db, root, &[], IndexMode::Auto, environment_times_out).unwrap();
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_NO_PROGRESS, "{rebuild:?}");
        assert!(
            rebuild
                .stalled
                .as_deref()
                .is_some_and(|why| why.contains("fingerprinting the numstat environment")),
            "{rebuild:?}"
        );
        // It no longer takes precedence, so later passes do not retry it.
        let before = fingerprints();
        commit_all(root, &[("side.rs", "s1\n")], "side more");
        index_history(&db, root, &[], IndexMode::Auto, environment_times_out).unwrap();
        assert_eq!(fingerprints(), before);
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
    }

    #[test]
    fn a_saved_rebuild_no_run_will_finish_is_pruned() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let run = || index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)));
        assert!(run().is_err());
        policy_git(root, &["checkout", "-q", "side"]);
        run().unwrap();
        assert!(
            db.git_history_build().unwrap().is_some(),
            "a rebuild for a branch that still exists is kept"
        );

        db.execute_raw_for_tests(&format!(
            "UPDATE git_history_build_v2_progress SET updated_at = unixepoch() - {}",
            STALE_BUILD_SECS + 60
        ))
        .unwrap();
        run().unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "an expired rebuild was kept"
        );

        policy_git(root, &["checkout", "-q", "main"]);
        assert!(run().is_err());
        policy_git(root, &["checkout", "-q", "side"]);
        db.execute_raw_for_tests(&format!(
            "UPDATE git_history_build_v2 SET target_sha = '{}'",
            "d".repeat(40)
        ))
        .unwrap();
        run().unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "a rebuild of a commit git no longer has was kept"
        );
    }

    #[test]
    fn a_rebuild_runs_keep_discarding_is_kept_while_one_with_no_runs_is_pruned() {
        let (dir, db) = half_staged_main();
        let root = dir.path();
        let age_progress = || {
            db.execute_raw_for_tests(&format!(
                "UPDATE git_history_build_v2_progress SET updated_at = unixepoch() - {}",
                STALE_BUILD_SECS + 60
            ))
            .unwrap();
        };
        age_progress();
        db.record_git_history_discard("the environment moved", DISCARD_ENVIRONMENT_CHANGED, None)
            .unwrap();
        prune_stale_build(&db, root).unwrap();
        let kept = db.git_history_build_status().unwrap().unwrap();
        assert!(
            unix_now() - kept.updated_at > STALE_BUILD_SECS,
            "a discard moved updated_at"
        );

        db.execute_raw_for_tests(&format!(
            "UPDATE git_history_build_v2_progress SET discarded_at = unixepoch() - {}",
            STALE_BUILD_SECS + 60
        ))
        .unwrap();
        prune_stale_build(&db, root).unwrap();
        assert!(db.git_history_build().unwrap().is_none());
    }

    #[test]
    fn a_pass_that_may_not_stage_does_not_start_a_rebuild() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        advance_main(root);
        let follow_up = WalkLimits {
            may_stage: false,
            ..sliced(512, None)
        };
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Incremental, follow_up).unwrap_err()
        );
        assert!(
            message.contains(&format!(
                "git history is indexed at {}, and HEAD {} needs a full rebuild",
                short_sha(&stored),
                short_sha(&head(root))
            )),
            "{message}"
        );
        assert!(db.git_history_build().unwrap().is_none());
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
    }

    #[test]
    fn a_failed_slice_records_its_error_instead_of_progress() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let run = || index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)));
        assert!(run().is_err());
        policy_git(root, &["checkout", "-q", "side"]);
        run().unwrap();
        assert_eq!(history_rebuild(&db).unwrap().unwrap().state, OUTCOME_PARKED);

        // Main's next slice reads `main 5`, whose parent's `a.rs` blob is gone.
        let blob = rev(root, "main~3:a.rs");
        std::fs::remove_file(root.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
        policy_git(root, &["checkout", "-q", "main"]);
        let message = format!("{:#}", run().unwrap_err());
        assert!(!message.contains("rebuild in progress"), "{message}");
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.state, OUTCOME_FAILED, "{rebuild:?}");
        assert_eq!(rebuild.commits_saved, 1);
        assert!(
            rebuild
                .stalled
                .as_deref()
                .is_some_and(|why| why.contains("git log failed")),
            "{rebuild:?}"
        );
    }

    #[test]
    fn a_plan_that_runs_out_of_time_for_another_target_only_notes_the_saved_rebuild() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        advance_main(root);
        let main_head = head(root);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
        assert_eq!(
            history_rebuild(&db).unwrap().unwrap().state,
            OUTCOME_PROGRESSING
        );

        policy_git(root, &["checkout", "-qb", "other", "HEAD~1"]);
        commit_all(root, &[("o.rs", "o\n")], "other work");
        let plan_times_out = forcing(sliced(1, Some(1)), |walk| walk == HistoryWalk::Plan);
        assert!(index_history(&db, root, &[], IndexMode::Auto, plan_times_out).is_err());
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(rebuild.target_sha, main_head, "the saved commits are kept");
        assert_eq!(
            (rebuild.state.as_str(), rebuild.stalled.as_deref()),
            (OUTCOME_PROGRESSING, None),
            "another target's listing changed this rebuild's outcome"
        );
        assert!(
            rebuild
                .note
                .as_deref()
                .is_some_and(|why| why.contains(&format!(
                    "listing the commits toward {} ran out of time",
                    short_sha(&head(root))
                ))),
            "{rebuild:?}"
        );
        // The next run that works on the rebuild clears the note.
        policy_git(root, &["checkout", "-q", "main"]);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
        assert_eq!(history_rebuild(&db).unwrap().unwrap().note, None);
    }

    #[test]
    fn an_explicit_full_rebuild_is_continued_by_later_passes() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);

        // HEAD is the stored head: the next pass would otherwise take the
        // same-HEAD path and park the rebuild.
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).unwrap_err()
        );
        assert!(message.contains("rebuild in progress"), "{message}");
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(
            (rebuild.cause.as_str(), rebuild.stored_sha.as_deref()),
            (CAUSE_FULL, Some(stored.as_str()))
        );
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "{:?}",
            history_rebuild(&db).unwrap()
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));

        // HEAD has moved past the stored head.
        advance_main(root);
        assert!(index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).is_err());
        // Back on the stored line with main merged: the diverged rebuild is
        // parked, and its target is an ancestor of HEAD.
        policy_git(root, &["checkout", "-q", "side"]);
        policy_git(root, &["merge", "-q", "--no-edit", "-X", "ours", "main"]);

        assert!(index_history(&db, root, &[], IndexMode::Full, sliced(1, Some(1))).is_err());
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(
            (rebuild.cause.as_str(), rebuild.stored_sha.as_deref()),
            (CAUSE_FULL, Some(stored.as_str()))
        );

        // The next hook pass finishes it rather than going incremental and
        // parking it again.
        index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1))).unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "{:?}",
            history_rebuild(&db).unwrap()
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
    }

    #[test]
    fn a_rebuild_left_against_the_stored_head_preempts_an_incremental_pass_that_would_succeed() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        for round in 0..4 {
            commit_all(root, &[("e.rs", &format!("e{round}\n"))], "side more");
        }
        let range_times_out = forcing(sliced(2, Some(1)), |walk| walk == HistoryWalk::Incremental);
        assert!(index_history(&db, root, &[], IndexMode::Auto, range_times_out).is_err());
        let rebuild = history_rebuild(&db).unwrap().unwrap();
        assert_eq!(
            (rebuild.cause.as_str(), rebuild.stored_sha.as_deref()),
            (CAUSE_RANGE_TOO_LONG, Some(stored.as_str()))
        );

        // The incremental walk would now succeed; the rebuild is finished
        // instead, so nothing is left parked.
        index_history(&db, root, &[], IndexMode::Auto, sliced(2, Some(1))).unwrap();
        assert!(
            db.git_history_build().unwrap().is_none(),
            "{:?}",
            history_rebuild(&db).unwrap()
        );
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn git_index_rebuilds_in_slices_on_every_migrated_starting_state() {
        for state in ["fresh", "562c496", "0.49.0"] {
            let dir = diverging_repo();
            let root = dir.path();
            let db_dir = tempfile::tempdir().unwrap();
            let path = db_dir.path().join("index.db");
            if state != "fresh" {
                let db = Database::open(&path).unwrap();
                let mut rewind = String::from(
                    "DROP TABLE git_history_build_v2_commits;
                     DROP TABLE git_history_build_v2_keys;
                     DROP TABLE git_history_build_v2_progress;
                     DROP TABLE git_history_build_v2;
                     DELETE FROM schema_migrations WHERE name = '0031_git_history_build_v2';",
                );
                rewind.push_str(if state == "562c496" {
                    "INSERT INTO git_history_build VALUES (1, 'head', 'policy', 0, 'a');
                     INSERT INTO git_history_build_progress VALUES (1, 4);
                     INSERT INTO git_history_build_slices VALUES (0, 1, 'out');"
                } else {
                    "DROP TABLE git_history_build_slices;
                     DROP TABLE git_history_build_progress;
                     DROP TABLE git_history_build;
                     DELETE FROM schema_migrations WHERE name = '0030_git_history_build';"
                });
                db.execute_raw_for_tests(&rewind).unwrap();
            }
            let db = Database::open(&path).unwrap();
            let mut runs = 0;
            while let Err(error) =
                index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)))
            {
                runs += 1;
                assert!(runs <= 6, "{state}: {error:#}");
                assert!(
                    format!("{error:#}").contains("rebuild in progress"),
                    "{state}: {error:#}"
                );
            }
            assert!(runs >= 1, "{state}: a one-commit slice finished in one run");
            assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
            let fresh = Database::open_in_memory().unwrap();
            git_history_index(&fresh, root).unwrap();
            assert_eq!(history_rows(&db), history_rows(&fresh), "{state}");
        }
    }

    #[test]
    fn split_history_cuts_at_the_headers_each_parser_recognizes() {
        let numstat = "commit\tabc\t1700000000\ta@b\u{1f}A\u{1f}one\0\n1\t0\t\0commit\tfake\0new.rs\0\
                       \0commit\tbad\tnot-a-date\ta@b\u{1f}A\u{1f}bad\0\n2\t0\tx.rs\0\
                       \0commit\tdef\t1700000001\ta@b\u{1f}A\u{1f}two\0\n3\t1\ty.rs\0";
        let tree_only = "\0commit\tabc\t1700000000\ta@b\u{1f}A\u{1f}change\0\n\nfirst.rs\0commit\tfake\t123\0tab\tpath.rs\0\0\0commit\tdef\t1700000001\ta@b\u{1f}A\u{1f}next\0\nlast.rs\0";
        for (raw, tree_only, shas) in [
            (numstat, false, &["abc", "bad", "def"][..]),
            (tree_only, true, &["abc", "def"][..]),
        ] {
            let source = HistorySource {
                tree_only,
                shallow: BTreeSet::new(),
            };
            let parts = split_history(raw, &source).unwrap();
            assert_eq!(
                parts
                    .iter()
                    .map(|(sha, _)| sha.as_str())
                    .collect::<Vec<_>>(),
                shas
            );
            let mut alone = Vec::new();
            for (sha, part) in &parts {
                let (commits, headers) = parse_history_with_headers(part, &source).unwrap();
                assert_eq!(
                    headers.iter().map(|h| h.sha.as_str()).collect::<Vec<_>>(),
                    [sha.as_str()]
                );
                alone.extend(commits);
            }
            assert_eq!(
                commit_view(&alone),
                commit_view(&parse_history(raw, &source).unwrap()),
                "tree_only={tree_only}"
            );
        }
        let numstat_source = HistorySource {
            tree_only: false,
            shallow: BTreeSet::new(),
        };
        let parts = split_history(numstat, &numstat_source).unwrap();
        let (renamed, _) = parse_history_with_headers(parts[0].1, &numstat_source).unwrap();
        assert_eq!(
            renamed[0].renames,
            [("commit\tfake".to_owned(), "new.rs".to_owned())]
        );
        let (dropped, _) = parse_history_with_headers(parts[1].1, &numstat_source).unwrap();
        assert!(
            dropped.is_empty(),
            "a commit whose date does not parse is dropped"
        );
        assert!(
            split_history(&format!("1\t2\tstray.rs\0{numstat}"), &numstat_source).is_err(),
            "a record ahead of the first header must not be dropped silently"
        );
    }

    #[test]
    fn diverged_rebuild_that_cannot_fit_one_commit_names_the_recovery() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        advance_main(root);
        // Plan the rebuild, then let every slice run out of time.
        let planned = index_history(&db, root, &[], IndexMode::Auto, sliced(4, Some(0)))
            .unwrap_err()
            .to_string();
        assert!(
            planned.contains("git history rebuild in progress"),
            "{planned}"
        );
        let slices_time_out = forcing(sliced(4, None), |walk| {
            matches!(walk, HistoryWalk::Slice(_))
        });
        let messages: Vec<String> = (0..3)
            .map(|_| {
                let error =
                    index_history(&db, root, &[], IndexMode::Auto, slices_time_out).unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains(&format!(
                        "stored git history head {} is not an ancestor of HEAD",
                        short_sha(&stored)
                    )),
                    "{message}"
                );
                assert!(!message.contains("truncated by"), "{message}");
                assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
                message
            })
            .collect();
        assert!(
            messages[0].contains("0 of 8 commits saved")
                && messages[0].contains("next slice 2 commits"),
            "{messages:#?}"
        );
        assert!(
            messages[1].contains("next slice 1 commits"),
            "{messages:#?}"
        );
        let last = &messages[2];
        assert!(
            last.contains("made no progress within the 60000 ms budget"),
            "{last}"
        );
        assert!(
            last.contains("alone takes longer than the budget"),
            "{last}"
        );
        assert!(
            last.contains("`CODESAGE_GIT_HISTORY_TIMEOUT_MS=<ms> codesage git-index --full`"),
            "{last}"
        );
        assert_eq!(db.git_history_build().unwrap().unwrap().slice_len, 1);
    }

    #[test]
    fn incremental_range_over_budget_rebuilds_in_slices_instead_of_failing_every_run() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        let before = history_rows(&db);
        for round in 0..4 {
            commit_all(
                root,
                &[
                    ("side.rs", &format!("s{round}\n")),
                    ("e.rs", &format!("e{round}\n")),
                ],
                &format!("side {round}"),
            );
        }
        assert!(is_ancestor(root, &stored, &head(root)).unwrap());

        let range_times_out = forcing(sliced(2, Some(1)), |walk| walk == HistoryWalk::Incremental);
        let message = format!(
            "{:#}",
            index_history(&db, root, &[], IndexMode::Incremental, range_times_out).unwrap_err()
        );
        assert!(
            message.contains(&format!(
                "reading the commits from stored git history head {} to HEAD",
                short_sha(&stored)
            )),
            "{message}"
        );
        assert!(
            message.contains("git history rebuild in progress: 2 of 7 commits saved"),
            "{message}"
        );
        assert_finish_command_is_scoped(&message);
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, stored);
        assert_eq!(history_rows(&db), before, "partial rows became visible");

        // Later runs resume the rebuild rather than retry the range.
        let mut runs = 0;
        while let Err(error) =
            index_history(&db, root, &[], IndexMode::Incremental, range_times_out)
        {
            runs += 1;
            assert!(runs <= 10, "the rebuild never completed: {error:#}");
            assert!(
                format!("{error:#}").contains("rebuild in progress"),
                "{error:#}"
            );
            assert_eq!(history_rows(&db), before, "partial rows became visible");
        }
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
        assert!(db.git_history_build().unwrap().is_none());
        let fresh = Database::open_in_memory().unwrap();
        git_history_index(&fresh, root).unwrap();
        assert_eq!(history_rows(&db), history_rows(&fresh));
    }

    #[test]
    fn commit_with_an_unparseable_date_is_dropped_as_by_a_single_walk() {
        use std::io::Write;
        use std::process::Stdio;

        let dir = diverging_repo();
        let root = dir.path();
        let rev = |spec: &str| {
            let out = Command::new("git")
                .args(["rev-parse", spec])
                .current_dir(root)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        // Git's walk reads the leading digits of this date; `%ct` prints nothing.
        let object = format!(
            "tree {}\nparent {}\nauthor T <t@e> {now} +0000\ncommitter T <t@e> {now}x +0000\n\nbad date\n",
            rev("HEAD^{tree}"),
            rev("HEAD"),
            now = unix_now() - 60
        );
        let mut child = Command::new("git")
            .args([
                "hash-object",
                "-t",
                "commit",
                "--literally",
                "-w",
                "--stdin",
            ])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(object.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let bad = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        policy_git(root, &["update-ref", "HEAD", &bad]);
        commit_all(root, &[("a.rs", "after\n")], "after bad date");

        let source = HistorySource::read(root).unwrap();
        let anchor = anchor_at_commit(root, &head(root)).unwrap();
        let plan =
            plan_history(root, &head(root), anchor.cutoff(), Duration::from_secs(60)).unwrap();
        assert!(plan.contains(&bad), "the walk must list the commit");
        let walk = history_commits(
            root,
            None,
            anchor.cutoff(),
            &source,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(walk.iter().all(|commit| commit.sha != bad));
        let shas = |commits: &[Commit]| commits.iter().map(|c| c.sha.clone()).collect::<Vec<_>>();

        let db = Database::open_in_memory().unwrap();
        let fingerprint = source.fingerprint(&effective_exclusion_fingerprint(&[]));
        let anchors = PassAnchors {
            current: anchor,
            previous: anchor.epoch,
            exclusion_fingerprint: &fingerprint,
            source: &source,
            limits: sliced(2, None),
        };
        match stage_history(&db, root, &head(root), &anchors, FullReason::Rebuild).unwrap() {
            Staged::Complete { commits, .. } => assert_eq!(shas(&commits), shas(&walk)),
            Staged::Incomplete(progress) => panic!("staging stopped: {progress:?}"),
        }
        git_history_index(&db, root).unwrap();
        assert_eq!(db.get_git_index_state().unwrap().unwrap().0, head(root));
    }

    #[test]
    fn policy_change_on_a_diverged_head_still_names_the_divergence() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        index_history(&db, root, &[], IndexMode::Auto, sliced(512, None)).unwrap();
        let stored = head(root);
        advance_main(root);
        let message = format!(
            "{:#}",
            index_history(
                &db,
                root,
                &["side.rs".to_owned()],
                IndexMode::Auto,
                sliced(1, Some(1))
            )
            .unwrap_err()
        );
        assert!(
            message.starts_with(&format!(
                "stored git history head {} is not an ancestor of HEAD {}",
                short_sha(&stored),
                short_sha(&head(root))
            )),
            "{message}"
        );
        assert!(
            message.contains("git history rebuild in progress"),
            "{message}"
        );
    }

    #[test]
    fn unfinished_first_rebuild_reports_progress_without_a_divergence_cause() {
        let dir = diverging_repo();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        let message = index_history(&db, root, &[], IndexMode::Auto, sliced(1, Some(1)))
            .unwrap_err()
            .to_string();
        assert!(
            message.starts_with("git history rebuild in progress: 1 of 3 commits saved"),
            "{message}"
        );
        assert!(!message.contains("not an ancestor"), "{message}");
        assert!(db.get_git_index_state().unwrap().is_none());
        assert!(db.git_file("a.rs").unwrap().is_none());
    }
}
