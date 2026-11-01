//! Structural session baselines persisted under `.codesage/sessions/<id>.json`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use codesage_protocol::work::checkpoint;
use codesage_protocol::{
    RiskAssessment, SessionActedOn, SessionDiff, SessionRiskEntry, SessionRiskRegression,
    SessionSnapshot, SessionToolActedOn, SessionWorktree, SessionWorktreeChange,
};
use codesage_storage::Database;

use crate::git_history::{RiskRequestScope, assess_risk_batch_for_ranking, assess_risk_with_scope};
use crate::impact::WalkCache;

const SESSIONS_DIR: &str = "sessions";

/// Only these highest-risk files receive per-file deltas at session end.
const TOP_RISK_BASELINE: usize = 50;

#[derive(Clone, Debug)]
pub struct CompleteRiskRanking {
    rows: Vec<SessionRiskEntry>,
    assessments: Vec<CachedRiskAssessment>,
    test_rows: Vec<SessionRiskEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct CachedRiskAssessment {
    pub file: String,
    pub score: f64,
    pub unscored: bool,
    pub notes: Vec<String>,
}

impl From<RiskAssessment> for CachedRiskAssessment {
    fn from(assessment: RiskAssessment) -> Self {
        Self {
            file: assessment.file,
            score: assessment.score,
            unscored: assessment.unscored,
            notes: assessment.notes,
        }
    }
}

impl CompleteRiskRanking {
    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.test_rows
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SessionRiskEntry>()),
            )
            .saturating_add(
                self.test_rows
                    .iter()
                    .map(|row| row.file.capacity())
                    .fold(0usize, usize::saturating_add),
            )
            .saturating_add(
                self.rows
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SessionRiskEntry>()),
            )
            .saturating_add(
                self.rows
                    .iter()
                    .map(|row| row.file.capacity())
                    .fold(0usize, usize::saturating_add),
            )
            .saturating_add(
                self.assessments
                    .capacity()
                    .saturating_mul(std::mem::size_of::<CachedRiskAssessment>()),
            )
            .saturating_add(
                self.assessments
                    .iter()
                    .map(|assessment| {
                        assessment
                            .file
                            .capacity()
                            .saturating_add(
                                assessment
                                    .notes
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<String>()),
                            )
                            .saturating_add(
                                assessment
                                    .notes
                                    .iter()
                                    .map(String::capacity)
                                    .fold(0usize, usize::saturating_add),
                            )
                    })
                    .fold(0usize, usize::saturating_add),
            )
    }

    pub fn rows(&self) -> &[SessionRiskEntry] {
        &self.rows
    }

    pub fn test_rows(&self) -> &[SessionRiskEntry] {
        &self.test_rows
    }

    pub fn into_rows(self) -> Vec<SessionRiskEntry> {
        self.rows
    }

    pub(crate) fn assessments(&self) -> &[CachedRiskAssessment] {
        &self.assessments
    }
}

#[derive(Debug)]
pub struct IncompleteRiskRanking {
    cause: anyhow::Error,
}

impl IncompleteRiskRanking {
    pub fn new(cause: anyhow::Error) -> Self {
        Self { cause }
    }
}

impl std::fmt::Display for IncompleteRiskRanking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("risk ranking is incomplete: one or more score components failed")
    }
}

