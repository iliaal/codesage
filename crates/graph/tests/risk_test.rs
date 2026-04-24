//! assess_risk composition tests. Seeds git_files + git_co_changes directly so
//! the score inputs are controlled (bypasses the real git log indexer).

use codesage_graph::{assess_risk, full_index};
use codesage_storage::Database;

/// Build a small project so impact_analysis has a graph to walk. One class, two
/// callers, one test. The structural data is only needed to give assess_risk's
/// dependent-file BFS something to find.
fn setup_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Controller.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller {\n  public function show(Repository $r, $id) { return $r->find($id); }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Service.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Service {\n  public function run(Repository $r) { return $r->find(1); }\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[]).unwrap();
    (dir, db)
}

#[test]
fn score_zero_and_note_when_no_git_history() {
    let (_dir, db) = setup_project();
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert_eq!(r.total_commits, 0);
    assert_eq!(r.fix_count, 0);
    assert!(
        r.score < 0.2,
        "score without history should stay low, got {}",
        r.score
    );
    assert!(
        r.notes.iter().any(|n| n.contains("no git history")),
        "expected 'no git history' note, got {:?}",
        r.notes
    );
}

#[test]
fn hotspot_fix_heavy_file_scores_high_and_emits_notes() {
    let (_dir, db) = setup_project();

    // Seed a hot, fix-heavy file with lots of churn and high fix ratio.
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    // A few cooler files so churn_percentile is well-defined and our target ends up on top.
    for (p, c) in [
        ("Controller.php", 1.0_f64),
        ("Service.php", 2.0),
        ("other_a.php", 0.5),
        ("other_b.php", 0.7),
    ] {
        db.upsert_git_file(p, c, 0, 5, Some(1_700_000_000)).unwrap();
    }

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.score >= 0.6,
        "hotspot+fix-heavy should score >= 0.6, got {}",
        r.score
    );
    assert!(r.churn_percentile >= 0.99);
    assert!((r.fix_ratio - 0.5).abs() < 1e-9);
    let notes = r.notes.join(" | ");
    assert!(notes.contains("hotspot"), "missing hotspot note: {notes}");
    assert!(
        notes.contains("fix-heavy"),
        "missing fix-heavy note: {notes}"
    );
}

#[test]
fn cold_isolated_file_scores_low() {
    let (_dir, db) = setup_project();

    // Give a few files history so churn_percentile has a distribution.
    for p in ["Repository.php", "Controller.php", "Service.php"] {
        db.upsert_git_file(p, 10.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    db.upsert_git_file("other_cold.php", 0.01, 0, 1, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "other_cold.php").unwrap();
    assert!(
        r.score < 0.4,
        "cold file should score < 0.4, got {}",
        r.score
    );
    assert!(!r.notes.iter().any(|n| n.contains("hotspot")));
    assert!(!r.notes.iter().any(|n| n.contains("fix-heavy")));
}

#[test]
fn test_gap_false_when_coupled_to_test_file() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    // Pair must be lexicographically sorted.
    db.upsert_git_co_change(
        "Repository.php",
        "RepositoryTest.php",
        5.0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(!r.test_gap, "coupled test file must close the test gap");
    assert!(!r.notes.iter().any(|n| n.contains("test gap")));
    assert!(r.top_coupled.iter().any(|c| c.file == "RepositoryTest.php"));
}

#[test]
fn test_gap_false_when_sibling_test_exists_without_coupling() {
    let (_dir, db) = setup_project();
    // Same directory, PHP sibling convention.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    // No co-change relationship seeded.
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        !r.test_gap,
        "sibling test file should close the test gap even without coupling"
    );
}

#[test]
fn test_gap_true_when_no_test_sibling_and_no_coupled_test() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Controller.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_co_change(
        "Controller.php",
        "Repository.php",
        3.0,
        4,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(r.test_gap, "no test anywhere should flag test_gap");
    assert!(r.notes.iter().any(|n| n.contains("test gap")));
}

#[test]
fn high_coupling_triggers_coupling_note() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    for i in 0..10 {
        let other = format!("z_other_{i:02}.php");
        db.upsert_git_file(&other, 0.5, 0, 5, Some(1_700_000_000))
            .unwrap();
        // "Repository.php" < "z_other_NN.php" lexicographically so the pair is sorted correctly.
        db.upsert_git_co_change(
            "Repository.php",
            &other,
            (10 - i) as f64,
            5,
            Some(1_700_000_000),
        )
        .unwrap();
    }

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert_eq!(r.coupled_files, 10);
    assert!(
        r.notes.iter().any(|n| n.contains("high coupling")),
        "missing coupling note: {:?}",
        r.notes
    );
}

