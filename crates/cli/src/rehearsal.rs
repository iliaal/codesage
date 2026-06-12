//! `review_rehearsal`: predict the objections a reviewer would likely raise
//! against a patch, before it is committed. Pure composition over shipped
//! primitives — `assess_risk_diff`, `recommend_tests`, drift, and feature
//! mapping — so there is no duplicated analysis logic here, only the glue that
//! turns those signals into severity-ranked objections.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use codesage_graph::{assess_risk_diff, recommend_tests};
use codesage_protocol::{
    FeatureFileRole, ReviewObjection, ReviewRehearsal, ReviewSeverity, TrustBoundary,
};
use codesage_storage::Database;

use crate::drift::{self, DriftKind};

/// A file crossing at least this many trust boundaries warrants a security note
/// (matches the `assess_risk` threshold).
const TRUST_BOUNDARY_THRESHOLD: usize = 3;

/// Hottest symbols to surface per high-risk file.
const HOTSPOT_EVIDENCE_CAP: usize = 3;

/// A patch touching at least this many distinct feature slices reads as
/// scattered — worth confirming the change set is intentional. Tuned to fire on
/// genuine scope creep, not a focused deep change (which concentrates in 1–2
/// features).
const SCOPE_SPREAD_THRESHOLD: usize = 4;

/// Cap on per-slice evidence lines in the scope-spread objection.
const SCOPE_EVIDENCE_CAP: usize = 10;

