//! Query-side risk + coupling over the `git_files` / `git_co_changes` tables.

use anyhow::{Context, Result};
use codesage_protocol::{
    ClusteredDirectory, CoChangeEntry, CouplingReport, FileCategory, ImpactRequest, ImpactTarget,
    RiskAssessment, RiskDiffAssessment,
};
use codesage_storage::Database;

use super::tests_rec::test_sibling_exists;
use crate::query::impact_analysis;

/// Top-N files that historically co-change with `file_path`, wrapped in a
/// report that explains empty results. See [`CouplingReport`] for the
/// disambiguation an agent needs: was the file never indexed, does it have
/// history but no pair above the co-change threshold, or was the path wrong.
///
/// Schema change from the pre-0.4.1 `Vec<CoChangeEntry>` return type: callers
/// that read the MCP `find_coupling` response should now index into
/// `result.coupled` instead of treating the result as a bare array.
pub fn find_coupling(db: &Database, file_path: &str, limit: usize) -> Result<CouplingReport> {
    let rows = db.co_changes_for(file_path, limit)?;
    let coupled: Vec<CoChangeEntry> = rows
        .into_iter()
        .map(|r| CoChangeEntry {
            file: r.file,
            weight: r.weight,
            count: r.count,
            last_observed_at: r.last_observed_at,
        })
        .collect();

    let git = db.git_file(file_path)?;
    let file_indexed = git.is_some();
    let file_commits = git.as_ref().map(|g| g.total_commits).unwrap_or(0);

    // Note is generated only when `coupled` is empty. Distinguishes the three
    // dominant causes so an agent can decide whether to retry, try a
    // different tool, or warn the user that the index needs a refresh.
    let note = if !coupled.is_empty() {
        None
    } else if !file_indexed {
        Some(
            "file has no git history (not tracked by git, no commits yet, or path shape \
             does not match the index — verify with `codesage status` or \
             `codesage git-index --full`)"
                .to_string(),
        )
    } else if file_commits < 3 {
        Some(format!(
            "file has only {file_commits} tracked commit(s); co-change pairs need a \
             count of 3+ to be recorded (see `codesage git-index --full` to rebaseline)"
        ))
    } else {
        Some(format!(
            "file has {file_commits} commits but no co-change pair crosses the min-count \
             threshold of 3; this file typically changes in isolation"
        ))
    };

    Ok(CouplingReport {
        coupled,
        file_indexed,
        file_commits,
        note,
    })
}

/// Risk score for a single file. Composes:
/// - churn percentile (0..1) — weight 0.35
/// - fix ratio (fix_count / total_commits, capped at 1.0) — weight 0.20
/// - dependent file pressure (capped via 20 dependents) — weight 0.15
/// - coupled file pressure (capped via 10 coupled) — weight 0.15
/// - test gap (no test among coupled or as adjacent file) — weight 0.15
///
/// Output includes the decomposition so the agent can quote specific signals
/// in PR descriptions or risk callouts. Empty git history → score=0 with a note.
pub fn assess_risk(db: &Database, file_path: &str) -> Result<RiskAssessment> {
    let git = db.git_file(file_path)?;
    let churn_score = git.as_ref().map(|g| g.churn_score).unwrap_or(0.0);
    let total_commits = git.as_ref().map(|g| g.total_commits).unwrap_or(0);
    let fix_count = git.as_ref().map(|g| g.fix_count).unwrap_or(0);
    let churn_percentile = db.churn_percentile(file_path)?;
    let fix_ratio = if total_commits > 0 {
        (fix_count as f64 / total_commits as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let coupled = db.co_changes_for(file_path, 10)?;
    let coupled_files = coupled.len() as u32;
    let top_coupled: Vec<CoChangeEntry> = coupled
        .into_iter()
        .map(|r| CoChangeEntry {
            file: r.file,
            weight: r.weight,
            count: r.count,
            last_observed_at: r.last_observed_at,
        })
        .collect();

    // Reverse-dependency pressure via existing impact analysis (depth=2).
    let dependent_files = impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: file_path.to_string(),
            },
            depth: 2,
            source_only: true,
        },
    )
    .with_context(|| format!("computing dependent_files for risk({file_path})"))?
    .len() as u32;

    // Test gap: do any coupled files look like tests, or does a sibling file matching
    // common test conventions exist in the index?
    let has_coupled_test = top_coupled
        .iter()
        .any(|e| matches!(FileCategory::classify(&e.file), FileCategory::Test));
    let has_sibling_test = test_sibling_exists(db, file_path)
        .with_context(|| format!("checking sibling test for risk({file_path})"))?;
    let test_gap = !has_coupled_test && !has_sibling_test;

    let dep_pressure = (dependent_files as f64 / 20.0).min(1.0);
    let coup_pressure = (coupled_files as f64 / 10.0).min(1.0);
    let test_gap_term = if test_gap { 1.0 } else { 0.0 };

    let score = 0.35 * churn_percentile
        + 0.20 * fix_ratio
        + 0.15 * dep_pressure
        + 0.15 * coup_pressure
        + 0.15 * test_gap_term;

    let mut notes = Vec::new();
    if git.is_none() {
        notes.push(
            "no git history for this file (file too new, or `codesage git-index` hasn't been run)"
                .to_string(),
        );
    }
    if churn_percentile >= 0.75 {
        notes.push(format!(
            "hotspot: churn percentile {:.0}%",
            churn_percentile * 100.0
        ));
    }
    if fix_ratio >= 0.4 && total_commits >= 5 {
        notes.push(format!(
            "fix-heavy: {fix_count}/{total_commits} commits ({:.0}%) tagged as fixes",
            fix_ratio * 100.0
        ));
    }
    if dependent_files >= 10 {
        notes.push(format!(
            "wide blast radius: {dependent_files} files depend on this (depth-2)"
        ));
    }
    if coupled_files >= 5 {
        notes.push(format!(
            "high coupling: {coupled_files} files historically change with this"
        ));
    }
    if test_gap {
        notes.push("test gap: no obvious test file (sibling or co-changer)".to_string());
    }

    Ok(RiskAssessment {
        file: file_path.to_string(),
        score,
        churn_score,
        churn_percentile,
        fix_ratio,
        total_commits,
        fix_count,
        dependent_files,
        coupled_files,
        test_gap,
        top_coupled,
        notes,
    })
}

