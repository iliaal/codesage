//! Bounded project orientation from indexed facts and Git state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{
    EntrypointSummary, FeatureKind, FeatureKindCount, FeatureRecord, FreshnessInfo, Handle,
    Language, LanguageStat, OverviewHotSymbol, OverviewModule, OverviewRecovery, ProjectOverview,
    Symbol, TrustBoundaryCount,
};
use codesage_storage::Database;

use crate::drift::{self, DriftKind};

const ENTRYPOINT_CAP: usize = 15;
const TOP_RISK_CAP: usize = 10;

pub fn build_project_overview(root: &Path, db: &Database) -> Result<ProjectOverview> {
    build_project_overview_with_options(root, db, false)
}

/// `include_tests` ranks test files alongside product code in
/// `top_risk_files`; the default ranking excludes them.
pub fn build_project_overview_with_options(
    root: &Path,
    db: &Database,
    include_tests: bool,
) -> Result<ProjectOverview> {
    let read = db.read_snapshot()?;
    let ranking = crate::session::top_risk_ranking_with_options(
        db,
        crate::search::env_default_on("CODESAGE_COUPLING_RECURRENCE"),
        include_tests,
    )?;
    let overview = build_project_overview_with_top_risk(root, db, &ranking)?;
    drop(read);
    Ok(overview)
}

pub fn build_project_overview_with_top_risk(
    root: &Path,
    db: &Database,
    ranking: &crate::session::CompleteRiskRanking,
) -> Result<ProjectOverview> {
    codesage_protocol::work::checkpoint()?;
    let files = db.all_files_with_id_and_language()?;

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

    let paths: Vec<_> = files.iter().map(|(_, path, _)| path.clone()).collect();
    let mut symbols = HashMap::new();
    for batch in paths.chunks(500) {
        codesage_protocol::work::checkpoint()?;
        symbols.extend(db.symbols_for_files(batch)?);
    }
    let flags: HashMap<_, _> = db
        .all_file_test_flags()?
        .into_iter()
        .map(|(path, stored, interpretation)| {
            let test = crate::index::file_is_test(&path, stored, interpretation.as_deref());
            (path, test)
        })
        .collect();
    let names: Vec<_> = symbols
        .values()
        .flatten()
        .map(|s| s.name.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut counts = HashMap::new();
    for batch in names.chunks(500) {
        codesage_protocol::work::checkpoint()?;
        counts.extend(db.reference_counts_for_names(batch)?);
    }
    let module_map = module_map(root, &files, &symbols, &flags)?;
    let mut hot: Vec<_> = symbols
        .iter()
        .filter(|(path, _)| !flags.get(*path).copied().unwrap_or(false))
        .flat_map(|(_, rows)| rows.iter())
        .filter(|s| !s.is_test)
        .collect();
    hot.sort_by(|a, b| {
        counts
            .get(&b.name)
            .cmp(&counts.get(&a.name))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.line_start.cmp(&b.line_start))
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
    });
    let hot_symbols = hot
        .into_iter()
        .take(10)
        .map(|s| OverviewHotSymbol {
            handle: s.handle().to_string(),
            fan_in: counts.get(&s.name).copied().unwrap_or(0),
            counts_floor: true,
        })
        .collect();
    let entrypoints = ranked_entrypoints(db, &features, &symbols, &counts, &flags)?;

    let top_risk_files = ranking.rows().iter().take(TOP_RISK_CAP).cloned().collect();
    let top_risk_tests = ranking
        .test_rows()
        .iter()
        .take(TOP_RISK_CAP)
        .cloned()
        .collect();

    let trust_boundary_clusters: Vec<TrustBoundaryCount> = db
        .trust_boundary_counts()?
        .into_iter()
        .map(|(boundary, file_count)| TrustBoundaryCount {
            boundary,
            file_count,
        })
        .collect();

    let test_conventions = test_conventions_for(&languages);
    let hook_health = crate::hook_health::inspect(root);
    let recovery = recovery(&freshness, db.get_git_index_state()?.is_some());

    codesage_protocol::work::checkpoint()?;
    Ok(ProjectOverview {
        project_root: root.display().to_string(),
        languages,
        file_count: db.file_count()?,
        symbol_count: db.symbol_count()?,
        freshness,
        feature_summary,
        feature_count,
        module_map,
        top_risk_files,
        top_risk_tests,
        hot_symbols,
        recovery,
        trust_boundary_clusters,
        test_conventions,
        entrypoints,
        hook_health,
    })
}

