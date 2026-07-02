//! `project_overview`: a one-call orientation snapshot for an agent starting
//! work on a project. Pure aggregation over already-indexed facts — no new
//! analysis, no semantic search. Bounded by construction (every list is
//! capped) so the response stays a digest, not a dump.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{
    EntrypointSummary, FeatureKind, FeatureKindCount, FreshnessInfo, Language, LanguageStat,
    ProjectOverview, SuggestedCall, TrustBoundaryCount,
};
use codesage_storage::Database;

use crate::drift::{self, DriftKind};

/// Cap on entrypoints and top-risk files surfaced in the overview.
const ENTRYPOINT_CAP: usize = 15;
const TOP_RISK_CAP: usize = 10;

/// Build the bounded project overview from the index and git state.
pub fn build_project_overview(root: &Path, db: &Database) -> Result<ProjectOverview> {
    let files = db.all_files_with_id_and_language()?;

    // Per-language file counts, descending.
    let mut lang_counts: HashMap<Language, usize> = HashMap::new();
    for (_, _, lang) in &files {
        *lang_counts.entry(*lang).or_insert(0) += 1;
    }
    let mut languages: Vec<LanguageStat> = lang_counts
        .into_iter()
        .map(|(language, file_count)| LanguageStat {
            language,
            file_count,
        })
        .collect();
    languages.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.language.as_str().cmp(b.language.as_str()))
    });

    let freshness = build_freshness(root, db);

    // Features: grouped counts + a sample of entrypoints.
    let features = db.list_features(None, None, None, 0)?;
    let feature_count = features.len();
    let mut kind_counts: HashMap<FeatureKind, usize> = HashMap::new();
    for f in &features {
        *kind_counts.entry(f.kind).or_insert(0) += 1;
    }
    let mut feature_summary: Vec<FeatureKindCount> = kind_counts
        .into_iter()
        .map(|(kind, count)| FeatureKindCount { kind, count })
        .collect();
    feature_summary.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });

    let entrypoints: Vec<EntrypointSummary> = features
        .iter()
        .filter(|f| {
            matches!(
                f.kind,
                FeatureKind::CliCommand
                    | FeatureKind::Route
                    | FeatureKind::Service
                    | FeatureKind::Library
            )
        })
        .take(ENTRYPOINT_CAP)
        .map(|f| EntrypointSummary {
            feature_id: f.feature_id.clone(),
            kind: f.kind,
            title: f.title.clone(),
            entry_path: f.entry_path.clone(),
            command: f.entry_command.clone(),
            route: f.entry_route.clone(),
        })
        .collect();

    let top_risk_files = codesage_graph::top_risk_files(db, TOP_RISK_CAP).unwrap_or_default();

    let trust_boundary_clusters: Vec<TrustBoundaryCount> = db
        .trust_boundary_counts()
        .unwrap_or_default()
        .into_iter()
        .map(|(boundary, file_count)| TrustBoundaryCount {
            boundary,
            file_count,
        })
        .collect();

    let test_conventions = test_conventions_for(&languages);
    let suggested_next_calls = suggested_next_calls(&freshness);

    Ok(ProjectOverview {
        project_root: root.display().to_string(),
        languages,
        file_count: db.file_count().unwrap_or(0),
        symbol_count: db.symbol_count().unwrap_or(0),
        freshness,
        feature_summary,
        feature_count,
        top_risk_files,
        trust_boundary_clusters,
        test_conventions,
        entrypoints,
        suggested_next_calls,
    })
}

fn build_freshness(root: &Path, db: &Database) -> FreshnessInfo {
    let report = drift::check_drift(root, db);
    let structural_kind = match report.kind {
        DriftKind::NotGit => "not_git",
        DriftKind::NeverIndexed => "never_indexed",
        DriftKind::Fresh => "fresh",
        DriftKind::BehindHead => "behind_head",
        DriftKind::UnrelatedAncestor => "unrelated_ancestor",
        DriftKind::Unknown => "unknown",
    }
    .to_string();
    let semantic_indexed_files = db.semantic_file_count().unwrap_or(0);
    FreshnessInfo {
        structural_kind,
        structural_summary: report.summary(),
        commits_behind: report.commits_between,
        indexed_sha: report.stored_sha,
        head_sha: report.head_sha,
        semantic_indexed_files,
        semantic_indexed: semantic_indexed_files > 0,
    }
}