#[test]
fn wide_blast_radius_note_fires_when_many_dependents() {
    let (_dir, db) = setup_project();
    // Repository.php has 2 direct callers in the fixture. Force 10 dependents by
    // seeding git_files rows so the risk function still runs, but then assert the
    // note only fires when impact_analysis returns >=10 deps. On this tiny fixture
    // the impact depth-2 is 2, so the "wide blast radius" note must NOT fire.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.dependent_files < 10,
        "fixture has only 2 callers, got {}",
        r.dependent_files
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("wide blast radius")),
        "wide blast radius must not fire below the threshold, got {:?}",
        r.notes
    );
}

// ----- assess_risk_diff -----

#[test]
fn risk_diff_empty_input_returns_defaults() {
    let (_dir, db) = setup_project();
    let r = codesage_graph::assess_risk_diff(&db, &[]).unwrap();
    assert!(r.files.is_empty());
    assert_eq!(r.max_score, 0.0);
    assert_eq!(r.mean_score, 0.0);
    assert!(r.max_risk_file.is_none());
    assert!(r.summary_notes.is_empty());
}

#[test]
fn risk_diff_aggregates_max_and_mean_across_files() {
    let (_dir, db) = setup_project();

    // Hot+fix-heavy + lots of cooler files for a wide percentile distribution.
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Controller.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    for (p, c) in [
        ("Service.php", 2.0_f64),
        ("other_a.php", 0.5),
        ("other_b.php", 0.7),
    ] {
        db.upsert_git_file(p, c, 0, 5, Some(1_700_000_000)).unwrap();
    }

    let files = vec!["Repository.php".to_string(), "Controller.php".to_string()];
    let r = codesage_graph::assess_risk_diff(&db, &files).unwrap();

    assert_eq!(r.files.len(), 2);
    assert_eq!(r.max_risk_file.as_deref(), Some("Repository.php"));
    assert!(
        r.max_score >= 0.6,
        "max should reflect the hot file, got {}",
        r.max_score
    );
    assert!(
        r.mean_score < r.max_score,
        "mean should pull below max, got {}",
        r.mean_score
    );
    assert!(r.hotspot_files.contains(&"Repository.php".to_string()));
    assert!(r.fix_heavy_files.contains(&"Repository.php".to_string()));
    assert!(
        r.summary_notes.iter().any(|n| n.contains("hotspot")),
        "expected hotspot note, got {:?}",
        r.summary_notes
    );
}