fn ranked_entrypoints(
    db: &Database,
    features: &[FeatureRecord],
    symbols: &HashMap<String, Vec<Symbol>>,
    counts: &HashMap<String, u32>,
    flags: &HashMap<String, bool>,
) -> Result<Vec<EntrypointSummary>> {
    let mut candidates: Vec<_> = features
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
        .map(|f| {
            let auxiliary = f
                .entry_path
                .split('/')
                .any(|part| matches!(part, "bench" | "benches" | "scripts" | "examples"));
            let class = if flags.get(&f.entry_path).copied().unwrap_or(false) {
                2
            } else if auxiliary {
                1
            } else {
                0
            };
            let kind = match f.kind {
                FeatureKind::CliCommand => 3,
                FeatureKind::Service | FeatureKind::Route => 2,
                _ => 1,
            };
            let fan_in: u64 = symbols
                .get(&f.entry_path)
                .into_iter()
                .flatten()
                .filter(|s| !s.is_test)
                .map(|s| u64::from(counts.get(&s.name).copied().unwrap_or(0)))
                .sum();
            let churn = db
                .git_file(&f.entry_path)?
                .map_or(0.0, |row| row.churn_score);
            Ok((f, class, kind, fan_in, churn))
        })
        .collect::<Result<_>>()?;
    candidates.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| b.4.total_cmp(&a.4))
            .then_with(|| a.0.entry_path.cmp(&b.0.entry_path))
            .then_with(|| a.0.feature_id.cmp(&b.0.feature_id))
    });
    let mut selected = Vec::new();
    for class in 0..=2 {
        let mut kinds = HashSet::new();
        for (f, candidate_class, _, _, _) in &candidates {
            if selected.len() == ENTRYPOINT_CAP {
                break;
            }
            if *candidate_class == class && kinds.insert(f.kind) {
                selected.push(*f);
            }
        }
        for (f, candidate_class, _, _, _) in &candidates {
            if selected.len() == ENTRYPOINT_CAP {
                break;
            }
            if *candidate_class == class && !selected.iter().any(|s| s.feature_id == f.feature_id) {
                selected.push(*f);
            }
        }
        if selected.len() == ENTRYPOINT_CAP {
            break;
        }
    }
    Ok(selected
        .into_iter()
        .take(ENTRYPOINT_CAP)
        .map(|f| EntrypointSummary {
            feature_id: f.feature_id.clone(),
            kind: f.kind,
            title: f.title.clone(),
            entry_path: f.entry_path.clone(),
            command: f.entry_command.clone(),
            route: f.entry_route.clone(),
        })
        .collect())
}

