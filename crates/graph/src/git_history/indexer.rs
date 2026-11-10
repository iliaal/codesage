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
use codesage_storage::db::{CoChangeWrite, GitHistoryBuild};
use globset::GlobSet;

use super::bus_factor::normalized_author;
use super::source::HistorySource;
use crate::git::{HistoryDeadline, git_history_command, history_output};

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
}

/// One budgeted git child of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryWalk {
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

    let mut diverged_from = None;
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
            Some((last_sha, last_indexed_at)) if last_sha == head_sha => {
                // HEAD is unchanged, so the anchor is too: the decay below is a
                // no-op unless the last pass measured from a different clock.
                let previous = previous_anchor(root, &last_sha, last_indexed_at, anchor)?;
                db.execute_batch(|db| {
                    decay_git_history_between(db, previous, anchor.epoch)?;
                    db.prune_git_author_events(anchor.cutoff())?;
                    anchor.persist(db, &head_sha, false)?;
                    db.set_git_index_exclusion_fingerprint(&exclusion_fingerprint)?;
                    db.clear_git_history_build()
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
                } else if resumable_build(db, root, &head_sha, &exclusion_fingerprint)?.is_some() {
                    // Only a failed pass leaves a rebuild behind, so the
                    // incremental range would fail the same way again.
                    IndexMode::Full
                } else {
                    IndexMode::Incremental
                }
            }
            None => IndexMode::Full,
        },
    };

    let mut stats = match effective_mode {
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
    }?;
    // A resumed rebuild completes at the commit it was planned for; HEAD may
    // have moved on since, along a range an incremental pass composes onto it.
    if db
        .get_git_index_state()?
        .is_some_and(|(indexed, _)| indexed != head_sha)
    {
        let follow_up = index_history(db, root, extra_excludes, IndexMode::Incremental, limits)?;
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
    let (target, mut commits) = match stage_history(db, root, head_sha, &anchors)? {
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
    if build.policy != policy {
        return Ok(None);
    }
    if build.target_sha != head_sha && !is_ancestor(root, &build.target_sha, head_sha)? {
        return Ok(None);
    }
    if anchor_at_commit(root, &build.target_sha)?.cutoff() != build.cutoff {
        return Ok(None);
    }
    Ok(Some(build))
}

/// Read the full history in slices that each fit in what remains of the
/// run's budget, saving each one, so a history whose single walk exceeds the
/// budget completes over several runs. The commits are planned once, in the
/// order and window a single walk would list them, and each slice reads the
/// same per-commit output, so the assembled commits equal that walk's.
fn stage_history(
    db: &Database,
    root: &Path,
    head_sha: &str,
    anchors: &PassAnchors<'_>,
) -> Result<Staged> {
    let limits = anchors.limits;
    let source = anchors.source;
    let mut planned_at = None;
    let build = match resumable_build(db, root, head_sha, anchors.exclusion_fingerprint)? {
        Some(build) => build,
        None => {
            let started = Instant::now();
            planned_at = Some(started);
            let cutoff = anchors.current.cutoff();
            let planned = limits.run(HistoryWalk::Plan, || {
                plan_history(root, head_sha, cutoff, limits.budget)
            });
            let commits = match planned {
                Ok(commits) => commits,
                Err(error) if is_deadline(&error) => {
                    return Ok(Staged::Incomplete(BuildProgress {
                        target: head_sha.to_owned(),
                        staged: 0,
                        total: 0,
                        slice_len: 0,
                        stalled: Some(
                            "listing the commits in the history window ran out of time".into(),
                        ),
                    }));
                }
                Err(error) => return Err(error),
            };
            let slice_len = limits.first_slice.clamp(1, MAX_SLICE_COMMITS);
            db.execute_batch(|db| {
                db.start_git_history_build(
                    head_sha,
                    anchors.exclusion_fingerprint,
                    cutoff,
                    &commits,
                    slice_len,
                )
            })?;
            GitHistoryBuild {
                target_sha: head_sha.to_owned(),
                policy: anchors.exclusion_fingerprint.to_owned(),
                cutoff,
                commits,
                slice_len,
                staged: 0,
            }
        }
    };

    // A run that planned shares the budget with planning; one that resumes
    // gives its first slice the whole budget, so a timeout there measures
    // the slice rather than the overhead before it.
    let deadline = planned_at.unwrap_or_else(Instant::now) + limits.budget;
    let total = build.commits.len();
    let mut staged = build.staged;
    let mut slice_len = build.slice_len.clamp(1, MAX_SLICE_COMMITS);
    let mut slices = 0usize;
    let mut advanced = planned_at.is_some();
    let mut stalled = None;
    while staged < total {
        if limits.max_slices.is_some_and(|max| slices >= max) {
            break;
        }
        let started = Instant::now();
        let given = deadline.saturating_duration_since(started);
        if (slices > 0 || planned_at.is_some()) && given.is_zero() {
            break;
        }
        let shas = &build.commits[staged..(staged + slice_len).min(total)];
        let read = limits.run(HistoryWalk::Slice(shas.len()), || {
            read_history_slice(root, shas, source.tree_only, given)
        });
        match read {
            Ok(output) => {
                // Headers, not parsed commits: a single walk also drops a
                // commit whose date does not parse, and so must the slice.
                let (_, headers) = parse_history_with_headers(&output, source)?;
                anyhow::ensure!(
                    headers.iter().eq(shas.iter()),
                    "git log returned other commits than the {} requested from {}",
                    shas.len(),
                    shas[0]
                );
                let next = next_slice_len(shas.len(), started.elapsed(), limits.budget);
                db.execute_batch(|db| {
                    db.stage_git_history_slice(staged, shas.len(), &output, next)
                })?;
                staged += shas.len();
                slice_len = next;
                slices += 1;
                advanced = true;
            }
            Err(error) if is_deadline(&error) => {
                // Only a slice that had most of the budget says the slice is too large.
                if slices == 0 && given >= limits.budget / 2 {
                    if shas.len() > 1 {
                        slice_len = shas.len() / 2;
                        db.set_git_history_slice_len(slice_len)?;
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
    if staged < total {
        return Ok(Staged::Incomplete(BuildProgress {
            target: build.target_sha,
            staged,
            total,
            slice_len,
            stalled: if advanced {
                None
            } else {
                Some(stalled.unwrap_or_else(|| "no slice was read".into()))
            },
        }));
    }

    let assembled = (|| -> Result<Vec<Commit>> {
        let mut commits = Vec::with_capacity(total);
        let mut expected = 0usize;
        for (start, len, output) in db.git_history_build_slices()? {
            anyhow::ensure!(
                start == expected,
                "staged git history skips from commit {expected} to {start}"
            );
            let (parsed, headers) = parse_history_with_headers(&output, source)?;
            anyhow::ensure!(
                headers.len() == len,
                "staged git history slice at commit {start} holds {} of {len} commits",
                headers.len()
            );
            commits.extend(parsed);
            expected += len;
        }
        anyhow::ensure!(
            expected == total,
            "staged git history holds {expected} of {total} planned commits"
        );
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
        FullReason::Rebuild => String::new(),
    };
    match &progress.stalled {
        Some(stall) => anyhow!(
            "{cause}git history rebuild made no progress within the {budget_ms} ms budget: \
             {stall}; the previous history index is unchanged. Rerun with a larger budget: \
             `CODESAGE_GIT_HISTORY_TIMEOUT_MS=<ms> codesage git-index --full` (at most 600000)"
        ),
        None => anyhow!(
            "{cause}git history rebuild in progress: {} of {} commits saved toward {} within \
             the {budget_ms} ms budget, next slice {} commits; the previous history index is \
             unchanged until the rebuild completes. Rerun `codesage git-index` to continue; \
             `CODESAGE_GIT_HISTORY_TIMEOUT_MS=600000 codesage git-index --full` finishes in one \
             run when the remaining slices fit in that budget",
            progress.staged,
            progress.total,
            short_sha(&progress.target),
            progress.slice_len
        ),
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
        db.clear_git_history_build()
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

/// [`parse_history`] plus the SHA of every commit header in `raw`, in order,
/// including headers whose commit the parse dropped.
fn parse_history_with_headers(
    raw: &str,
    source: &HistorySource,
) -> Result<(Vec<Commit>, Vec<String>)> {
    let (mut commits, headers) = if source.tree_only {
        // The tree-only parse fails on a header it cannot read, so it drops none.
        let commits = parse_name_log(raw)?;
        let headers = commits.iter().map(|commit| commit.sha.clone()).collect();
        (commits, headers)
    } else {
        parse_log_with_headers(raw)?
    };
    for commit in &mut commits {
        commit.boundary = source.shallow.contains(&commit.sha);
    }
    Ok((commits, headers))
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
    let mut command = git_history_command(root);
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
    let mut command = git_history_command(root);
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
    let mut command = git_history_command(root);
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

fn parse_name_log(raw: &str) -> Result<Vec<Commit>> {
    let mut commits = Vec::new();
    let mut current: Option<Commit> = None;
    let mut expect_header = true;
    let mut first_path = false;
    for record in raw.split('\0') {
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
    Ok(commits)
}

/// Parse `run_git_log` output. A record that is neither a commit header nor a
/// numstat line is an error rather than a skip: text injected ahead of a header
/// hides that commit and attributes its files to the previous one.
fn parse_log(raw: &str) -> Result<Vec<Commit>> {
    Ok(parse_log_with_headers(raw)?.0)
}

/// [`parse_log`] plus the SHA of every commit header, kept or skipped.
fn parse_log_with_headers(raw: &str) -> Result<(Vec<Commit>, Vec<String>)> {
    let mut commits = Vec::new();
    let mut headers = Vec::new();
    let mut current: Option<Commit> = None;
    let mut skipped_commits = 0usize;
    let mut skipped_changes = 0usize;
    // Keep a bounded sample of dropped commit SHAs so the warning can name
    // which commits were skipped, not just how many.
    let mut skipped_shas: Vec<String> = Vec::new();

    let mut records = raw.split('\0');
    while let Some(record) = records.next() {
        // Git inserts newlines before numstat records and between commits. Strip
        // them only at record boundaries, never from the filename field itself.
        let line = record.trim_start_matches('\n');
        if let Some(rest) = line.strip_prefix("commit\t") {
            if let Some(prev) = current.take() {
                commits.push(prev);
            }
            let mut parts = rest.splitn(3, '\t');
            let _sha = parts.next().unwrap_or("");
            headers.push(_sha.to_owned());
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
            let Some(old_path) = records.next() else {
                break;
            };
            let Some(new_path) = records.next() else {
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
            match stage_history(&db, root, &head_sha, &anchors).unwrap() {
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
        match stage_history(&db, root, &head(root), &anchors).unwrap() {
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
