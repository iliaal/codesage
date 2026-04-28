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

use anyhow::{Context, Result, bail};
use codesage_protocol::{SessionDiff, SessionRiskEntry, SessionRiskRegression, SessionSnapshot};
use codesage_storage::Database;

use crate::git_history::assess_risk;

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

    let mut risk_regressions: Vec<SessionRiskRegression> = Vec::new();
    let mut max_risk_regression = 0.0_f64;
    for entry in &snapshot.top_risk_files {
        if !now_files_set.contains(entry.file.as_str()) {
            // File removed during the session — counted under removed_files,
            // not as a regression.
            continue;
        }
        let after = match assess_risk(db, &entry.file) {
            Ok(r) => r.score,
            Err(e) => {
                tracing::warn!(error = %e, file = %entry.file, "assess_risk failed during session_end");
                continue;
            }
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

    let pass = new_cycles.is_empty() && max_risk_regression < RISK_FAIL_THRESHOLD;

    let mut summary_notes = Vec::new();
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
        duration_seconds: now_unix - snapshot.created_at,
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
    let components = tarjan_scc_local(&edges);
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

/// Score every file via `assess_risk`, return the top `limit` by score.
/// Files with a risk computation error are skipped (logged).
fn compute_top_risk(
    db: &Database,
    files: &[String],
    limit: usize,
) -> Result<Vec<SessionRiskEntry>> {
    let mut scored: Vec<SessionRiskEntry> = Vec::with_capacity(files.len());
    for f in files {
        match assess_risk(db, f) {
            Ok(r) => scored.push(SessionRiskEntry {
                file: r.file,
                score: r.score,
            }),
            Err(e) => {
                tracing::warn!(error = %e, file = %f, "assess_risk failed during snapshot; skipping");
            }
        }
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    Ok(scored)
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
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating sessions dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(snap).context("serializing snapshot")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_snapshot(project_root: &Path, session_id: &str) -> Result<SessionSnapshot> {
    let path = snapshot_path(project_root, session_id);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snap: SessionSnapshot =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(snap)
}

/// Local copy of Tarjan's SCC algorithm. Duplicated rather than reused
/// from `git_history::risk` to keep that module's helper private; the
/// algorithm is small and the duplication keeps internal API surface
/// minimal. If a third call site appears, lift it to a shared helper.
fn tarjan_scc_local(edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut idx_of: HashMap<&str, usize> = HashMap::new();
    let mut nodes: Vec<&str> = Vec::new();
    for (a, b) in edges {
        for n in [a.as_str(), b.as_str()] {
            if !idx_of.contains_key(n) {
                idx_of.insert(n, nodes.len());
                nodes.push(n);
            }
        }
    }
    let n = nodes.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (a, b) in edges {
        let u = idx_of[a.as_str()];
        let v = idx_of[b.as_str()];
        adj[u].push(v);
    }

    const UNVISITED: i32 = -1;
    let mut index_counter: i32 = 0;
    let mut index: Vec<i32> = vec![UNVISITED; n];
    let mut lowlink: Vec<i32> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut components: Vec<Vec<String>> = Vec::new();

    for start in 0..n {
        if index[start] != UNVISITED {
            continue;
        }
        let mut work: Vec<(usize, usize)> = Vec::new();
        index[start] = index_counter;
        lowlink[start] = index_counter;
        index_counter += 1;
        stack.push(start);
        on_stack[start] = true;
        work.push((start, 0));

        while let Some(&(v, i)) = work.last() {
            if i < adj[v].len() {
                let w = adj[v][i];
                work.last_mut().unwrap().1 = i + 1;
                if index[w] == UNVISITED {
                    index[w] = index_counter;
                    lowlink[w] = index_counter;
                    index_counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                if lowlink[v] == index[v] {
                    let mut component: Vec<String> = Vec::new();
                    loop {
                        let w = stack.pop().expect("stack underflow");
                        on_stack[w] = false;
                        component.push(nodes[w].to_string());
                        if w == v {
                            break;
                        }
                    }
                    components.push(component);
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    components
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
}