fn module_map(
    root: &Path,
    files: &[(i64, String, Language)],
    symbols: &HashMap<String, Vec<Symbol>>,
    flags: &HashMap<String, bool>,
) -> Result<Vec<OverviewModule>> {
    let mut modules: BTreeMap<String, OverviewModule> = BTreeMap::new();
    let mut package_roots = HashMap::new();
    let mut remaining_bytes: u64 = 32 * 1024 * 1024;
    for (_, file, language) in files {
        codesage_protocol::work::checkpoint()?;
        let path = package_root(root, file, &mut package_roots);
        let row = modules
            .entry(path.clone())
            .or_insert_with(|| OverviewModule {
                handle: Handle::dir(&path).map(|handle| handle.to_string()),
                path,
                files: 0,
                lines: 0,
                unmeasured_files: 0,
                symbols: 0,
                product_symbols: 0,
                languages: Vec::new(),
                test_share: 0.0,
            });
        row.files += 1;
        row.test_share += if flags.get(file).copied().unwrap_or(false) {
            1.0
        } else {
            0.0
        };
        let definitions = symbols.get(file);
        row.symbols += definitions.map_or(0, Vec::len);
        row.product_symbols += definitions
            .into_iter()
            .flatten()
            .filter(|s| !s.is_test && !flags.get(file).copied().unwrap_or(false))
            .count();
        if let Some(lang) = row.languages.iter_mut().find(|l| l.language == *language) {
            lang.file_count += 1;
        } else {
            row.languages.push(LanguageStat {
                language: *language,
                file_count: 1,
            });
        }
        if remaining_bytes == 0 {
            row.unmeasured_files += 1;
            continue;
        }
        let limit = remaining_bytes.min(10 * 1024 * 1024);
        match crate::describe::read_regular(root, file, limit) {
            Ok(bytes) => {
                remaining_bytes -= bytes.len() as u64;
                row.lines += bytes.iter().filter(|&&b| b == b'\n').count()
                    + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
            }
            Err(_) => {
                remaining_bytes -= limit;
                row.unmeasured_files += 1;
            }
        }
    }
    let mut modules: Vec<_> = modules.into_values().collect();
    for row in &mut modules {
        row.test_share /= row.files as f64;
        row.languages.sort_by(|a, b| {
            b.file_count
                .cmp(&a.file_count)
                .then_with(|| a.language.as_str().cmp(b.language.as_str()))
        });
    }
    modules.sort_by(|a, b| {
        b.product_symbols
            .cmp(&a.product_symbols)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(modules)
}

fn package_root(root: &Path, file: &str, cache: &mut HashMap<String, bool>) -> String {
    for directory in Path::new(file)
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let path = directory.to_string_lossy().into_owned();
        let package = *cache.entry(path.clone()).or_insert_with(|| {
            [
                "Cargo.toml",
                "package.json",
                "pyproject.toml",
                "pom.xml",
                "go.mod",
                "composer.json",
            ]
            .iter()
            .any(|manifest| {
                let manifest = root.join(&path).join(manifest);
                std::fs::symlink_metadata(&manifest).is_ok_and(|meta| meta.is_file())
                    && manifest.canonicalize().is_ok_and(|manifest| {
                        root.canonicalize()
                            .is_ok_and(|root| manifest.starts_with(root))
                    })
            })
        });
        if package {
            return path;
        }
    }
    file.split_once('/')
        .map_or(".".to_string(), |(directory, _)| directory.to_string())
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
    // A damaged or legacy semantic table is unknown coverage, not an empty
    // index; the structural half of the overview stays available either way.
    let semantic = db.semantic_file_count();
    let semantic_indexed_files = *semantic.as_ref().unwrap_or(&0);
    let semantic_unavailable = semantic.err().map(|error| format!("{error:#}"));
    FreshnessInfo {
        structural_kind,
        structural_summary: report.summary(),
        commits_behind: report.commits_between,
        indexed_files_behind: report.indexed_files_behind,
        indexed_files_behind_bounded: report.indexed_files_behind_bounded,
        indexed_sha: report.stored_sha,
        head_sha: report.head_sha,
        semantic_indexed_files,
        semantic_indexed: semantic_indexed_files > 0,
        semantic_unavailable,
    }
}

// Keep these hints aligned with `recommend_tests` sibling conventions.
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
            Language::C => {
                "C: `foo_test.c` / `test_foo.c` (sibling or under `tests/`); php-src \
                 `<dir>/tests/*.phpt`"
            }
            Language::Cpp => {
                "C++: `foo_test.cpp` / `test_foo.cpp` (sibling or under `tests/`, \
                 same extension)"
            }
        })
        .map(str::to_string)
        .collect()
}

