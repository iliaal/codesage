//! Session baseline: snapshot the structural state of the index at the
//! start of an agent's editing session, diff it against current state at
//! the end. Closes the loop on "did this batch of edits introduce import
//! cycles or regress risk on hot files."
//!
//! Pattern borrowed from sentrux's session_start / session_end MCP tools,
//! reimplemented around CodeSage's existing risk + cycle infrastructure.
//! Snapshots persist as JSON under `.codesage/sessions/<id>.json`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use codesage_protocol::{SessionDiff, SessionRiskEntry, SessionRiskRegression, SessionSnapshot};
use codesage_storage::Database;

use crate::git_history::{assess_risk, assess_risk_batch};

/// Subdirectory under `.codesage/` where session snapshots live.
const SESSIONS_DIR: &str = "sessions";

/// Number of highest-risk files to capture as the regression baseline at
/// snapshot time. Files outside this set don't get a per-file risk delta
/// in `session_end`, which keeps the snapshot bounded on huge repos.
const TOP_RISK_BASELINE: usize = 50;

/// Per-file risk-score delta that counts as a regression. Below this we
/// treat the change as noise (recomputation jitter, churn-percentile
/// shifts as other files change).
const RISK_REGRESSION_THRESHOLD: f64 = 0.05;

/// Maximum risk regression that still passes the session gate. Stricter
/// than the per-file threshold: a single file moving from 0.40 to 0.55
/// is normal during active editing; one moving past 0.10 in delta is
/// the kind of signal worth pausing on.
const RISK_FAIL_THRESHOLD: f64 = 0.10;

/// Upper bound on snapshot file size before read/parse. Large monorepos store
/// full file lists; this caps hostile or corrupted snapshots.
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// Tolerated forward skew on a snapshot's `created_at`. A backwards clock
/// step (NTP correction, WSL2 resume) can leave the stamp ahead of the
/// current wall clock; only skew beyond this bound is treated as a corrupt
/// or hostile snapshot. The computed session duration clamps to ≥ 0.
const MAX_SNAPSHOT_FUTURE_SKEW_SECS: i64 = 24 * 60 * 60;

/// Take a snapshot of the current index state and persist it under
/// `.codesage/sessions/<session_id>.json`. Overwrites any existing
/// snapshot for the same id (re-running session_start is how you reset
/// a baseline mid-session).
pub fn session_start(
    project_root: &Path,
    db: &Database,
    session_id: &str,
) -> Result<SessionSnapshot> {
    validate_session_id(session_id)?;

    let files = db.all_file_paths().context("listing indexed files")?;
    let file_count = files.len() as u32;
    let symbol_count = db.symbol_count().context("counting symbols")? as u32;
    let cycles = compute_cycles(db).context("computing import cycles")?;
    let top_risk_files =
        compute_top_risk(db, &files, TOP_RISK_BASELINE).context("computing top-risk baseline")?;
    let git_head = read_git_head(project_root);

    let snapshot = SessionSnapshot {
        session_id: session_id.to_string(),
        created_at: now_unix(),
        file_count,
        symbol_count,
        files,
        cycles,
        top_risk_files,
        git_head,
    };

    write_snapshot(project_root, &snapshot)?;
    Ok(snapshot)
}