/// Aggregate `assess_risk` across the file list of a patch. Lets an agent
/// ask one question instead of N round-trips. Output exposes both the
/// per-file decomposition and patch-level rollups (max/mean, files in each
/// risk category, paste-ready summary notes).
pub fn assess_risk_diff(db: &Database, file_paths: &[String]) -> Result<RiskDiffAssessment> {
    if file_paths.is_empty() {
        return Ok(RiskDiffAssessment::default());
    }

    let files: Vec<RiskAssessment> = file_paths
        .iter()
        .map(|p| assess_risk(db, p))
        .collect::<Result<Vec<_>>>()?;

    let max_score = files.iter().map(|f| f.score).fold(0.0_f64, f64::max);
    let mean_score = files.iter().map(|f| f.score).sum::<f64>() / files.len() as f64;
    let max_risk_file = files
        .iter()
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.file.clone());

    let test_gap_files: Vec<String> = files
        .iter()
        .filter(|f| f.test_gap)
        .map(|f| f.file.clone())
        .collect();

    let wide_blast_files: Vec<String> = files
        .iter()
        .filter(|f| f.dependent_files >= 10)
        .map(|f| f.file.clone())
        .collect();

    let fix_heavy_files: Vec<String> = files
        .iter()
        .filter(|f| f.fix_ratio >= 0.4 && f.total_commits >= 5)
        .map(|f| f.file.clone())
        .collect();

    let hotspot_files: Vec<String> = files
        .iter()
        .filter(|f| f.churn_percentile >= 0.75)
        .map(|f| f.file.clone())
        .collect();

    let mut summary_notes = Vec::new();
    if !hotspot_files.is_empty() {
        summary_notes.push(format!(
            "patch touches {} hotspot file(s)",
            hotspot_files.len()
        ));
    }
    if !fix_heavy_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) historically fix-heavy",
            fix_heavy_files.len()
        ));
    }
    if !test_gap_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) lack test coverage (no sibling test, no test in co-change history)",
            test_gap_files.len()
        ));
    }
    if !wide_blast_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) have wide blast radius (>=10 dependents)",
            wide_blast_files.len()
        ));
    }
    if max_score >= 0.6 {
        summary_notes.push(format!(
            "max risk score {max_score:.2}; consider smaller patch and broader test sweep"
        ));
    }

    let (files, clustered_directories) = cluster_by_directory(files, DIR_CLUSTER_THRESHOLD);

    Ok(RiskDiffAssessment {
        files,
        max_score,
        mean_score,
        max_risk_file,
        test_gap_files,
        wide_blast_files,
        fix_heavy_files,
        hotspot_files,
        summary_notes,
        clustered_directories,
    })
}

/// When a patch touches at least this many files in a single directory, the
/// per-file detail for that directory is condensed into a `ClusteredDirectory`
/// entry. Measured on real 30-day session logs: `assess_risk_diff` responses
/// at p95 were 24 KB and saved ~13% with this rule; smaller patches are
/// untouched so agent prompts built against the flat shape keep working.
const DIR_CLUSTER_THRESHOLD: usize = 5;

/// Group `files` by their parent directory. Any directory with
/// `>= threshold` entries is collapsed to a `ClusteredDirectory` whose
/// `top_files` keep full detail for the three highest-scoring files and
/// whose `omitted_files` lists the rest by name. Directories below the
/// threshold are returned unchanged in the first tuple element.
fn cluster_by_directory(
    files: Vec<RiskAssessment>,
    threshold: usize,
) -> (Vec<RiskAssessment>, Vec<ClusteredDirectory>) {
    use std::collections::BTreeMap;

    // Bucket by parent directory, preserving insertion order inside each bucket.
    let mut buckets: BTreeMap<String, Vec<RiskAssessment>> = BTreeMap::new();
    for f in files {
        let dir = std::path::Path::new(&f.file)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        buckets.entry(dir).or_default().push(f);
    }

    let mut kept: Vec<RiskAssessment> = Vec::new();
    let mut clusters: Vec<ClusteredDirectory> = Vec::new();
    for (dir, mut items) in buckets {
        if items.len() < threshold {
            kept.extend(items);
            continue;
        }
        // Sort by risk score descending so top-3 are the highest.
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let count = items.len() as u32;
        let top_files: Vec<RiskAssessment> = items.iter().take(3).cloned().collect();
        let omitted_files: Vec<String> = items.iter().skip(3).map(|f| f.file.clone()).collect();
        clusters.push(ClusteredDirectory {
            directory: dir,
            count,
            top_files,
            omitted_files,
        });
    }
    (kept, clusters)
}