impl std::error::Error for IncompleteRiskRanking {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub fn top_risk_ranking(db: &Database) -> Result<CompleteRiskRanking> {
    top_risk_ranking_with_policy(
        db,
        crate::search::env_default_on("CODESAGE_COUPLING_RECURRENCE"),
    )
}

pub fn top_risk_ranking_with_policy(
    db: &Database,
    recurrence: bool,
) -> Result<CompleteRiskRanking> {
    top_risk_ranking_with_options(db, recurrence, false)
}

/// `include_tests` widens the ranking back to test files, which the default
/// ranking leaves out so hot test modules cannot crowd out product code.
pub fn top_risk_ranking_with_options(
    db: &Database,
    recurrence: bool,
    include_tests: bool,
) -> Result<CompleteRiskRanking> {
    let _policy = crate::git_history::CompletePolicy::enter(recurrence);
    checkpoint()?;
    let mut files = Vec::new();
    let mut tests = Vec::new();
    for (path, stored, interpretation) in db.all_file_test_flags()? {
        let is_test = crate::index::file_is_test(&path, stored, interpretation.as_deref());
        if include_tests || !is_test {
            files.push(path.clone());
        }
        if is_test {
            tests.push(path);
        }
    }
    let candidates = risk_candidates(db, &files)?;
    let test_candidates = risk_candidates(db, &tests)?;
    let mut combined = candidates.clone();
    let included: HashSet<_> = candidates.iter().collect();
    combined.extend(
        test_candidates
            .iter()
            .filter(|path| !included.contains(path))
            .cloned(),
    );
    let (scored, failure) = risk_scores(db, &combined)?;
    if let Some(cause) = failure {
        return Err(IncompleteRiskRanking::new(cause).into());
    }
    let by_file: HashMap<_, _> = scored
        .into_iter()
        .map(|assessment| (assessment.file.clone(), assessment))
        .collect();
    let assessments = rank_population(&by_file, &candidates, TOP_RISK_BASELINE)?;
    let test_rows = rank_population(&by_file, &test_candidates, 10)?
        .into_iter()
        .map(|assessment| SessionRiskEntry {
            file: assessment.file,
            score: assessment.score,
        })
        .collect();
    Ok(CompleteRiskRanking {
        rows: assessments
            .iter()
            .map(|assessment| SessionRiskEntry {
                file: assessment.file.clone(),
                score: assessment.score,
            })
            .collect(),
        assessments,
        test_rows,
    })
}

/// Ignore smaller deltas caused by churn-percentile shifts and scoring jitter.
const RISK_REGRESSION_THRESHOLD: f64 = 0.05;

/// A delta at or above this threshold fails the session gate.
const RISK_FAIL_THRESHOLD: f64 = 0.10;

/// Bound reads of repository-supplied snapshots before parsing.
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// Tolerate backwards clock corrections; duration still clamps to zero.
const MAX_SNAPSHOT_FUTURE_SKEW_SECS: i64 = 24 * 60 * 60;

/// Persist the current index baseline, replacing any snapshot with the same ID.
pub fn session_start(
    project_root: &Path,
    db: &Database,
    session_id: &str,
) -> Result<SessionSnapshot> {
    checkpoint()?;
    validate_session_id(session_id)?;
    let read = db.read_snapshot()?;
    let ranking = top_risk_ranking(db)?;
    let snapshot = build_session_snapshot_with_top_risk(project_root, db, session_id, &ranking)?;
    drop(read);
    persist_session_snapshot(project_root, &snapshot)?;
    Ok(snapshot)
}

/// The caller must pin and validate the database snapshot associated with `ranking`.
pub fn build_session_snapshot_with_top_risk(
    project_root: &Path,
    db: &Database,
    session_id: &str,
    ranking: &CompleteRiskRanking,
) -> Result<SessionSnapshot> {
    checkpoint()?;
    validate_session_id(session_id)?;

    let files = db.all_file_paths().context("listing indexed files")?;
    let file_count = files.len() as u32;
    let symbol_count = db.symbol_count().context("counting symbols")? as u32;
    let (cycles, lazy_edges) = compute_cycles(db).context("computing import cycles")?;
    let top_risk_files = ranking.rows.clone();
    let git_head = read_git_head(project_root);
    let worktree = git_head
        .as_deref()
        .filter(|_| crate::session_ledger::ledger_enabled())
        .and_then(|head| worktree_changes(project_root, head));

    let snapshot = SessionSnapshot {
        session_id: session_id.to_string(),
        created_at: now_unix(),
        file_count,
        symbol_count,
        structural_overlay_paths: db.structural_overlay_paths()?,
        files,
        cycles,
        lazy_edges,
        top_risk_files,
        git_head,
        worktree,
    };

    checkpoint()?;
    Ok(snapshot)
}

pub fn persist_session_snapshot(project_root: &Path, snapshot: &SessionSnapshot) -> Result<()> {
    validate_session_id(&snapshot.session_id)?;
    write_snapshot(project_root, snapshot)
}

/// Diff the current index against a saved baseline, retaining it for reuse.
pub fn session_end(project_root: &Path, db: &Database, session_id: &str) -> Result<SessionDiff> {
    checkpoint()?;
    let _policy = crate::git_history::CompletePolicy::enter_current();
    validate_session_id(session_id)?;
    let snapshot = read_snapshot(project_root, session_id).with_context(|| {
        format!(
            "loading session snapshot '{session_id}' (was session {{action:\"start\"}} called?)"
        )
    })?;

    let now_files = db.all_file_paths().context("listing indexed files")?;
    let now_files_set: HashSet<&str> = now_files.iter().map(|s| s.as_str()).collect();
    let snap_files_set: HashSet<&str> = snapshot.files.iter().map(|s| s.as_str()).collect();

    let mut new_files: Vec<String> = now_files_set
        .difference(&snap_files_set)
        .map(|s| s.to_string())
        .collect();
    new_files.sort();
    let mut removed_files: Vec<String> = snap_files_set
        .difference(&now_files_set)
        .map(|s| s.to_string())
        .collect();
    removed_files.sort();

    let (now_cycles, lazy_edges) = compute_cycles(db).context("computing current cycles")?;
    let snap_cycles_set: HashSet<Vec<String>> = snapshot.cycles.iter().cloned().collect();
    let now_cycles_set: HashSet<Vec<String>> = now_cycles.iter().cloned().collect();
    let mut new_cycles: Vec<Vec<String>> = now_cycles_set
        .difference(&snap_cycles_set)
        .cloned()
        .collect();
    new_cycles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let mut resolved_cycles: Vec<Vec<String>> = snap_cycles_set
        .difference(&now_cycles_set)
        .cloned()
        .collect();
    resolved_cycles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

    let baseline_files: Vec<String> = snapshot
        .top_risk_files
        .iter()
        .filter(|e| now_files_set.contains(e.file.as_str()))
        .map(|e| e.file.clone())
        .collect();
    let (after_scores, risk_failure) = risk_scores(db, &baseline_files)?;
    let mut risk_assessment_failed = risk_failure.is_some();
    let after_by_file: HashMap<&str, f64> = after_scores
        .iter()
        .map(|assessment| (assessment.file.as_str(), assessment.score))
        .collect();

    let mut risk_regressions: Vec<SessionRiskRegression> = Vec::new();
    let mut max_risk_regression = 0.0_f64;
    for entry in &snapshot.top_risk_files {
        checkpoint()?;
        if !now_files_set.contains(entry.file.as_str()) {
            continue;
        }
        let Some(&after) = after_by_file.get(entry.file.as_str()) else {
            // Missing fallback scores must fail the gate closed.
            risk_assessment_failed = true;
            continue;
        };
        let delta = after - entry.score;
        if delta >= RISK_REGRESSION_THRESHOLD {
            if delta > max_risk_regression {
                max_risk_regression = delta;
            }
            risk_regressions.push(SessionRiskRegression {
                file: entry.file.clone(),
                before: entry.score,
                after,
                delta,
            });
        }
    }
    risk_regressions.sort_by(|a, b| {
        b.delta
            .partial_cmp(&a.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let pass = new_cycles.is_empty()
        && max_risk_regression < RISK_FAIL_THRESHOLD
        && !risk_assessment_failed;

    let mut summary_notes = Vec::new();
    if risk_assessment_failed {
        summary_notes.push(
            "risk reassessment failed for one or more baseline files — session gate failed closed"
                .to_string(),
        );
    }
    if !new_cycles.is_empty() {
        let largest = new_cycles.iter().map(|c| c.len()).max().unwrap_or(0);
        summary_notes.push(format!(
            "{} new import cycle(s) introduced (largest: {} files)",
            new_cycles.len(),
            largest
        ));
    }
    if !resolved_cycles.is_empty() {
        summary_notes.push(format!(
            "{} import cycle(s) resolved during session",
            resolved_cycles.len()
        ));
    }
    if max_risk_regression >= RISK_FAIL_THRESHOLD {
        summary_notes.push(format!(
            "max risk regression {max_risk_regression:.2} exceeds fail threshold {:.2}",
            RISK_FAIL_THRESHOLD
        ));
    } else if !risk_regressions.is_empty() {
        summary_notes.push(format!(
            "{} top-risk file(s) regressed (max delta {max_risk_regression:.2})",
            risk_regressions.len()
        ));
    }
    if !new_files.is_empty() || !removed_files.is_empty() {
        summary_notes.push(format!(
            "file count {} → {} ({} added, {} removed)",
            snapshot.file_count,
            now_files.len(),
            new_files.len(),
            removed_files.len()
        ));
    }
    if pass && summary_notes.is_empty() {
        summary_notes.push("no structural regressions detected".to_string());
    }

    let now_unix = now_unix();
    let git_head_after = read_git_head(project_root);
    checkpoint()?;
    let acted_on = acted_on(
        project_root,
        db,
        &snapshot,
        &now_files_set,
        &new_files,
        &removed_files,
        crate::session_ledger::ledger_enabled(),
    )?;

    Ok(SessionDiff {
        session_id: snapshot.session_id,
        duration_seconds: (now_unix - snapshot.created_at).max(0),
        pass,
        file_count_before: snapshot.file_count,
        file_count_after: now_files.len() as u32,
        symbol_count_before: snapshot.symbol_count,
        symbol_count_after: db.symbol_count().context("counting symbols")? as u32,
        new_files,
        removed_files,
        new_cycles,
        resolved_cycles,
        lazy_edges,
        risk_regressions,
        max_risk_regression,
        summary_notes,
        git_head_before: snapshot.git_head,
        git_head_after,
        acted_on,
    })
}

/// Bound the start-of-session change record on trees with mass changes.
const MAX_WORKTREE_CHANGES: usize = 10_000;
/// Bound the per-category path lists in `acted_on`.
const MAX_ACTED_ON_FILES: usize = 50;
/// Files above this size, or past the per-capture hashing budget, get a
/// size and mtime fingerprint instead of a Git blob id.
const MAX_HASHED_FILE_BYTES: u64 = 8 << 20;
const MAX_HASHED_TOTAL_BYTES: u64 = 256 << 20;
/// Shared deadline for the Git children behind one worktree capture or
/// comparison; a child still running at the deadline is killed.
const WORKTREE_GIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Ledger tools whose input is the caller's own patch or file list. Their
/// handles echo files the agent already chose or changed, so they count as
/// checked, not fetched. A single-target `assess_risk` stays a fetch.
const CHECK_TOOLS: &[&str] = &[
    crate::session_ledger::ASSESS_RISK_CHECK_TOOL,
    "recommend_tests",
    "review_rehearsal",
    "edit_check",
];

/// Run an observational Git child with the repository's fsmonitor hook
/// disabled, optional locks off, and a deadline; `None` unless it exits 0.
fn observe_git(
    project_root: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    deadline: std::time::Instant,
) -> Option<Vec<u8>> {
    let mut full = vec!["-c", "core.fsmonitor=false"];
    full.extend_from_slice(args);
    match input {
        None => crate::drift::git_output(project_root, &full, deadline),
        Some(input) => {
            match crate::drift::git_output_with_stdin(project_root, &full, input, deadline) {
                Some((Some(0), stdout)) => Some(stdout),
                _ => None,
            }
        }
    }
}

/// Paths that differ from `base` in the working tree (tracked changes plus
/// untracked, non-ignored files), relative to `project_root`. CodeSage's own
/// state directory is excluded. `None` when Git cannot answer in time.
fn changed_paths(
    project_root: &Path,
    base: &str,
    deadline: std::time::Instant,
) -> Option<Vec<String>> {
    let tracked = observe_git(
        project_root,
        &[
            "diff",
            "--name-only",
            "--relative",
            "--no-renames",
            "--ignore-submodules",
            "-z",
            base,
            "--",
        ],
        None,
        deadline,
    )?;
    let untracked = observe_git(
        project_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        deadline,
    )?;
    let mut paths: Vec<String> = tracked
        .split(|b| *b == 0)
        .chain(untracked.split(|b| *b == 0))
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| String::from_utf8(raw.to_vec()).ok())
        .filter(|path| !path.starts_with(".codesage/"))
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn stat_fingerprint(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("stat:{}:{mtime}", meta.len())
}

/// Content fingerprints for `paths`: a Git blob id for regular files within
/// the hashing bounds, a `stat:<len>:<mtime_ns>` fingerprint for larger
/// ones, and `None` for deleted or non-regular paths. One `git hash-object`
/// child serves the batch.
fn blob_ids(
    project_root: &Path,
    paths: &[String],
    deadline: std::time::Instant,
) -> Option<HashMap<String, Option<String>>> {
    let mut out: HashMap<String, Option<String>> = HashMap::with_capacity(paths.len());
    let mut hashable: Vec<&String> = Vec::new();
    let mut budget = MAX_HASHED_TOTAL_BYTES;
    for path in paths {
        let fingerprint = match std::fs::symlink_metadata(project_root.join(path)) {
            Ok(meta) if meta.is_file() => {
                if path.contains('\n') || meta.len() > MAX_HASHED_FILE_BYTES || meta.len() > budget
                {
                    Some(stat_fingerprint(&meta))
                } else {
                    budget -= meta.len();
                    hashable.push(path);
                    None
                }
            }
            _ => None,
        };
        out.insert(path.clone(), fingerprint);
    }
    if hashable.is_empty() {
        return Some(out);
    }
    let mut input = Vec::new();
    for path in &hashable {
        input.extend_from_slice(path.as_bytes());
        input.push(b'\n');
    }
    let stdout = observe_git(
        project_root,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(&input),
        deadline,
    )?;
    let ids: Vec<&str> = std::str::from_utf8(&stdout).ok()?.lines().collect();
    if ids.len() != hashable.len() {
        return None;
    }
    for (path, id) in hashable.into_iter().zip(ids) {
        out.insert(path.clone(), Some(id.to_string()));
    }
    Some(out)
}

fn worktree_changes(project_root: &Path, head: &str) -> Option<SessionWorktree> {
    let deadline = std::time::Instant::now() + WORKTREE_GIT_BUDGET;
    let mut paths = changed_paths(project_root, head, deadline)?;
    let truncated = paths.len() > MAX_WORKTREE_CHANGES;
    paths.truncate(MAX_WORKTREE_CHANGES);
    let blobs = blob_ids(project_root, &paths, deadline)?;
    Some(SessionWorktree {
        changes: paths
            .into_iter()
            .map(|path| {
                let blob = blobs.get(&path).cloned().flatten();
                SessionWorktreeChange { path, blob }
            })
            .collect(),
        truncated,
    })
}

/// Whether a path recorded at snapshot time still has its recorded content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recheck {
    Same,
    Changed,
    /// Hashing failed or missed its deadline.
    Unknown,
}

/// Compare each path against its start record using the fingerprint kind
/// that record stored, independent of the hashing budget: a `stat:` record
/// compares size and mtime, a blob id is re-hashed (one `git hash-object`
/// batch under `deadline`), and a deleted or non-regular record compares
/// presence. A file now larger than [`MAX_HASHED_FILE_BYTES`] cannot match a
/// blob record, which was only taken at or below that size.
fn recheck_start_records(
    project_root: &Path,
    start: &HashMap<&str, &Option<String>>,
    paths: &[String],
    deadline: std::time::Instant,
) -> HashMap<String, Recheck> {
    let mut out = HashMap::with_capacity(paths.len());
    let mut rehash: Vec<(&String, &str)> = Vec::new();
    for path in paths {
        let Some(&before) = start.get(path.as_str()) else {
            continue;
        };
        let meta = std::fs::symlink_metadata(project_root.join(path))
            .ok()
            .filter(std::fs::Metadata::is_file);
        let verdict = match (before.as_deref(), meta) {
            (None, None) => Recheck::Same,
            (None, Some(_)) | (Some(_), None) => Recheck::Changed,
            (Some(stat), Some(meta)) if stat.starts_with("stat:") => {
                if stat_fingerprint(&meta) == stat {
                    Recheck::Same
                } else {
                    Recheck::Changed
                }
            }
            (Some(_), Some(meta)) if meta.len() > MAX_HASHED_FILE_BYTES => Recheck::Changed,
            (Some(blob), Some(_)) => {
                rehash.push((path, blob));
                continue;
            }
        };
        out.insert(path.clone(), verdict);
    }
    if rehash.is_empty() {
        return out;
    }
    let mut input = Vec::new();
    for (path, _) in &rehash {
        input.extend_from_slice(path.as_bytes());
        input.push(b'\n');
    }
    let ids = observe_git(
        project_root,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(&input),
        deadline,
    )
    .and_then(|stdout| String::from_utf8(stdout).ok())
    .map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
    .filter(|ids| ids.len() == rehash.len());
    for (index, (path, blob)) in rehash.into_iter().enumerate() {
        let verdict = match &ids {
            Some(ids) if ids[index] == blob => Recheck::Same,
            Some(_) => Recheck::Changed,
            None => Recheck::Unknown,
        };
        out.insert(path.clone(), verdict);
    }
    out
}

/// Files edited since the snapshot, how they were determined, and how many
/// recorded paths could not be rechecked.
fn edited_files(
    project_root: &Path,
    snapshot: &SessionSnapshot,
    new_files: &[String],
    removed_files: &[String],
) -> (HashSet<String>, &'static str, usize) {
    let index_only = || {
        (
            new_files.iter().chain(removed_files).cloned().collect(),
            "index",
            0,
        )
    };
    let deadline = std::time::Instant::now() + WORKTREE_GIT_BUDGET;
    let Some(head) = snapshot.git_head.as_deref() else {
        return index_only();
    };
    let Some(now) = changed_paths(project_root, head, deadline) else {
        return index_only();
    };
    let Some(start) = &snapshot.worktree else {
        return (now.into_iter().collect(), "git_head", 0);
    };
    let start: HashMap<&str, &Option<String>> = start
        .changes
        .iter()
        .map(|change| (change.path.as_str(), &change.blob))
        .collect();
    let recheck: Vec<String> = now
        .iter()
        .filter(|path| start.contains_key(path.as_str()))
        .cloned()
        .collect();
    let verdicts = recheck_start_records(project_root, &start, &recheck, deadline);
    let unknown = verdicts
        .values()
        .filter(|verdict| **verdict == Recheck::Unknown)
        .count();
    let now_set: HashSet<&str> = now.iter().map(String::as_str).collect();
    let mut edited: HashSet<String> = now
        .iter()
        .filter(|path| {
            !start.contains_key(path.as_str())
                || verdicts.get(path.as_str()) == Some(&Recheck::Changed)
        })
        .cloned()
        .collect();
    edited.extend(
        start
            .keys()
            .filter(|path| !now_set.contains(*path))
            .map(|path| (*path).to_string()),
    );
    (edited, "worktree", unknown)
}

fn first_sorted(set: &HashSet<String>) -> Vec<String> {
    let mut paths: Vec<String> = set.iter().cloned().collect();
    paths.sort();
    paths.truncate(MAX_ACTED_ON_FILES);
    paths
}

fn acted_on(
    project_root: &Path,
    db: &Database,
    snapshot: &SessionSnapshot,
    now_files: &HashSet<&str>,
    new_files: &[String],
    removed_files: &[String],
    enabled: bool,
) -> Result<SessionActedOn> {
    if !enabled {
        return Ok(SessionActedOn {
            ledger: "disabled".to_string(),
            edited_source: "disabled".to_string(),
            ..Default::default()
        });
    }
    let since = u64::try_from(snapshot.created_at.max(0))
        .unwrap_or(0)
        .saturating_mul(1000);
    let ledger =
        match crate::session_ledger::read_session_ledger(project_root, &snapshot.session_id, since)
        {
            Ok(read) => read,
            Err(err) => {
                tracing::warn!(error = %err, "session ledger unreadable; reporting no calls");
                Default::default()
            }
        };

    let mut served: HashSet<String> = HashSet::new();
    let mut checked: HashSet<String> = HashSet::new();
    let mut recommended: HashSet<String> = HashSet::new();
    let mut by_tool: std::collections::BTreeMap<String, (u32, HashSet<String>)> =
        Default::default();
    for call in &ledger.calls {
        let check = CHECK_TOOLS.contains(&call.tool.as_str());
        let entry = by_tool.entry(call.tool.clone()).or_default();
        entry.0 = entry.0.saturating_add(1);
        for handle in &call.target_handles {
            let Some(path) = crate::session_ledger::handle_path(handle) else {
                continue;
            };
            if call.tool == "recommend_tests" {
                recommended.insert(path.clone());
            }
            if check {
                checked.insert(path.clone());
            } else {
                served.insert(path.clone());
            }
            entry.1.insert(path);
        }
    }

    let (edited, edited_source, edited_unknown) =
        edited_files(project_root, snapshot, new_files, removed_files);
    let snapshot_files: HashSet<&str> = snapshot.files.iter().map(String::as_str).collect();
    let (edited_indexed, edited_unindexed): (HashSet<String>, HashSet<String>) =
        edited.into_iter().partition(|path| {
            now_files.contains(path.as_str()) || snapshot_files.contains(path.as_str())
        });

    let fetched_then_edited: HashSet<String> =
        served.intersection(&edited_indexed).cloned().collect();
    let edited_unfetched: HashSet<String> = edited_indexed.difference(&served).cloned().collect();
    let fetched_unedited = served.difference(&edited_indexed).count();
    let checked_edited = checked.intersection(&edited_indexed).count();
    let recommended_tests_edited: HashSet<String> = if recommended.is_empty() {
        HashSet::new()
    } else {
        let tests: HashSet<String> = db
            .all_file_test_flags()?
            .into_iter()
            .filter(|(path, stored, interpretation)| {
                crate::index::file_is_test(path, *stored, interpretation.as_deref())
            })
            .map(|(path, _, _)| path)
            .collect();
        recommended
            .intersection(&edited_indexed)
            .filter(|path| {
                tests.contains(*path) || codesage_parser::discover::is_test_like_path(path)
            })
            .cloned()
            .collect()
    };

    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Ok(SessionActedOn {
        ledger: if ledger.present { "recorded" } else { "absent" }.to_string(),
        calls: count(ledger.calls.len()),
        edited_source: edited_source.to_string(),
        fetched_then_edited: count(fetched_then_edited.len()),
        edited_unfetched: count(edited_unfetched.len()),
        fetched_unedited: count(fetched_unedited),
        edited_unindexed: count(edited_unindexed.len()),
        edited_unknown: count(edited_unknown),
        checked: count(checked.len()),
        checked_edited: count(checked_edited),
        recommended_tests_edited: count(recommended_tests_edited.len()),
        fetched_then_edited_files: first_sorted(&fetched_then_edited),
        edited_unfetched_files: first_sorted(&edited_unfetched),
        recommended_tests_edited_files: first_sorted(&recommended_tests_edited),
        per_tool: by_tool
            .into_iter()
            .map(|(tool, (calls, files))| {
                let acted = files.intersection(&edited_indexed).count();
                (
                    tool,
                    SessionToolActedOn {
                        calls,
                        fetched: count(files.len()),
                        fetched_then_edited: count(acted),
                    },
                )
            })
            .collect(),
        ledger_capped: ledger.capped,
        worktree_truncated: snapshot
            .worktree
            .as_ref()
            .is_some_and(|worktree| worktree.truncated),
    })
}

/// Compute all non-trivial SCCs in the file-level load-time import graph,
/// plus the count of lazy-only file pairs that would have closed or enlarged
/// a cycle. Each cycle is returned as a sorted member list; the outer Vec is
/// sorted by (descending size, members) for stable equality across recompute.
fn compute_cycles(db: &Database) -> Result<(Vec<Vec<String>>, u32)> {
    let cycles = crate::git_history::ImportCycles::load(db).context("loading import cycles")?;
    let lazy_edges = cycles.suppressed_pairs.len() as u32;
    let mut out: Vec<Vec<String>> = cycles
        .components
        .into_iter()
        .filter(|c| c.len() >= 2)
        .map(|mut c| {
            c.sort();
            c
        })
        .collect();
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    Ok((out, lazy_edges))
}

/// The top-`limit` highest-risk files across the whole indexed project.
/// Backs `project_overview`. Missing history leaves only structural scores;
/// the complete ranking retains that measurement state for later reuse.
pub fn top_risk_files(db: &Database, limit: usize) -> Result<Vec<SessionRiskEntry>> {
    top_risk_files_with_options(db, limit, false)
}

/// Test files (`files.is_test`, or the path heuristic for rows indexed
/// before the flag existed) are excluded unless `include_tests`.
pub fn top_risk_files_with_options(
    db: &Database,
    limit: usize,
    include_tests: bool,
) -> Result<Vec<SessionRiskEntry>> {
    Ok(top_risk_assessments(db, limit, include_tests)?
        .into_iter()
        .map(|assessment| SessionRiskEntry {
            file: assessment.file,
            score: assessment.score,
        })
        .collect())
}

fn top_risk_assessments(
    db: &Database,
    limit: usize,
    include_tests: bool,
) -> Result<Vec<CachedRiskAssessment>> {
    checkpoint()?;
    let _policy = crate::git_history::CompletePolicy::enter_current();
    let files: Vec<String> = db
        .all_file_test_flags()?
        .into_iter()
        .filter(|(path, stored, interpretation)| {
            include_tests || !crate::index::file_is_test(path, *stored, interpretation.as_deref())
        })
        .map(|(path, _, _)| path)
        .collect();
    compute_top_risk(db, &files, limit)
}

/// Score every file via one batched risk call, return the top `limit` by
/// score. Unrecovered score components invalidate the complete ranking.
fn compute_top_risk(
    db: &Database,
    files: &[String],
    limit: usize,
) -> Result<Vec<CachedRiskAssessment>> {
    let candidates = risk_candidates(db, files)?;
    checkpoint()?;
    let (scored, failure) = risk_scores(db, &candidates)?;
    if let Some(cause) = failure {
        return Err(IncompleteRiskRanking::new(cause).into());
    }
    Ok(rank_assessments(scored, limit))
}

fn risk_candidates(db: &Database, files: &[String]) -> Result<Vec<String>> {
    // Bound per-file dependency walks by selecting high-churn candidates.
    const CANDIDATE_BUDGET: usize = 400;
    if files.len() > CANDIDATE_BUDGET {
        db.top_churn_files_for_paths(files, CANDIDATE_BUDGET)
    } else {
        Ok(files.to_vec())
    }
}

fn rank_population(
    by_file: &HashMap<String, CachedRiskAssessment>,
    candidates: &[String],
    limit: usize,
) -> Result<Vec<CachedRiskAssessment>> {
    let scored = candidates
        .iter()
        .map(|path| {
            by_file
                .get(path)
                .cloned()
                .with_context(|| format!("risk batch omitted candidate {path}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(rank_assessments(scored, limit))
}

fn rank_assessments(
    mut scored: Vec<CachedRiskAssessment>,
    limit: usize,
) -> Vec<CachedRiskAssessment> {
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    scored
}

/// Batch shared graph work; fall back to per-file scoring on batch failure.
/// Retain the first unrecovered error alongside any completed scores.
type RiskScores = (Vec<CachedRiskAssessment>, Option<anyhow::Error>);

fn risk_scores(db: &Database, files: &[String]) -> Result<RiskScores> {
    checkpoint()?;
    if files.is_empty() {
        return Ok((Vec::new(), None));
    }
    match assess_risk_batch_for_ranking(db, files) {
        Ok(batch) => {
            let assessments = batch
                .files
                .into_iter()
                .map(|mut assessment| {
                    for note in &mut assessment.notes {
                        if let Some(full) = batch.legend.get(note) {
                            *note = full.clone();
                        }
                    }
                    CachedRiskAssessment::from(assessment)
                })
                .collect();
            Ok((assessments, None))
        }
        batch => {
            checkpoint()?;
            if let Err(e) = batch {
                if e.downcast_ref::<codesage_protocol::work::WorkStopped>()
                    .is_some()
                {
                    return Err(e);
                }
                tracing::warn!(error = %e, "batched risk scoring failed; falling back to per-file assess_risk");
            }
            let mut out = Vec::with_capacity(files.len());
            let mut failure = None;
            // Ranking and card facts do not need the symbol breakdown.
            let mut cache = WalkCache::default();
            let mut scope = RiskRequestScope::score_only(&mut cache);
            for f in files {
                checkpoint()?;
                match assess_risk_with_scope(db, f, &mut scope) {
                    Ok(r) => out.push(CachedRiskAssessment::from(r)),
                    Err(e) => {
                        checkpoint()?;
                        if e.downcast_ref::<codesage_protocol::work::WorkStopped>()
                            .is_some()
                        {
                            return Err(e);
                        }
                        tracing::warn!(error = %e, file = %f, "assess_risk failed; skipping");
                        if failure.is_none() {
                            failure = Some(e);
                        }
                    }
                }
            }
            Ok((out, failure))
        }
    }
}

/// Best-effort `git rev-parse HEAD`. Returns None when not a git repo or
/// git isn't available; sessions still work without it.
fn read_git_head(project_root: &Path) -> Option<String> {
    let out = crate::git_command(project_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reject session ids that contain path separators or other characters
/// that would let a caller escape the sessions/ directory.
pub(crate) fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("session_id must not be empty");
    }
    if id.len() > 128 {
        bail!("session_id too long (max 128 chars)");
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if !id.chars().all(allowed) {
        bail!("session_id may only contain ASCII alphanumerics, '-', '_', '.'");
    }
    if id.starts_with('.') {
        bail!("session_id must not start with '.'");
    }
    Ok(())
}

fn snapshot_path(project_root: &Path, session_id: &str) -> PathBuf {
    project_root
        .join(".codesage")
        .join(SESSIONS_DIR)
        .join(format!("{session_id}.json"))
}

fn write_snapshot(project_root: &Path, snap: &SessionSnapshot) -> Result<()> {
    let path = snapshot_path(project_root, &snap.session_id);
    if let Some(parent) = path.parent() {
        reject_symlinked_parents(project_root, parent)?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating sessions dir {}", parent.display()))?;
    }
    reject_snapshot_symlink(&path)?;
    let json = serde_json::to_vec_pretty(snap).context("serializing snapshot")?;
    ensure!(
        json.len() as u64 <= MAX_SNAPSHOT_BYTES,
        "snapshot payload too large to write ({} bytes, max {MAX_SNAPSHOT_BYTES})",
        json.len()
    );
    checkpoint()?;
    crate::state_file::replace(&path, &json)
        .with_context(|| format!("writing snapshot {}", path.display()))?;
    Ok(())
}

fn read_snapshot(project_root: &Path, session_id: &str) -> Result<SessionSnapshot> {
    let path = snapshot_path(project_root, session_id);
    if let Some(parent) = path.parent() {
        reject_symlinked_parents(project_root, parent)?;
    }
    reject_snapshot_symlink(&path)?;
    let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.len() <= MAX_SNAPSHOT_BYTES,
        "session snapshot {} is too large ({} bytes, max {MAX_SNAPSHOT_BYTES})",
        path.display(),
        meta.len()
    );
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snap: SessionSnapshot =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        snap.session_id == session_id,
        "session snapshot session_id mismatch: expected '{session_id}', got '{}'",
        snap.session_id
    );
    let now = now_unix();
    ensure!(
        snap.created_at <= now + MAX_SNAPSHOT_FUTURE_SKEW_SECS,
        "session snapshot created_at is implausibly far in the future ({})",
        snap.created_at
    );
    Ok(snap)
}

/// Reject repository-planted parent symlinks; the project root may use trusted symlinks.
pub(crate) fn reject_symlinked_parents(project_root: &Path, target: &Path) -> Result<()> {
    let Ok(rel) = target.strip_prefix(project_root) else {
        return Ok(());
    };
    let mut cur = project_root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&cur)
            && meta.file_type().is_symlink()
        {
            bail!(
                "refusing to use session snapshot dir {}: {} is a symlink",
                target.display(),
                cur.display()
            );
        }
    }
    Ok(())
}

fn reject_snapshot_symlink(path: &Path) -> Result<()> {
    // `exists()` misses dangling symlinks that could redirect a write.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    if meta.file_type().is_symlink() {
        bail!(
            "refusing to use session snapshot {}: it is a symlink",
            path.display()
        );
    }
    if !meta.is_file() {
        bail!(
            "refusing to use session snapshot {}: it is not a regular file",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_risk_populations_select_their_own_churn_candidates() {
        let db = Database::open_in_memory().unwrap();
        db.execute_batch(|db| {
            for index in 0..401 {
                for (prefix, is_test, churn) in [("src", false, 1000.0), ("tests", true, 1.0)] {
                    let path = format!("{prefix}/file{index}.rs");
                    db.upsert_file(&codesage_protocol::FileInfo {
                        path: path.clone(),
                        language: codesage_protocol::Language::Rust,
                        content_hash: "fixture".into(),
                        is_test,
                    })?;
                    db.upsert_git_file(&path, churn, 1, 2, Some(1_700_000_000))?;
                }
            }
            Ok(())
        })
        .unwrap();
        let ranking = top_risk_ranking_with_policy(&db, false).unwrap();
        assert_eq!(ranking.rows().len(), 50);
        assert!(
            ranking
                .rows()
                .iter()
                .all(|row| row.file.starts_with("src/"))
        );
        assert_eq!(ranking.test_rows().len(), 10);
        assert!(
            ranking
                .test_rows()
                .iter()
                .all(|row| row.file.starts_with("tests/"))
        );
        let test_paths: Vec<_> = (0..401)
            .map(|index| format!("tests/file{index}.rs"))
            .collect();
        let expected = compute_top_risk(&db, &test_paths[..400], 10).unwrap();
        assert_eq!(ranking.test_rows()[0].score, expected[0].score);
    }

    #[test]
    fn ranking_payload_accounts_for_truncated_vector_and_string_capacity() {
        let mut rows = Vec::with_capacity(400);
        let mut file = String::with_capacity(1024);
        file.push_str("src/a.rs");
        rows.push(SessionRiskEntry { file, score: 0.5 });
        let mut assessments = Vec::with_capacity(300);
        let mut notes = Vec::with_capacity(20);
        let mut note = String::with_capacity(4096);
        note.push_str("unscored history");
        notes.push(note);
        assessments.push(CachedRiskAssessment {
            file: "src/a.rs".into(),
            score: 0.5,
            unscored: true,
            notes,
        });
        let expected = std::mem::size_of::<CompleteRiskRanking>()
            + rows.capacity() * std::mem::size_of::<SessionRiskEntry>()
            + rows[0].file.capacity()
            + assessments.capacity() * std::mem::size_of::<CachedRiskAssessment>()
            + assessments[0].file.capacity()
            + assessments[0].notes.capacity() * std::mem::size_of::<String>()
            + assessments[0].notes[0].capacity();
        let ranking = CompleteRiskRanking {
            rows,
            assessments,
            test_rows: Vec::new(),
        };
        assert_eq!(ranking.allocated_bytes(), expected);
        assert!(ranking.allocated_bytes() >= 400 * std::mem::size_of::<SessionRiskEntry>() + 1024);
    }

    #[test]
    fn concurrent_snapshots_of_one_session_remain_complete() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), &snapshot_with_created_at("same", now_unix())).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let root = dir.path();
                scope.spawn(move || {
                    for _ in 0..20 {
                        write_snapshot(root, &snapshot_with_created_at("same", now_unix()))
                            .unwrap();
                        assert_eq!(read_snapshot(root, "same").unwrap().session_id, "same");
                    }
                });
            }
        });
    }

    #[test]
    fn validate_session_id_accepts_basic_ids() {
        assert!(validate_session_id("default").is_ok());
        assert!(validate_session_id("s-2026-04-27").is_ok());
        assert!(validate_session_id("agent_42.01").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_path_traversal() {
        assert!(validate_session_id("..").is_err());
        assert!(validate_session_id("../../etc").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id(".hidden").is_err());
    }

    fn snapshot_with_created_at(session_id: &str, created_at: i64) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.to_string(),
            created_at,
            file_count: 0,
            symbol_count: 0,
            structural_overlay_paths: Vec::new(),
            files: Vec::new(),
            cycles: Vec::new(),
            lazy_edges: 0,
            top_risk_files: Vec::new(),
            git_head: None,
            worktree: None,
        }
    }

    #[test]
    fn session_end_tolerates_small_future_created_at_with_zero_duration() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let snap = snapshot_with_created_at("skew", now_unix() + 60);
        write_snapshot(dir.path(), &snap).unwrap();

        let diff = session_end(dir.path(), &db, "skew").expect("small skew must be tolerated");
        assert_eq!(
            diff.duration_seconds, 0,
            "negative duration must clamp to 0"
        );
    }

    #[test]
    fn session_end_rejects_implausibly_future_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let snap = snapshot_with_created_at("far", now_unix() + 2 * 24 * 60 * 60);
        write_snapshot(dir.path(), &snap).unwrap();

        let err = session_end(dir.path(), &db, "far").unwrap_err();
        assert!(
            format!("{err:#}").contains("future"),
            "expected future-skew rejection, got: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_refuses_a_symlinked_sessions_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let cs = root.join(".codesage");
        std::fs::create_dir(&cs).unwrap();
        std::os::unix::fs::symlink(&outside, cs.join(SESSIONS_DIR)).unwrap();

        let err = write_snapshot(root, &snapshot_with_created_at("s1", now_unix())).unwrap_err();

        assert!(
            format!("{err:#}").contains("is a symlink"),
            "expected a symlink refusal, got: {err:#}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "the snapshot was written through the symlinked sessions dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_refuses_a_symlinked_codesage_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".codesage")).unwrap();

        let err = write_snapshot(&root, &snapshot_with_created_at("s1", now_unix())).unwrap_err();

        assert!(
            format!("{err:#}").contains("is a symlink"),
            "expected a symlink refusal, got: {err:#}"
        );
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_does_not_use_a_planted_legacy_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("created-by-attacker");
        let sessions = root.join(".codesage").join(SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&target, sessions.join(".s1.json.tmp")).unwrap();

        write_snapshot(root, &snapshot_with_created_at("s1", now_unix())).unwrap();
        assert_eq!(read_snapshot(root, "s1").unwrap().session_id, "s1");
        assert!(!target.exists(), "the temp write created the link's target");
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_refuses_a_dangling_final_snapshot_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("created-by-attacker");
        let sessions = root.join(".codesage").join(SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&target, sessions.join("s1.json")).unwrap();

        let err = write_snapshot(root, &snapshot_with_created_at("s1", now_unix())).unwrap_err();

        assert!(
            format!("{err:#}").contains("is a symlink"),
            "expected a symlink refusal, got: {err:#}"
        );
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_refuses_a_directory_at_the_snapshot_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sessions = root.join(".codesage").join(SESSIONS_DIR);
        std::fs::create_dir_all(sessions.join("s1.json")).unwrap();

        let err = write_snapshot(root, &snapshot_with_created_at("s1", now_unix())).unwrap_err();

        assert!(
            format!("{err:#}").contains("not a regular file"),
            "expected a file-type refusal, got: {err:#}"
        );
        assert!(
            std::fs::read_dir(&sessions)
                .unwrap()
                .filter_map(|e| e.ok())
                .all(|e| e.file_name() == "s1.json"),
            "a temp file was left behind"
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let out = crate::git_command(root)
            .args([
                "-c",
                "user.email=fixture@codesage.test",
                "-c",
                "user.name=fixture",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn ledger_fixture() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = Database::open_in_memory().unwrap();
        for (path, is_test) in [
            ("src/a.rs", false),
            ("src/b.rs", false),
            ("src/c.rs", false),
            ("src/d.rs", false),
            ("src/e.rs", false),
            ("tests/a_test.rs", true),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, format!("// {path}\n")).unwrap();
            db.upsert_file(&codesage_protocol::FileInfo {
                path: path.into(),
                language: codesage_protocol::Language::Rust,
                content_hash: "fixture".into(),
                is_test,
            })
            .unwrap();
        }
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::write(root.join(".codesage/index.db"), b"").unwrap();
        git(root, &["init", "-q", "."]);
        git(root, &["add", "src", "tests"]);
        git(root, &["commit", "-qm", "initial"]);
        (dir, db)
    }

    fn record(root: &Path, tool: &str, handles: &[&str]) {
        let call = crate::session_ledger::LedgerCall {
            ts: crate::session_ledger::now_millis(),
            tool: tool.into(),
            target_handles: handles.iter().map(|h| (*h).to_string()).collect(),
            detail: None,
            bytes: 1,
        };
        crate::session_ledger::append_call(
            root,
            crate::session_ledger::LedgerScope::Session("s"),
            &call,
        )
        .unwrap();
    }

    #[test]
    fn session_end_correlates_served_files_with_files_edited_during_the_session() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        std::fs::write(root.join("src/d.rs"), "// dirty before the session\n").unwrap();
        std::fs::write(root.join("src/e.rs"), "// dirty before the session\n").unwrap();

        session_start(root, &db, "s").unwrap();
        record(root, "describe", &["file:src/a.rs"]);
        record(root, "find_symbol", &["sym:src/c.rs#c", "sym:src/c.rs#d"]);
        record(
            root,
            "recommend_tests",
            &["file:tests/a_test.rs", "file:src/a.rs"],
        );
        record(root, "project_overview", &[]);

        std::fs::write(root.join("src/a.rs"), "// edited\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "// edited\n").unwrap();
        std::fs::write(root.join("src/e.rs"), "// edited again\n").unwrap();
        std::fs::write(root.join("tests/a_test.rs"), "// edited\n").unwrap();
        std::fs::write(root.join("src/unindexed.rs"), "// new\n").unwrap();

        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.ledger, "recorded");
        assert_eq!(acted.edited_source, "worktree");
        assert_eq!(acted.calls, 4);
        assert_eq!(acted.fetched_then_edited, 1);
        assert_eq!(acted.fetched_then_edited_files, vec!["src/a.rs"]);
        assert_eq!(acted.edited_unfetched, 3);
        assert_eq!(
            acted.edited_unfetched_files,
            vec!["src/b.rs", "src/e.rs", "tests/a_test.rs"]
        );
        assert_eq!(acted.fetched_unedited, 1, "src/c.rs");
        assert_eq!((acted.checked, acted.checked_edited), (2, 2));
        assert_eq!(acted.edited_unindexed, 1, "src/unindexed.rs");
        assert_eq!(acted.recommended_tests_edited, 1);
        assert_eq!(
            acted.recommended_tests_edited_files,
            vec!["tests/a_test.rs"]
        );
        let tool = |name: &str| acted.per_tool[name].clone();
        assert_eq!(
            tool("describe"),
            SessionToolActedOn {
                calls: 1,
                fetched: 1,
                fetched_then_edited: 1
            }
        );
        assert_eq!(
            tool("find_symbol"),
            SessionToolActedOn {
                calls: 1,
                fetched: 1,
                fetched_then_edited: 0
            }
        );
        assert_eq!(
            tool("recommend_tests"),
            SessionToolActedOn {
                calls: 1,
                fetched: 2,
                fetched_then_edited: 2
            }
        );
        assert_eq!(tool("project_overview").fetched, 0);
    }

    #[test]
    fn post_edit_checks_do_not_count_as_fetched_then_edited() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        session_start(root, &db, "s").unwrap();
        record(root, "describe", &["file:src/a.rs"]);
        std::fs::write(root.join("src/a.rs"), "// edited\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "// edited\n").unwrap();
        record(
            root,
            "review_rehearsal",
            &["file:src/a.rs", "file:src/b.rs"],
        );
        record(
            root,
            crate::session_ledger::ASSESS_RISK_CHECK_TOOL,
            &["file:src/b.rs"],
        );
        record(root, "assess_risk", &["file:src/c.rs"]);

        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.fetched_then_edited_files, vec!["src/a.rs"]);
        assert_eq!(acted.edited_unfetched_files, vec!["src/b.rs"]);
        assert_eq!(
            acted.fetched_unedited, 1,
            "single-target assess_risk is a fetch"
        );
        assert_eq!((acted.checked, acted.checked_edited), (2, 2));
        assert_eq!(acted.per_tool["review_rehearsal"].fetched_then_edited, 2);
    }

    #[test]
    fn disabled_ledger_reports_nothing_and_skips_edit_detection() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        session_start(root, &db, "s").unwrap();
        record(root, "describe", &["file:src/a.rs"]);
        std::fs::write(root.join("src/a.rs"), "// edited\n").unwrap();
        let snapshot = read_snapshot(root, "s").unwrap();
        let now: HashSet<&str> = snapshot.files.iter().map(String::as_str).collect();
        let acted = acted_on(root, &db, &snapshot, &now, &[], &[], false).unwrap();
        assert_eq!(acted.ledger, "disabled");
        assert_eq!(acted.edited_source, "disabled");
        assert_eq!(
            (
                acted.calls,
                acted.fetched_then_edited,
                acted.edited_unfetched
            ),
            (0, 0, 0)
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_git_children_do_not_run_the_repository_fsmonitor_hook() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        let marker = root.join("fsmonitor-fired");
        let hook = root.join("fsmonitor-hook.sh");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(root, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        std::fs::write(root.join("src/d.rs"), "// dirty before the session\n").unwrap();

        session_start(root, &db, "s").unwrap();
        std::fs::write(root.join("src/a.rs"), "// edited\n").unwrap();
        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.edited_unfetched_files, vec!["src/a.rs"]);
        assert!(!marker.exists(), "session git children ran core.fsmonitor");

        let out = crate::git_command(root).arg("status").output().unwrap();
        assert!(out.status.success());
        assert!(
            marker.exists(),
            "the marker hook must fire for ordinary git status"
        );
    }

    #[cfg(unix)]
    #[test]
    fn large_untracked_files_get_a_stat_fingerprint_instead_of_a_hash() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        let big = std::fs::File::create(root.join("big.bin")).unwrap();
        big.set_len(2 << 30).unwrap();
        drop(big);

        let started = std::time::Instant::now();
        session_start(root, &db, "s").unwrap();
        let elapsed = started.elapsed();
        let snapshot = read_snapshot(root, "s").unwrap();
        let change = snapshot
            .worktree
            .unwrap()
            .changes
            .into_iter()
            .find(|change| change.path == "big.bin")
            .unwrap();
        assert!(
            change
                .blob
                .as_deref()
                .unwrap()
                .starts_with("stat:2147483648:"),
            "{change:?}"
        );
        assert!(elapsed < WORKTREE_GIT_BUDGET, "{elapsed:?}");

        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.edited_source, "worktree");
        assert_eq!(
            acted.edited_unindexed, 0,
            "an untouched large file is not an edit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_budget_exhausted_start_rechecks_each_path_with_its_recorded_kind() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        std::fs::create_dir(root.join("data")).unwrap();
        let count = (MAX_HASHED_TOTAL_BYTES / MAX_HASHED_FILE_BYTES) as usize + 1;
        for index in 0..count {
            let file = std::fs::File::create(root.join(format!("data/big{index:02}.bin"))).unwrap();
            file.set_len(MAX_HASHED_FILE_BYTES).unwrap();
        }
        session_start(root, &db, "s").unwrap();
        let blobs: HashMap<String, Option<String>> = read_snapshot(root, "s")
            .unwrap()
            .worktree
            .unwrap()
            .changes
            .into_iter()
            .map(|change| (change.path, change.blob))
            .collect();
        let kind = |index: usize| blobs[&format!("data/big{index:02}.bin")].clone().unwrap();
        assert!(!kind(count - 2).starts_with("stat:"), "{blobs:?}");
        assert!(kind(count - 1).starts_with("stat:"), "{blobs:?}");

        std::fs::remove_file(root.join("data/big00.bin")).unwrap();
        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.edited_source, "worktree");
        assert_eq!(acted.edited_unindexed, 1, "only data/big00.bin changed");
        assert_eq!(acted.edited_unknown, 0);

        std::fs::write(root.join(format!("data/big{:02}.bin", count - 1)), b"x").unwrap();
        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(
            acted.edited_unindexed, 2,
            "the stat-recorded file changed too"
        );
    }

    #[test]
    fn session_without_calls_or_edits_reports_zeros() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        std::fs::write(root.join("src/d.rs"), "// dirty before the session\n").unwrap();
        session_start(root, &db, "s").unwrap();

        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.ledger, "absent");
        assert_eq!(acted.edited_source, "worktree");
        assert_eq!(
            (
                acted.calls,
                acted.fetched_then_edited,
                acted.edited_unfetched,
                acted.fetched_unedited,
                acted.edited_unindexed,
                acted.recommended_tests_edited,
            ),
            (0, 0, 0, 0, 0, 0)
        );
        assert!(acted.per_tool.is_empty());
    }

    #[test]
    fn ledger_calls_from_before_session_start_are_ignored() {
        let (dir, db) = ledger_fixture();
        let root = dir.path();
        let stale = crate::session_ledger::LedgerCall {
            ts: 1_000,
            tool: "describe".into(),
            target_handles: vec!["file:src/a.rs".into()],
            detail: None,
            bytes: 1,
        };
        crate::session_ledger::append_call(
            root,
            crate::session_ledger::LedgerScope::Session("s"),
            &stale,
        )
        .unwrap();
        session_start(root, &db, "s").unwrap();
        std::fs::write(root.join("src/a.rs"), "// edited\n").unwrap();

        let acted = session_end(root, &db, "s").unwrap().acted_on;
        assert_eq!(acted.calls, 0);
        assert_eq!(acted.fetched_then_edited, 0);
        assert_eq!(acted.edited_unfetched_files, vec!["src/a.rs"]);
    }

    #[test]
    fn session_end_outside_git_falls_back_to_index_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        write_snapshot(dir.path(), &snapshot_with_created_at("nogit", now_unix())).unwrap();
        let acted = session_end(dir.path(), &db, "nogit").unwrap().acted_on;
        assert_eq!(acted.edited_source, "index");
        assert_eq!(acted.edited_unfetched, 0);
    }

    #[test]
    fn write_snapshot_round_trips_through_an_ordinary_sessions_dir() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot_with_created_at("s1", now_unix());
        write_snapshot(dir.path(), &snap).unwrap();

        let read = read_snapshot(dir.path(), "s1").unwrap();
        assert_eq!(read.session_id, "s1");
    }
}