/// One test-convention hint per indexed language. Mirrors the sibling
/// conventions `recommend_tests` resolves, so an agent reads the same contract
/// here that the tool enforces.
fn test_conventions_for(languages: &[LanguageStat]) -> Vec<String> {
    languages
        .iter()
        .map(|l| match l.language {
            Language::Php => {
                "PHP: `FooTest.php` siblings; Laravel mirror-tree \
                 `tests/{Unit,Feature,Integration}/<rest>/<File>Test.php`; php-src \
                 `<dir>/tests/*.phpt`"
            }
            Language::Python => "Python: `test_foo.py` / `foo_test.py` (sibling or under `tests/`)",
            Language::Go => "Go: `foo_test.go` sibling",
            Language::Rust => {
                "Rust: integration tests under `<crate>/tests/*.rs`; inline `#[cfg(test)] mod tests`"
            }
            Language::JavaScript => "JavaScript: `foo.test.js` / `foo.spec.js`",
            Language::TypeScript => "TypeScript: `foo.test.ts(x)` / `foo.spec.ts(x)`",
            Language::Java => "Java: `src/test/java` mirror tree, `FooTest.java`",
            Language::C => "C: project `tests/` dir; php-src `<dir>/tests/*.phpt`",
            Language::Cpp => "C++: project `tests/` dir; CMake/ctest targets",
        })
        .map(str::to_string)
        .collect()
}

/// Recommended next CodeSage calls for common intents. The stale-index entry is
/// prepended only when the structural index is actually behind, so the agent is
/// nudged to refresh before trusting structural results.
fn suggested_next_calls(freshness: &FreshnessInfo) -> Vec<SuggestedCall> {
    let mut calls = Vec::new();
    if matches!(
        freshness.structural_kind.as_str(),
        "behind_head" | "unrelated_ancestor"
    ) {
        calls.push(SuggestedCall {
            intent: "index may be stale".to_string(),
            tool: "codesage index (CLI)".to_string(),
            why: format!(
                "{} — structural results may not reflect the working tree",
                freshness.structural_summary
            ),
        });
    }
    calls.extend([
        SuggestedCall {
            intent: "find code by behavior or symptom".to_string(),
            tool: "search".to_string(),
            why: "intent query when you don't know the exact symbol name".to_string(),
        },
        SuggestedCall {
            intent: "locate an exact symbol".to_string(),
            tool: "find_symbol".to_string(),
            why: "definition by name; cheaper and more precise than grep".to_string(),
        },
        SuggestedCall {
            intent: "review or modify a whole feature".to_string(),
            tool: "feature_bundle".to_string(),
            why: "curated entry+owned+tests+context bundle for a feature_id".to_string(),
        },
        SuggestedCall {
            intent: "before editing a file".to_string(),
            tool: "assess_risk".to_string(),
            why: "churn/fix/blast/coupling/test-gap/trust-boundary score to size the patch"
                .to_string(),
        },
        SuggestedCall {
            intent: "what breaks if I change this".to_string(),
            tool: "impact_analysis".to_string(),
            why: "reverse blast radius across the reference graph".to_string(),
        },
        SuggestedCall {
            intent: "before committing a patch".to_string(),
            tool: "review_rehearsal".to_string(),
            why: "predicted review objections (risk, test gaps, cycles, boundaries) for the diff"
                .to_string(),
        },
    ]);
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE_TOOL: &str = "codesage index (CLI)";

    fn freshness(kind: &str) -> FreshnessInfo {
        FreshnessInfo {
            structural_kind: kind.to_string(),
            structural_summary: format!("summary for {kind}"),
            commits_behind: None,
            indexed_sha: None,
            head_sha: None,
            semantic_indexed_files: 0,
            semantic_indexed: false,
        }
    }

    #[test]
    fn behind_head_prepends_stale_index_nudge() {
        let calls = suggested_next_calls(&freshness("behind_head"));
        assert_eq!(
            calls.first().map(|c| c.tool.as_str()),
            Some(STALE_TOOL),
            "stale nudge must come first so the agent refreshes before trusting results"
        );
        assert!(
            calls[0].why.contains("summary for behind_head"),
            "why should carry the drift summary, got {:?}",
            calls[0].why
        );
    }

    #[test]
    fn unrelated_ancestor_prepends_stale_index_nudge() {
        let calls = suggested_next_calls(&freshness("unrelated_ancestor"));
        assert_eq!(calls.first().map(|c| c.tool.as_str()), Some(STALE_TOOL));
    }

    #[test]
    fn fresh_index_omits_stale_index_nudge() {
        let calls = suggested_next_calls(&freshness("fresh"));
        assert!(
            calls.iter().all(|c| c.tool != STALE_TOOL),
            "fresh index must not nudge a reindex, got {:?}",
            calls.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(
            calls.first().map(|c| c.tool.as_str()),
            Some("search"),
            "baseline suggestions should lead with search when nothing is stale"
        );
    }
}
