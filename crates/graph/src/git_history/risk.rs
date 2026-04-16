//! Query-side risk + coupling over the `git_files` / `git_co_changes` tables.

use anyhow::{Context, Result};
use codesage_protocol::{
    CoChangeEntry, FileCategory, ImpactRequest, ImpactTarget, RiskAssessment, RiskDiffAssessment,
};
use codesage_storage::Database;

use super::tests_rec::test_sibling_exists;
use crate::query::impact_analysis;

/// Top-N files that historically co-change with `file_path`. Returns the OTHER file in
/// each pair, weight-sorted descending. Backed by the `git_co_changes` table populated
/// by `git_history_index`.
pub fn find_coupling(db: &Database, file_path: &str, limit: usize) -> Result<Vec<CoChangeEntry>> {
    let rows = db.co_changes_for(file_path, limit)?;
    Ok(rows
        .into_iter()
        .map(|r| CoChangeEntry {
            file: r.file,
            weight: r.weight,
            count: r.count,
            last_observed_at: r.last_observed_at,
        })
        .collect())
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
    })
}