/// Load the snapshot for `session_id` and diff it against current index
/// state. The snapshot file is left in place so the same id can be
/// re-diffed (useful for "is the regression still there after I fixed it?").
pub fn session_end(project_root: &Path, db: &Database, session_id: &str) -> Result<SessionDiff> {
    validate_session_id(session_id)?;
    let snapshot = read_snapshot(project_root, session_id).with_context(|| {
        format!("loading session snapshot '{session_id}' (was session_start called?)")
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

    let now_cycles = compute_cycles(db).context("computing current cycles")?;
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

    // File removed during the session — counted under removed_files,
    // not as a regression.
    let baseline_files: Vec<String> = snapshot
        .top_risk_files
        .iter()
        .filter(|e| now_files_set.contains(e.file.as_str()))
        .map(|e| e.file.clone())
        .collect();
    let (after_scores, mut risk_assessment_failed) = risk_scores(db, &baseline_files);
    let after_by_file: HashMap<&str, f64> =
        after_scores.iter().map(|(f, s)| (f.as_str(), *s)).collect();

    let mut risk_regressions: Vec<SessionRiskRegression> = Vec::new();
    let mut max_risk_regression = 0.0_f64;
    for entry in &snapshot.top_risk_files {
        if !now_files_set.contains(entry.file.as_str()) {
            continue;
        }
        let Some(&after) = after_by_file.get(entry.file.as_str()) else {
            // Scored set omits files whose per-file fallback failed; the
            // failure flag is already set, keep the gate failing closed.
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
        risk_regressions,
        max_risk_regression,
        summary_notes,
        git_head_before: snapshot.git_head,
        git_head_after,
    })
}

/// Compute all non-trivial SCCs in the file-level import graph. Each cycle
/// is returned as a sorted member list; the outer Vec is sorted by
/// (descending size, members) for stable equality across recompute.
fn compute_cycles(db: &Database) -> Result<Vec<Vec<String>>> {
    let edges = db
        .enumerate_file_import_edges()
        .context("enumerate_file_import_edges")?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let components = crate::scc::tarjan_scc(&edges);
    let mut out: Vec<Vec<String>> = components
        .into_iter()
        .filter(|c| c.len() >= 2)
        .map(|mut c| {
            c.sort();
            c
        })
        .collect();
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    Ok(out)
}

/// The top-`limit` highest-risk files across the whole indexed project.
/// Backs `project_overview`. Returns empty when no files are indexed or git
/// history hasn't been indexed (every file scores ~0 without it).
pub fn top_risk_files(db: &Database, limit: usize) -> Result<Vec<SessionRiskEntry>> {
    let files: Vec<String> = db
        .all_files_with_id_and_language()?
        .into_iter()
        .map(|(_, path, _)| path)
        .collect();
    compute_top_risk(db, &files, limit)
}

/// Score every file via one batched risk call, return the top `limit` by
/// score. Files with a risk computation error are skipped (logged).
fn compute_top_risk(
    db: &Database,
    files: &[String],
    limit: usize,
) -> Result<Vec<SessionRiskEntry>> {
    // `assess_risk` runs a depth-2 reverse-dep BFS per file, so scoring every
    // file is O(files × graph) and dominates session_start / project_overview
    // on large repos. Churn is the dominant risk component, so when the file
    // set is large, narrow to the highest-churn candidates before the BFS.
    // Files absent from git history score ~0 anyway.
    const CANDIDATE_BUDGET: usize = 400;
    let candidates: Vec<String> = if files.len() > CANDIDATE_BUDGET {
        let ranked = db.top_churn_files(CANDIDATE_BUDGET)?;
        if ranked.is_empty() {
            // No git history → every file scores ~0; nothing to rank without
            // running the full O(N) BFS sweep for no signal.
            return Ok(Vec::new());
        }
        let allowed: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
        ranked
            .into_iter()
            .filter(|p| allowed.contains(p.as_str()))
            .collect()
    } else {
        files.to_vec()
    };
    let (pairs, _any_failed) = risk_scores(db, &candidates);
    let mut scored: Vec<SessionRiskEntry> = pairs
        .into_iter()
        .map(|(file, score)| SessionRiskEntry { file, score })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    Ok(scored)
}

/// Risk scores for `files` via one `assess_risk_batch` call, which computes
/// the import-cycle SCC set and the churn-percentile table once instead of
/// per file (per-file `assess_risk` re-runs both — tens of ms each on large
/// repos, unaffordable across a 400-candidate sweep).
///
/// If the batch call fails as a whole, falls back to per-file scoring to
/// preserve the skip-on-error semantics: files that error are logged and
/// omitted, and the returned flag reports whether any file failed.
fn risk_scores(db: &Database, files: &[String]) -> (Vec<(String, f64)>, bool) {
    if files.is_empty() {
        return (Vec::new(), false);
    }
    match assess_risk_batch(db, files) {
        Ok(batch) => (
            batch.files.into_iter().map(|r| (r.file, r.score)).collect(),
            false,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "batched risk scoring failed; falling back to per-file assess_risk");
            let mut out = Vec::with_capacity(files.len());
            let mut any_failed = false;
            for f in files {
                match assess_risk(db, f) {
                    Ok(r) => out.push((r.file, r.score)),
                    Err(e) => {
                        tracing::warn!(error = %e, file = %f, "assess_risk failed; skipping");
                        any_failed = true;
                    }
                }
            }
            (out, any_failed)
        }
    }
}

/// Best-effort `git rev-parse HEAD`. Returns None when not a git repo or
/// git isn't available; sessions still work without it.
fn read_git_head(project_root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(project_root)
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
fn validate_session_id(id: &str) -> Result<()> {
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
    let parent = path
        .parent()
        .context("snapshot path must have a parent directory")?;
    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("snapshot")
    );
    let tmp_path = parent.join(tmp_name);
    reject_snapshot_symlink(&tmp_path)?;
    std::fs::write(&tmp_path, &json)
        .with_context(|| format!("writing temp snapshot {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
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

/// Refuse to read or write through a symlinked snapshot path — same posture as
/// the MCP daemon's runtime-dir guard.
/// Refuse to read or write a snapshot through a symlinked parent directory.
///
/// [`reject_snapshot_symlink`] inspects only the final path component, so a
/// repo-planted `.codesage/sessions` (or `.codesage` itself) directory symlink
/// would otherwise redirect `create_dir_all`, the temp write and the rename
/// outside the project tree. Only components under `project_root` are checked;
/// the root's own path may legitimately run through symlinks the user owns.
fn reject_symlinked_parents(project_root: &Path, target: &Path) -> Result<()> {
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
    // lstat directly rather than gating on `Path::exists()`, which follows
    // links: a *dangling* symlink reads as absent there, and the write would
    // then create the file at the link's target.
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
    // A cloned repo can also ship a directory at this name: the temp file
    // would be written and only the rename would fail, leaving state behind.
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
            files: Vec::new(),
            cycles: Vec::new(),
            top_risk_files: Vec::new(),
            git_head: None,
        }
    }

    #[test]
    fn session_end_tolerates_small_future_created_at_with_zero_duration() {
        // A backwards clock step (NTP correction, WSL2 resume) can leave the
        // snapshot stamp slightly ahead of the wall clock; session_end must
        // still work rather than hard-failing until time catches up.
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
        // `.codesage/sessions` ships with the repo, so it can be a directory
        // symlink; `create_dir_all` and the rename would follow it out of tree.
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
    fn write_snapshot_refuses_a_dangling_symlinked_temp_file() {
        // The temp name is derived from the session id, so it is predictable
        // and plantable. A dangling link reads as absent to `Path::exists()`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("created-by-attacker");
        let sessions = root.join(".codesage").join(SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&target, sessions.join(".s1.json.tmp")).unwrap();

        let err = write_snapshot(root, &snapshot_with_created_at("s1", now_unix())).unwrap_err();

        assert!(
            format!("{err:#}").contains("is a symlink"),
            "expected a symlink refusal, got: {err:#}"
        );
        assert!(!target.exists(), "the temp write created the link's target");
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_refuses_a_dangling_final_snapshot_symlink() {
        // `reject_snapshot_symlink` used to gate on `Path::exists()`, which
        // follows links, so a dangling link read as absent.
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
        // A cloned directory here used to pass the symlink-only check: the
        // temp file was written and only the rename failed.
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

    #[test]
    fn write_snapshot_round_trips_through_an_ordinary_sessions_dir() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot_with_created_at("s1", now_unix());
        write_snapshot(dir.path(), &snap).unwrap();

        let read = read_snapshot(dir.path(), "s1").unwrap();
        assert_eq!(read.session_id, "s1");
    }
}