pub fn build_review_rehearsal(
    root: &Path,
    db: &Database,
    files: &[String],
) -> Result<ReviewRehearsal> {
    if files.is_empty() {
        return Ok(ReviewRehearsal {
            files: Vec::new(),
            objections: Vec::new(),
            summary_notes: vec![
                "No files supplied — pass the patch's file list (e.g. `git diff --name-only`)."
                    .to_string(),
            ],
        });
    }

    let mut objections: Vec<ReviewObjection> = Vec::new();

    // --- stale index (a caveat, not a code defect) ---
    let report = drift::check_drift(root, db);
    if matches!(
        report.kind,
        DriftKind::BehindHead | DriftKind::UnrelatedAncestor
    ) {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Medium,
            category: "stale-index".to_string(),
            title: "Index may be stale; risk and impact signals can be inaccurate".to_string(),
            evidence: vec![
                report.summary(),
                "Re-run `codesage index` before trusting this rehearsal".to_string(),
            ],
            files: Vec::new(),
        });
    }

    // --- risk rollup for the patch ---
    let risk = assess_risk_diff(db, files)?;
    let by_file: HashMap<&str, &codesage_protocol::RiskAssessment> =
        risk.files.iter().map(|a| (a.file.as_str(), a)).collect();

    if !risk.test_gap_files.is_empty() {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::High,
            category: "missing-tests".to_string(),
            title: format!(
                "{} changed file(s) have no sibling or coupled tests",
                risk.test_gap_files.len()
            ),
            evidence: vec![format!("test-gap: {}", risk.test_gap_files.join(", "))],
            files: risk.test_gap_files.clone(),
        });
    }

    // High / elevated risk files, from the detailed (non-clustered) entries.
    let mut high: Vec<&codesage_protocol::RiskAssessment> = Vec::new();
    let mut elevated: Vec<&codesage_protocol::RiskAssessment> = Vec::new();
    for a in &risk.files {
        if a.score >= 0.60 {
            high.push(a);
        } else if a.score >= 0.40 {
            elevated.push(a);
        }
    }
    if !high.is_empty() {
        // For each high-risk file, the score line followed by its hottest
        // symbols (already computed by `assess_risk`) so the reviewer knows
        // which function to read first. `why` reflects structural load
        // (size × references × cycle), not edit frequency.
        let mut evidence = Vec::new();
        for a in &high {
            evidence.push(format!("{} (score {:.2})", a.file, a.score));
            for s in a.top_symbols.iter().take(HOTSPOT_EVIDENCE_CAP) {
                evidence.push(format!(
                    "  hot symbol: {} @ line {} ({})",
                    s.name, s.line, s.why
                ));
            }
        }
        objections.push(ReviewObjection {
            severity: ReviewSeverity::High,
            category: "high-risk-file".to_string(),
            title: format!(
                "{} high-risk file(s) in the patch (score ≥ 0.60)",
                high.len()
            ),
            evidence,
            files: high.iter().map(|a| a.file.clone()).collect(),
        });
    }
    if !elevated.is_empty() {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Medium,
            category: "high-risk-file".to_string(),
            title: format!(
                "{} elevated-risk file(s) in the patch (score 0.40–0.60)",
                elevated.len()
            ),
            evidence: elevated
                .iter()
                .map(|a| format!("{} (score {:.2})", a.file, a.score))
                .collect(),
            files: elevated.iter().map(|a| a.file.clone()).collect(),
        });
    }

    if !risk.wide_blast_files.is_empty() {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Medium,
            category: "blast-radius".to_string(),
            title: format!(
                "{} file(s) with wide blast radius (≥10 dependents)",
                risk.wide_blast_files.len()
            ),
            evidence: risk
                .wide_blast_files
                .iter()
                .map(|f| match by_file.get(f.as_str()) {
                    Some(a) => format!("{f} ({} dependents)", a.dependent_files),
                    None => f.clone(),
                })
                .collect(),
            files: risk.wide_blast_files.clone(),
        });
    }

    if !risk.fix_heavy_files.is_empty() {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Medium,
            category: "fix-prone".to_string(),
            title: format!(
                "{} fix-prone file(s) (history shows repeated fixes)",
                risk.fix_heavy_files.len()
            ),
            evidence: risk
                .fix_heavy_files
                .iter()
                .map(|f| match by_file.get(f.as_str()) {
                    Some(a) => format!(
                        "{f} ({} fixes / {} commits, ratio {:.2})",
                        a.fix_count, a.total_commits, a.fix_ratio
                    ),
                    None => f.clone(),
                })
                .collect(),
            files: risk.fix_heavy_files.clone(),
        });
    }

    if !risk.hotspot_files.is_empty() {
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Low,
            category: "hotspot".to_string(),
            title: format!(
                "{} churn hotspot(s) (top-quartile change frequency)",
                risk.hotspot_files.len()
            ),
            evidence: vec![format!("hotspot: {}", risk.hotspot_files.join(", "))],
            files: risk.hotspot_files.clone(),
        });
    }

    for c in &risk.cycles_touching_patch {
        let mut evidence = vec![format!(
            "cycle of {} files: {}",
            c.size,
            c.members.join(" → ")
        )];
        if let Some(target) = &c.max_churn_file {
            evidence.push(format!("best refactor target: {target}"));
        }
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Medium,
            category: "import-cycle".to_string(),
            title: "Patch touches an import cycle".to_string(),
            evidence,
            files: c.members.clone(),
        });
    }

    // --- trust-boundary expansion, queried directly per input file so the
    // signal is complete even when risk detail was clustered away ---
    for f in files {
        let tb = db.trust_boundaries_for_file_path(f).unwrap_or_default();
        if tb.len() >= TRUST_BOUNDARY_THRESHOLD {
            let names: Vec<&str> = tb.iter().map(TrustBoundary::as_str).collect();
            objections.push(ReviewObjection {
                severity: ReviewSeverity::Medium,
                category: "trust-boundary".to_string(),
                title: format!(
                    "{f} crosses {} trust boundaries — security review recommended",
                    tb.len()
                ),
                evidence: vec![format!("boundaries: {}", names.join(", "))],
                files: vec![f.clone()],
            });
        }
    }

    // --- feature mapping: one `features_for_file` pass per input file feeds
    // both the feature-test-gap check and the scope-cohesion check ---
    let input_set: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut seen_features: BTreeSet<String> = BTreeSet::new();
    // Scope accumulation: distinct slices touched, the patch files under each,
    // and files claimed by no feature.
    let mut feature_titles: BTreeMap<String, String> = BTreeMap::new();
    let mut feature_patch_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut orphan_files: Vec<String> = Vec::new();
    for f in files {
        let feats = db.features_for_file(f).unwrap_or_default();
        if feats.is_empty() {
            orphan_files.push(f.clone());
        }
        for feat in &feats {
            feature_titles
                .entry(feat.feature_id.clone())
                .or_insert_with(|| feat.title.clone());
            feature_patch_files
                .entry(feat.feature_id.clone())
                .or_default()
                .push(f.clone());
        }
        for feat in feats {
            let core_change = feat.files.iter().any(|r| {
                r.path == *f && matches!(r.role, FeatureFileRole::Entry | FeatureFileRole::Owned)
            });
            if !core_change {
                continue;
            }
            if !seen_features.insert(feat.feature_id.clone()) {
                continue;
            }
            let test_files: Vec<String> = feat
                .files
                .iter()
                .filter(|r| matches!(r.role, FeatureFileRole::Test))
                .map(|r| r.path.clone())
                .collect();
            if test_files.is_empty() {
                continue;
            }
            let touches_test = test_files.iter().any(|t| input_set.contains(t.as_str()));
            if !touches_test {
                objections.push(ReviewObjection {
                    severity: ReviewSeverity::Medium,
                    category: "feature-test-gap".to_string(),
                    title: format!(
                        "Patch changes feature \"{}\" but none of its tests",
                        feat.title
                    ),
                    evidence: vec![format!("feature tests: {}", test_files.join(", "))],
                    files: test_files,
                });
            }
        }
    }

    // --- scope cohesion: a patch spread thin across many unrelated slices reads
    // as scope creep. Deterministic proxy for "intent mismatch" — we can't read
    // the PR's stated intent, so we flag the spread and ask for confirmation.
    // Low severity: an attention prompt, never a defect. ---
    if feature_titles.len() >= SCOPE_SPREAD_THRESHOLD {
        let mut evidence: Vec<String> = feature_titles
            .iter()
            .take(SCOPE_EVIDENCE_CAP)
            .map(|(id, title)| {
                let patch_files = feature_patch_files
                    .get(id)
                    .map(|v| v.join(", "))
                    .unwrap_or_default();
                format!("{title}: {patch_files}")
            })
            .collect();
        if feature_titles.len() > SCOPE_EVIDENCE_CAP {
            evidence.push(format!(
                "… and {} more slice(s)",
                feature_titles.len() - SCOPE_EVIDENCE_CAP
            ));
        }
        if !orphan_files.is_empty() {
            evidence.push(format!(
                "{} file(s) belong to no mapped feature: {}",
                orphan_files.len(),
                orphan_files.join(", ")
            ));
        }
        objections.push(ReviewObjection {
            severity: ReviewSeverity::Low,
            category: "scope-spread".to_string(),
            title: format!(
                "Patch spans {} feature slices — confirm this is one intentional change, not unrelated edits bundled together",
                feature_titles.len()
            ),
            evidence,
            files: orphan_files,
        });
    }

    // High first, then by category for stable output.
    objections.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.category.cmp(&b.category))
    });

    let summary_notes = build_summary(db, files, &risk, &objections);

    Ok(ReviewRehearsal {
        files: files.to_vec(),
        objections,
        summary_notes,
    })
}

fn build_summary(
    db: &Database,
    files: &[String],
    risk: &codesage_protocol::RiskDiffAssessment,
    objections: &[ReviewObjection],
) -> Vec<String> {
    let mut notes = Vec::new();

    let (mut h, mut m, mut l) = (0usize, 0usize, 0usize);
    for o in objections {
        match o.severity {
            ReviewSeverity::High => h += 1,
            ReviewSeverity::Medium => m += 1,
            ReviewSeverity::Low => l += 1,
        }
    }
    if objections.is_empty() {
        notes.push("No predicted review objections for this patch.".to_string());
    } else {
        notes.push(format!(
            "{} objection(s): {h} high, {m} medium, {l} low",
            objections.len()
        ));
    }

    notes.extend(risk.summary_notes.iter().cloned());

    if let Ok(tests) = recommend_tests(db, files) {
        if !tests.primary.is_empty() {
            notes.push(format!("Run tests: {}", tests.primary.join(", ")));
        } else if !tests.coupled.is_empty() {
            let coupled: Vec<String> = tests.coupled.iter().map(|c| c.file.clone()).collect();
            notes.push(format!("Coupled tests to consider: {}", coupled.join(", ")));
        }
    }

    notes
}