fn recovery(freshness: &FreshnessInfo, has_history: bool) -> Vec<OverviewRecovery> {
    let mut calls = Vec::new();
    // A commit range that touched no indexed file is not a reason to reindex;
    // an unmeasured comparison, or one that stopped before its first record,
    // still is.
    if matches!(
        freshness.structural_kind.as_str(),
        "behind_head" | "unrelated_ancestor"
    ) && (freshness.indexed_files_behind != Some(0) || freshness.indexed_files_behind_bounded)
    {
        calls.push(OverviewRecovery {
            command: "codesage index".to_string(),
            why: format!(
                "{} — structural results may not reflect the working tree",
                freshness.structural_summary
            ),
        });
    }
    if let Some(reason) = &freshness.semantic_unavailable {
        calls.push(OverviewRecovery {
            command: "codesage index --full".to_string(),
            why: format!("semantic coverage could not be measured: {reason}"),
        });
    }
    if !has_history && freshness.structural_kind != "not_git" {
        calls.push(OverviewRecovery {
            command: "codesage git-index".to_string(),
            why: "Git history has not been indexed".to_string(),
        });
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, source: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }

    fn product_fixture() -> (tempfile::TempDir, Database) {
        let root = tempfile::tempdir().unwrap();
        for (path, source) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers=[\"components/app\",\"components/core\"]\n",
            ),
            (
                "components/app/Cargo.toml",
                "[package]\nname=\"acme\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nrmcp=\"3.1\"\n",
            ),
            (
                "components/app/src/main.rs",
                "mod server;\nfn main() { server::run(); }\n",
            ),
            (
                "components/app/src/server.rs",
                "use rmcp::ServerHandler;\npub struct Acme;\nimpl ServerHandler for Acme {}\npub fn run() {}\n",
            ),
            (
                "components/core/Cargo.toml",
                "[package]\nname=\"acme-core\"\nversion=\"0.1.0\"\n",
            ),
            (
                "components/core/src/lib.rs",
                "pub fn leaf() {}\npub fn caller() { leaf(); leaf(); }\n#[cfg(test)] mod tests { fn private_test() {} }\n",
            ),
            (
                "components/core/tests/integration.rs",
                "#[test] fn exercises() { leaf(); }\n",
            ),
            ("tools/worker.py", "def work():\n    return 1\n"),
        ] {
            write(root.path(), path, source);
        }
        for index in 0..20 {
            write(
                root.path(),
                &format!("bench/task{index}.py"),
                "if __name__ == '__main__':\n    pass\n",
            );
        }
        let db = Database::open_in_memory().unwrap();
        crate::full_index(root.path(), &db, &[], false).unwrap();
        codesage_features::map_features(root.path(), &db, &[]).unwrap();
        (root, db)
    }

    #[test]
    fn product_entrypoints_are_diverse_before_bench_scripts() {
        let (root, db) = product_fixture();
        let overview = build_project_overview(root.path(), &db).unwrap();
        assert_eq!(overview.entrypoints.len(), 15);
        assert_eq!(
            overview
                .entrypoints
                .iter()
                .take(3)
                .map(|e| (e.kind, e.entry_path.as_str()))
                .collect::<Vec<_>>(),
            [
                (FeatureKind::CliCommand, "components/app/src/main.rs"),
                (FeatureKind::Service, "components/app/src/server.rs"),
                (FeatureKind::Library, "components/core/src/lib.rs"),
            ]
        );
        let value = serde_json::to_value(&overview).unwrap();
        assert!(value.get("suggested_next_calls").is_none());
        assert!(value["module_map"].is_array());
    }

    #[test]
    fn module_map_uses_package_roots_and_discloses_unreadable_lines() {
        let (root, db) = product_fixture();
        std::fs::remove_file(root.path().join("tools/worker.py")).unwrap();
        let overview = build_project_overview(root.path(), &db).unwrap();
        let core = overview
            .module_map
            .iter()
            .find(|m| m.path == "components/core")
            .unwrap();
        assert_eq!((core.files, core.lines, core.unmeasured_files), (2, 4, 0));
        assert_eq!(core.test_share, 0.5);
        let symbols = db
            .symbols_for_files(&[
                "components/core/src/lib.rs".into(),
                "components/core/tests/integration.rs".into(),
            ])
            .unwrap();
        assert_eq!(core.symbols, symbols.values().map(Vec::len).sum::<usize>());
        assert_eq!(core.product_symbols, 2);
        assert_eq!(core.languages.len(), 1);
        assert_eq!(core.languages[0].language, Language::Rust);
        assert_eq!(core.languages[0].file_count, 2);
        assert_eq!(core.handle.as_deref(), Some("dir:components/core"));
        let tools = overview
            .module_map
            .iter()
            .find(|m| m.path == "tools")
            .unwrap();
        assert_eq!(
            (tools.files, tools.lines, tools.unmeasured_files),
            (1, 0, 1)
        );
        assert!(
            overview
                .module_map
                .windows(2)
                .all(|pair| pair[0].product_symbols >= pair[1].product_symbols)
        );
    }

    #[test]
    fn risk_tests_are_separate_and_include_tests_restores_merged_ranking() {
        let (root, db) = product_fixture();
        let product = "components/core/src/lib.rs";
        let test = "components/core/tests/integration.rs";
        db.upsert_git_file(product, 2.0, 1, 2, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file(test, 20.0, 9, 10, Some(1_700_000_000))
            .unwrap();
        let overview = build_project_overview(root.path(), &db).unwrap();
        assert!(overview.top_risk_files.iter().all(|r| r.file != test));
        assert!(overview.top_risk_tests.iter().any(|r| r.file == test));
        let merged = build_project_overview_with_options(root.path(), &db, true).unwrap();
        assert!(merged.top_risk_files.iter().any(|r| r.file == test));
        assert_eq!(
            serde_json::to_value(overview.top_risk_tests).unwrap(),
            serde_json::to_value(merged.top_risk_tests).unwrap()
        );
        assert!(
            overview
                .hot_symbols
                .iter()
                .any(|s| s.handle == "sym:components/core/src/lib.rs#leaf")
        );
        assert!(overview.hot_symbols.iter().all(|s| !s.handle.contains("private_test") && !s.handle.contains("integration.rs")));
        assert!(
            overview
                .hot_symbols
                .windows(2)
                .all(|pair| pair[0].fan_in >= pair[1].fan_in)
        );
    }

    #[test]
    fn absent_history_recovers_only_for_git_projects() {
        assert_eq!(
            recovery(&freshness("fresh"), false)[0].command,
            "codesage git-index"
        );
        assert!(recovery(&freshness("not_git"), false).is_empty());
    }

    #[test]
    fn reused_risk_ranking_does_not_cache_working_tree_line_counts() {
        let (root, db) = product_fixture();
        let ranking = crate::session::top_risk_ranking(&db).unwrap();
        let before = build_project_overview_with_top_risk(root.path(), &db, &ranking).unwrap();
        write(
            root.path(),
            "tools/worker.py",
            "def work():\n    answer = 1\n    return answer\n",
        );
        let after = build_project_overview_with_top_risk(root.path(), &db, &ranking).unwrap();
        assert_eq!(
            before
                .module_map
                .iter()
                .find(|m| m.path == "tools")
                .unwrap()
                .lines,
            2
        );
        assert_eq!(
            after
                .module_map
                .iter()
                .find(|m| m.path == "tools")
                .unwrap()
                .lines,
            3
        );
        assert_eq!(before.symbol_count, after.symbol_count);
    }

    #[test]
    fn module_line_budget_keeps_membership_and_discloses_unmeasured_files() {
        let root = tempfile::tempdir().unwrap();
        let bytes = "x".repeat(9 * 1024 * 1024);
        let mut files = Vec::new();
        for index in 0..4 {
            let path = format!("large/{index}.py");
            write(root.path(), &path, &bytes);
            files.push((index, path, Language::Python));
        }
        let modules = module_map(root.path(), &files, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(
            (
                modules[0].files,
                modules[0].lines,
                modules[0].unmeasured_files
            ),
            (4, 3, 1)
        );
    }

    const STALE_TOOL: &str = "codesage index";

    fn freshness(kind: &str) -> FreshnessInfo {
        FreshnessInfo {
            structural_kind: kind.to_string(),
            structural_summary: format!("summary for {kind}"),
            commits_behind: None,
            indexed_files_behind: None,
            indexed_files_behind_bounded: false,
            indexed_sha: None,
            head_sha: None,
            semantic_indexed_files: 0,
            semantic_indexed: false,
            semantic_unavailable: None,
        }
    }

    #[test]
    fn behind_head_prepends_stale_index_nudge() {
        let calls = recovery(&freshness("behind_head"), true);
        assert_eq!(
            calls.first().map(|c| c.command.as_str()),
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
        let calls = recovery(&freshness("unrelated_ancestor"), true);
        assert_eq!(calls.first().map(|c| c.command.as_str()), Some(STALE_TOOL));
    }

    #[test]
    fn commits_behind_over_unindexed_paths_do_not_nudge_a_reindex() {
        for kind in ["behind_head", "unrelated_ancestor"] {
            let mut state = freshness(kind);
            state.commits_behind = Some(5);
            state.indexed_files_behind = Some(0);
            let calls = recovery(&state, true);
            assert!(
                calls.iter().all(|c| c.command != STALE_TOOL),
                "{kind}: 5 commits touching no indexed file must not ask for a reindex"
            );
        }
    }

    #[test]
    fn a_bounded_zero_count_still_nudges_a_reindex() {
        let mut state = freshness("behind_head");
        state.commits_behind = Some(5);
        state.indexed_files_behind = Some(0);
        state.indexed_files_behind_bounded = true;
        let calls = recovery(&state, true);
        assert_eq!(
            calls.first().map(|c| c.command.as_str()),
            Some(STALE_TOOL),
            "a comparison that stopped before its first record proved nothing"
        );
    }

    #[test]
    fn build_overview_on_empty_db_reports_zero_counts() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let overview = build_project_overview(dir.path(), &db)
            .expect("overview must build on a valid empty index");
        assert_eq!(overview.file_count, 0);
        assert_eq!(overview.symbol_count, 0);
        assert!(overview.top_risk_files.is_empty());
        assert!(overview.trust_boundary_clusters.is_empty());
        assert!(
            overview.hook_health.is_none(),
            "a non-git temp dir has no hooks to report"
        );
    }
    fn assert_semantic_unknown(db: &Database) {
        let dir = tempfile::tempdir().unwrap();
        let overview = build_project_overview(dir.path(), db)
            .expect("a semantic-schema problem must not fail the structural overview");
        let reason = overview
            .freshness
            .semantic_unavailable
            .as_deref()
            .expect("unmeasured semantic coverage must be disclosed");
        assert!(reason.contains("semantic_files"), "{reason}");
        assert!(!overview.freshness.semantic_indexed);
        assert_eq!(overview.recovery[0].command, "codesage index --full");
    }

    #[test]
    fn missing_semantic_table_is_unknown_not_empty() {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests("DROP TABLE semantic_files")
            .unwrap();
        assert_semantic_unknown(&db);
    }

    #[test]
    fn legacy_path_only_semantic_table_is_unknown() {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests(
            "DROP TABLE semantic_files;
             CREATE TABLE semantic_files(path TEXT PRIMARY KEY, content_hash TEXT NOT NULL,
                 indexed_at INTEGER NOT NULL DEFAULT (unixepoch()))",
        )
        .unwrap();
        assert_semantic_unknown(&db);
    }

    #[test]
    fn malformed_semantic_primary_key_is_unknown() {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests(
            "DROP TABLE semantic_files;
             CREATE TABLE semantic_files(chunk_table TEXT, path TEXT, content_hash TEXT, indexed_at INTEGER)",
        )
        .unwrap();
        assert_semantic_unknown(&db);
    }

    #[test]
    fn empty_semantic_table_is_measured_as_not_indexed() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let overview = build_project_overview(dir.path(), &db).unwrap();
        assert_eq!(overview.freshness.semantic_unavailable, None);
        assert_eq!(overview.freshness.semantic_indexed_files, 0);
        assert!(!overview.freshness.semantic_indexed);
    }

    #[test]
    fn build_overview_reports_hook_health_without_reaping() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            &["init", "-q"][..],
            &["config", "core.hooksPath", ".git/hooks"][..],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(
            hooks.join("post-merge"),
            "#!/bin/sh\n# installed by codesage install-hooks\n",
        )
        .unwrap();
        let lockdir = root.join(crate::hook_health::LOCK_DIR);
        std::fs::create_dir_all(&lockdir).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(lockdir.join("pid"), dead.to_string()).unwrap();
        std::fs::write(
            root.join(crate::hook_health::LOG_FILE),
            "[Sun Sep 14 07:09:37 UTC 2026] post-merge hook start pid=42\n",
        )
        .unwrap();

        let overview = build_project_overview(root, &db).unwrap();
        let health = overview.hook_health.expect("git repo reports hook health");
        assert_eq!(health.installed_hooks, vec!["post-merge".to_string()]);
        assert_eq!(
            health.last_run.as_deref(),
            Some("Sun Sep 14 07:09:37 UTC 2026")
        );
        assert_eq!(health.last_exit, None, "no exit line was logged");
        assert_eq!(health.lock.state, crate::hook_health::LOCK_HELD_DEAD);
        assert_eq!(health.lock.pid, Some(dead));
        assert!(!health.lock.reaped);
        assert!(
            lockdir.is_dir(),
            "overview is read-only: the dead lock stays"
        );
    }

    #[test]
    fn fresh_index_omits_stale_index_nudge() {
        let calls = recovery(&freshness("fresh"), true);
        assert!(
            calls.iter().all(|c| c.command != STALE_TOOL),
            "fresh index must not nudge a reindex, got {:?}",
            calls.iter().map(|c| c.command.as_str()).collect::<Vec<_>>()
        );
        assert!(calls.is_empty(), "fresh history and index need no recovery");
    }
}
