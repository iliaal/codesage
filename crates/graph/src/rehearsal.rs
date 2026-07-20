//! `review_rehearsal`: predict the objections a reviewer would likely raise
//! against a patch, before it is committed. Pure composition over shipped
//! primitives — `assess_risk_diff`, `recommend_tests`, drift, and feature
//! mapping — so there is no duplicated analysis logic here, only the glue that
//! turns those signals into severity-ranked objections.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;

use crate::git_history::{assess_risk_diff, recommend_tests};
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

/// A patch touching at least this many distinct feature *areas* (entry
/// directories) reads as scattered — worth confirming the change set is
/// intentional. Tuned against real history on php-src + a Laravel backend so a
/// focused deep change (which concentrates in 1–2 areas) does not fire.
const SCOPE_SPREAD_THRESHOLD: usize = 4;

/// Cap on per-area evidence lines in the scope-spread objection.
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
    let detailed_risk: Vec<&codesage_protocol::RiskAssessment> = risk
        .files
        .iter()
        .chain(
            risk.clustered_directories
                .iter()
                .flat_map(|cluster| cluster.top_files.iter()),
        )
        .collect();
    let by_file: HashMap<&str, &codesage_protocol::RiskAssessment> = detailed_risk
        .iter()
        .map(|a| (a.file.as_str(), *a))
        .collect();

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

    // High / elevated risk files, from every detailed risk entry.
    let mut high: Vec<&codesage_protocol::RiskAssessment> = Vec::new();
    let mut elevated: Vec<&codesage_protocol::RiskAssessment> = Vec::new();
    for &a in &detailed_risk {
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
    // Scope accumulation keyed by the feature's entry *directory*, not
    // feature_id. php-src maps every PHP function/method as its own feature
    // sharing one source file, so a single-file change can touch 60+ features
    // in one directory — counting feature ids would scream "scattered" on a
    // focused patch. Entry directory is the area-level locus that actually
    // signals scatter.
    let mut area_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut orphan_files: Vec<String> = Vec::new();
    for f in files {
        let feats = db.features_for_file(f).unwrap_or_default();
        if feats.is_empty() {
            orphan_files.push(f.clone());
        }
        for feat in &feats {
            area_files
                .entry(entry_area(&feat.entry_path))
                .or_default()
                .insert(f.clone());
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
    if area_files.len() >= SCOPE_SPREAD_THRESHOLD {
        let mut evidence: Vec<String> = area_files
            .iter()
            .take(SCOPE_EVIDENCE_CAP)
            .map(|(area, fs)| {
                let patch_files = fs.iter().cloned().collect::<Vec<_>>().join(", ");
                format!("{area}/: {patch_files}")
            })
            .collect();
        if area_files.len() > SCOPE_EVIDENCE_CAP {
            evidence.push(format!(
                "… and {} more area(s)",
                area_files.len() - SCOPE_EVIDENCE_CAP
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
                "Patch spans {} feature areas — confirm this is one intentional change, not unrelated edits bundled together",
                area_files.len()
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

/// The "area" of a feature for scope-spread accounting: the directory of its
/// entry file. Collapses many fine-grained features that share one source file
/// (php-src maps each PHP function as its own feature) into a single locus, so
/// the signal measures scatter across the tree rather than function density.
fn entry_area(entry_path: &str) -> String {
    std::path::Path::new(entry_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{
        FeatureConfidence, FeatureFileRef, FeatureKind, FeatureRecord, FileInfo, Language,
    };

    fn file_ref(path: &str, role: FeatureFileRole) -> FeatureFileRef {
        FeatureFileRef {
            path: path.to_string(),
            role,
            reason: None,
        }
    }

    fn feature(id: &str, entry_path: &str, files: Vec<FeatureFileRef>) -> FeatureRecord {
        FeatureRecord {
            feature_id: id.to_string(),
            title: format!("feature {id}"),
            summary: String::new(),
            kind: FeatureKind::Library,
            source: "test".to_string(),
            confidence: FeatureConfidence::High,
            entry_path: entry_path.to_string(),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            test_command: None,
            language: Language::Php,
            tags: Vec::new(),
            trust_boundaries: Vec::new(),
            files: vec![file_ref(entry_path, FeatureFileRole::Entry)]
                .into_iter()
                .chain(files)
                .collect(),
        }
    }

    fn index_rust_file(db: &Database, path: &str) {
        db.upsert_file(&FileInfo {
            path: path.to_string(),
            language: Language::Rust,
            content_hash: format!("test-hash-{path}"),
        })
        .unwrap();
    }

    #[test]
    fn high_risk_objection_includes_clustered_top_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let clustered: Vec<String> = (0..5).map(|i| format!("app/Risk/File{i}.php")).collect();
        let hot_id = db
            .upsert_file(&FileInfo {
                path: "app/Risk/File0.php".to_string(),
                language: Language::Php,
                content_hash: "hot".to_string(),
            })
            .unwrap();
        db.replace_file_trust_boundaries(
            hot_id,
            &[
                TrustBoundary::Network,
                TrustBoundary::Filesystem,
                TrustBoundary::Database,
                TrustBoundary::Secrets,
                TrustBoundary::ProcessExec,
            ],
        )
        .unwrap();
        for (i, path) in clustered.iter().enumerate() {
            db.upsert_git_file(
                path,
                if i == 0 { 100.0 } else { 0.1 },
                if i == 0 { 40 } else { 0 },
                if i == 0 { 80 } else { 5 },
                Some(1_700_000_000),
            )
            .unwrap();
        }
        for path in ["cold_a.php", "cold_b.php", "cold_c.php"] {
            db.upsert_git_file(path, 0.05, 0, 5, Some(1_700_000_000))
                .unwrap();
        }

        let risk = crate::git_history::assess_risk_diff(&db, &clustered).unwrap();
        assert!(
            risk.files.iter().all(|a| a.file != "app/Risk/File0.php"),
            "fixture must prove File0 only survives in clustered top_files"
        );
        assert!(
            risk.clustered_directories
                .iter()
                .flat_map(|c| c.top_files.iter())
                .any(|a| a.file == "app/Risk/File0.php" && a.score >= 0.60),
            "fixture should keep a high-risk clustered top file, got {risk:?}"
        );

        let r = build_review_rehearsal(dir.path(), &db, &clustered).unwrap();
        let obj = r
            .objections
            .iter()
            .find(|o| o.category == "high-risk-file" && o.severity == ReviewSeverity::High)
            .unwrap_or_else(|| panic!("expected high-risk-file objection, got {:?}", r.objections));

        assert!(
            obj.files.contains(&"app/Risk/File0.php".to_string()),
            "high-risk objection must include clustered top_files, got {obj:?}"
        );
    }

    #[test]
    fn scope_spread_keys_on_entry_directory_not_feature_id() {
        // php-src regression shape: one source file owns 60+ features (each
        // PHP function is its own feature), all sharing one entry directory.
        // Counting feature_ids would fire scope-spread on a focused one-file
        // patch; keying on the entry directory must not.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let changed = "ext/standard/array.c";
        for i in 0..SCOPE_SPREAD_THRESHOLD + 2 {
            db.upsert_feature(&feature(&format!("feat_{i:016x}"), changed, Vec::new()))
                .unwrap();
        }

        let r = build_review_rehearsal(dir.path(), &db, &[changed.to_string()]).unwrap();
        assert!(
            r.objections.iter().all(|o| o.category != "scope-spread"),
            "many feature_ids in ONE entry directory must not read as scatter, got {:?}",
            r.objections
                .iter()
                .map(|o| (&o.category, &o.title))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn scope_spread_fires_across_distinct_entry_areas() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut changed: Vec<String> = Vec::new();
        for i in 0..SCOPE_SPREAD_THRESHOLD {
            let entry = format!("area{i}/mod.rs");
            db.upsert_feature(&feature(&format!("feat_area_{i}"), &entry, Vec::new()))
                .unwrap();
            changed.push(entry);
        }

        let r = build_review_rehearsal(dir.path(), &db, &changed).unwrap();
        let obj = r
            .objections
            .iter()
            .find(|o| o.category == "scope-spread")
            .unwrap_or_else(|| {
                panic!(
                    "{SCOPE_SPREAD_THRESHOLD} distinct entry areas must fire scope-spread, got {:?}",
                    r.objections
                        .iter()
                        .map(|o| (&o.category, &o.title))
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(obj.severity, ReviewSeverity::Low);
        assert!(
            obj.title
                .contains(&format!("{SCOPE_SPREAD_THRESHOLD} feature areas")),
            "title should name the area count, got {:?}",
            obj.title
        );
    }

    #[test]
    fn review_severity_ord_ranks_high_before_medium_before_low() {
        // The objection sort relies on ReviewSeverity's derived Ord, which in
        // turn relies on declaration order in protocol. Pin the contract so a
        // careless reorder or extension of the enum fails here.
        assert!(ReviewSeverity::High < ReviewSeverity::Medium);
        assert!(ReviewSeverity::Medium < ReviewSeverity::Low);
        assert!(ReviewSeverity::High < ReviewSeverity::Low);
    }

    #[test]
    fn objections_are_sorted_high_to_low() {
        // Scenario producing all three severities:
        //   High   missing-tests (no sibling or coupled tests anywhere)
        //   Medium feature-test-gap (feature has a mapped test not in the patch)
        //   Low    scope-spread (>= threshold distinct entry areas)
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut changed: Vec<String> = Vec::new();
        for i in 0..SCOPE_SPREAD_THRESHOLD {
            let entry = format!("area{i}/mod.rs");
            let tests = vec![file_ref(
                &format!("area{i}/tests/it.rs"),
                FeatureFileRole::Test,
            )];
            index_rust_file(&db, &entry);
            db.upsert_feature(&feature(&format!("feat_area_{i}"), &entry, tests))
                .unwrap();
            changed.push(entry);
        }

        let r = build_review_rehearsal(dir.path(), &db, &changed).unwrap();
        let severities: Vec<ReviewSeverity> = r.objections.iter().map(|o| o.severity).collect();
        assert!(
            severities.contains(&ReviewSeverity::High)
                && severities.contains(&ReviewSeverity::Medium)
                && severities.contains(&ReviewSeverity::Low),
            "fixture should produce all three severities, got {severities:?}"
        );
        assert_eq!(
            severities.first(),
            Some(&ReviewSeverity::High),
            "highest severity must lead"
        );
        assert_eq!(
            severities.last(),
            Some(&ReviewSeverity::Low),
            "lowest severity must trail"
        );
        assert!(
            severities.windows(2).all(|w| w[0] <= w[1]),
            "objections must be sorted high → low, got {severities:?}"
        );
    }
}