#[test]
fn risk_diff_clusters_directories_past_threshold() {
    let (_dir, db) = setup_project();
    // Seed 6 files in one directory and 2 in another. Only the crowded dir
    // should cluster; the other keeps per-file detail.
    let crowded: Vec<String> = (0..6)
        .map(|i| format!("app/Actions/Foo/File{i}.php"))
        .collect();
    for p in &crowded {
        db.upsert_git_file(p, 2.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let others = ["app/Http/Other.php".to_string(), "README.md".to_string()];
    for p in &others {
        db.upsert_git_file(p, 0.5, 0, 5, Some(1_700_000_000))
            .unwrap();
    }

    let mut input = crowded.clone();
    input.extend_from_slice(&others);
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    // Crowded dir collapses; other two files stay verbatim.
    assert_eq!(r.files.len(), 2, "expected 2 un-clustered files");
    assert_eq!(
        r.clustered_directories.len(),
        1,
        "expected one cluster for the crowded dir"
    );
    let cluster = &r.clustered_directories[0];
    assert_eq!(cluster.directory, "app/Actions/Foo");
    assert_eq!(cluster.count, 6);
    assert_eq!(cluster.top_files.len(), 3, "top-3 preserved in detail");
    assert_eq!(cluster.omitted_files.len(), 3, "rest listed by name");
}

#[test]
fn risk_diff_below_threshold_keeps_flat_shape() {
    // 4 files in one dir is below the 5-file threshold; shape stays flat so
    // existing agent prompts that assume `files` holds everything don't
    // break on typical small patches.
    let (_dir, db) = setup_project();
    let files: Vec<String> = (0..4).map(|i| format!("app/Foo/File{i}.php")).collect();
    for p in &files {
        db.upsert_git_file(p, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let r = codesage_graph::assess_risk_diff(&db, &files).unwrap();
    assert_eq!(r.files.len(), 4);
    assert!(r.clustered_directories.is_empty());
}

#[test]
fn risk_diff_cluster_preserves_rollup_coverage() {
    // A clustered file that trips a rollup (e.g. hotspot, test_gap) must
    // still appear in the rollup arrays even though its per-file detail was
    // omitted. That is how an agent cross-references clusters back to
    // specific concerns.
    let (_dir, db) = setup_project();
    // 5 files in the same dir: one hot, four cool, plus some other repo
    // files so the hot one actually percentiles.
    db.upsert_git_file("app/Risk/Hot.php", 100.0, 10, 40, Some(1_700_000_000))
        .unwrap();
    for p in [
        "app/Risk/B.php",
        "app/Risk/C.php",
        "app/Risk/D.php",
        "app/Risk/E.php",
    ] {
        db.upsert_git_file(p, 0.1, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    // A few cool files elsewhere to pull Hot.php's percentile high.
    for p in ["unrelated_a.php", "unrelated_b.php", "unrelated_c.php"] {
        db.upsert_git_file(p, 0.05, 0, 5, Some(1_700_000_000))
            .unwrap();
    }

    let input = vec![
        "app/Risk/Hot.php".to_string(),
        "app/Risk/B.php".to_string(),
        "app/Risk/C.php".to_string(),
        "app/Risk/D.php".to_string(),
        "app/Risk/E.php".to_string(),
    ];
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    assert_eq!(r.clustered_directories.len(), 1);
    assert!(
        r.hotspot_files.contains(&"app/Risk/Hot.php".to_string()),
        "rollup must still list Hot.php even though it was clustered"
    );
}

#[test]
fn risk_diff_summary_includes_max_score_warning_when_high() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    for p in ["Controller.php", "Service.php"] {
        db.upsert_git_file(p, 0.1, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let r = codesage_graph::assess_risk_diff(&db, &["Repository.php".to_string()]).unwrap();
    assert!(r.max_score >= 0.6);
    assert!(
        r.summary_notes.iter().any(|n| n.contains("max risk score")),
        "expected explicit max-score warning, got {:?}",
        r.summary_notes
    );
}

// ----- recommend_tests -----

#[test]
fn recommend_tests_returns_empty_when_no_test_signal() {
    let (_dir, db) = setup_project();
    let r = codesage_graph::recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
    assert!(r.primary.is_empty());
    assert!(r.coupled.is_empty());
    assert!(
        r.notes.iter().any(|n| n.contains("no test files found")),
        "expected explanatory note, got {:?}",
        r.notes
    );
}

#[test]
fn recommend_tests_finds_sibling_test_in_index() {
    let (_dir, db) = setup_project();
    // Seed a sibling test file in git_files. assess_risk's test_sibling check
    // queries the same table, so this is the canonical "sibling exists" signal.
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = codesage_graph::recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
    assert_eq!(r.primary, vec!["RepositoryTest.php".to_string()]);
    assert!(r.coupled.is_empty(), "no co-change history seeded");
}

#[test]
fn recommend_tests_finds_coupled_test_via_co_change() {
    let (_dir, db) = setup_project();
    // No sibling file. Coupled test surfaces via co-change.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file(
        "tests/integration/auth_flow.test.ts",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    db.upsert_git_co_change(
        "Repository.php",
        "tests/integration/auth_flow.test.ts",
        4.2,
        8,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
    assert!(r.primary.is_empty(), "no sibling seeded");
    assert_eq!(r.coupled.len(), 1);
    let entry = &r.coupled[0];
    assert_eq!(entry.file, "tests/integration/auth_flow.test.ts");
    assert_eq!(entry.source, "Repository.php");
    assert_eq!(entry.count, 8);
}

#[test]
fn recommend_tests_dedupes_coupled_when_also_primary() {
    let (_dir, db) = setup_project();
    // Same file shows up as both sibling and a co-changer; recommend_tests should
    // only list it once, in primary, to avoid duplicate "run me" lines.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_co_change(
        "Repository.php",
        "RepositoryTest.php",
        5.0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
    assert_eq!(r.primary, vec!["RepositoryTest.php".to_string()]);
    assert!(
        r.coupled.is_empty(),
        "RepositoryTest.php was already in primary; expected no duplicate in coupled"
    );
}

#[test]
fn recommend_tests_aggregates_across_multiple_input_files() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Service.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("RepositoryTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("ServiceTest.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = codesage_graph::recommend_tests(
        &db,
        &["Repository.php".to_string(), "Service.php".to_string()],
    )
    .unwrap();
    assert_eq!(r.primary.len(), 2, "both siblings should surface");
    assert!(r.primary.contains(&"RepositoryTest.php".to_string()));
    assert!(r.primary.contains(&"ServiceTest.php".to_string()));
}

#[test]
fn recommend_tests_finds_rust_integration_tests_under_crate_tests_dir() {
    let (_dir, db) = setup_project();
    // Rust convention: source at crates/<name>/src/, integration tests at
    // crates/<name>/tests/. There's no per-file naming convention, so the
    // recommender lists every .rs file in that tests/ directory.
    db.upsert_git_file("crates/storage/src/db.rs", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file(
        "crates/storage/tests/db_integration.rs",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    db.upsert_git_file(
        "crates/storage/tests/schema_migration_test.rs",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    // A test under a different crate must NOT leak in.
    db.upsert_git_file(
        "crates/parser/tests/extract_test.rs",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r =
        codesage_graph::recommend_tests(&db, &["crates/storage/src/db.rs".to_string()]).unwrap();
    assert!(
        r.primary
            .contains(&"crates/storage/tests/db_integration.rs".to_string())
    );
    assert!(
        r.primary
            .contains(&"crates/storage/tests/schema_migration_test.rs".to_string())
    );
    assert!(
        !r.primary
            .contains(&"crates/parser/tests/extract_test.rs".to_string()),
        "tests from a different crate must not leak in: {:?}",
        r.primary
    );
}

#[test]
fn recommend_tests_skips_fixture_files_under_rust_tests_dir() {
    let (_dir, db) = setup_project();
    db.upsert_git_file(
        "crates/parser/src/extract.rs",
        1.0,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    db.upsert_git_file(
        "crates/parser/tests/extract_test.rs",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    // Fixture files are NOT test entry points; should not be recommended.
    db.upsert_git_file(
        "crates/parser/tests/fixtures/sample.rs",
        0.1,
        0,
        2,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &["crates/parser/src/extract.rs".to_string()])
        .unwrap();
    assert_eq!(r.primary, vec!["crates/parser/tests/extract_test.rs"]);
    assert!(
        !r.primary
            .contains(&"crates/parser/tests/fixtures/sample.rs".to_string())
    );
}

#[test]
fn recommend_tests_finds_phpt_tests_for_c_source() {
    let (_dir, db) = setup_project();
    // php-src convention: source at Zend/zend_compile.c, tests at Zend/tests/*.phpt.
    db.upsert_git_file("Zend/zend_compile.c", 5.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Zend/tests/bug12345.phpt", 0.5, 0, 2, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Zend/tests/gh21709.phpt", 0.5, 0, 2, Some(1_700_000_000))
        .unwrap();
    // Different subsystem's tests must not leak in.
    db.upsert_git_file(
        "ext/standard/tests/array_test.phpt",
        0.5,
        0,
        2,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &["Zend/zend_compile.c".to_string()]).unwrap();
    assert!(r.primary.contains(&"Zend/tests/bug12345.phpt".to_string()));
    assert!(r.primary.contains(&"Zend/tests/gh21709.phpt".to_string()));
    assert!(
        !r.primary
            .contains(&"ext/standard/tests/array_test.phpt".to_string()),
        "tests from a different subsystem must not leak in: {:?}",
        r.primary
    );
}

#[test]
fn recommend_tests_skips_phpt_tests_dir_when_oversized() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("ext/standard/array.c", 5.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    // Seed 60 .phpt files — should be skipped as too noisy for "primary".
    for i in 0..60 {
        let p = format!("ext/standard/tests/test_{i:03}.phpt");
        db.upsert_git_file(&p, 0.1, 0, 2, Some(1_700_000_000))
            .unwrap();
    }

    let r = codesage_graph::recommend_tests(&db, &["ext/standard/array.c".to_string()]).unwrap();
    assert!(
        r.primary.is_empty(),
        "tests dir over the 50-file threshold should not be returned as primary, got {} entries",
        r.primary.len()
    );
}

#[test]
fn recommend_tests_finds_laravel_mirror_tree_tests() {
    let (_dir, db) = setup_project();
    // Laravel convention seen in real projects: source at
    // app/Actions/Foo/Bar.php paired with test at
    // tests/Integration/Actions/Foo/BarTest.php (mirror tree under
    // tests/{Unit,Feature,Integration,Browser}). The flat sibling check
    // (tests/Unit/BarTest.php) misses these because the test path has the
    // intermediate Actions/Foo segments.
    let src = "app/Actions/CredentialingApplication/ExportZipAction.php";
    let test = "tests/Integration/Actions/CredentialingApplication/ExportZipActionTest.php";
    db.upsert_git_file(src, 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file(test, 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();
    // A test for an unrelated class must not leak in.
    db.upsert_git_file(
        "tests/Integration/Actions/Other/UnrelatedActionTest.php",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &[src.to_string()]).unwrap();
    assert_eq!(r.primary, vec![test.to_string()]);
}

#[test]
fn recommend_tests_finds_laravel_test_under_unit_or_feature_too() {
    let (_dir, db) = setup_project();
    let src = "app/Services/Facility/ProviderService.php";
    db.upsert_git_file(src, 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file(
        "tests/Unit/Services/Facility/ProviderServiceTest.php",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();
    db.upsert_git_file(
        "tests/Feature/Services/Facility/ProviderServiceTest.php",
        0.5,
        0,
        5,
        Some(1_700_000_000),
    )
    .unwrap();

    let r = codesage_graph::recommend_tests(&db, &[src.to_string()]).unwrap();
    assert!(
        r.primary
            .contains(&"tests/Unit/Services/Facility/ProviderServiceTest.php".to_string())
    );
    assert!(
        r.primary
            .contains(&"tests/Feature/Services/Facility/ProviderServiceTest.php".to_string())
    );
}

#[test]
fn recommend_tests_finds_symfony_mirror_tree_tests() {
    let (_dir, db) = setup_project();
    // Symfony convention: src/<rest>/<stem>.php pairs with tests/<rest>/<stem>Test.php
    // (no Unit/Feature subdir; tests/ mirrors src/ directly).
    let src = "src/Domain/Order/OrderService.php";
    let test = "tests/Domain/Order/OrderServiceTest.php";
    db.upsert_git_file(src, 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file(test, 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = codesage_graph::recommend_tests(&db, &[src.to_string()]).unwrap();
    assert_eq!(r.primary, vec![test.to_string()]);
}
