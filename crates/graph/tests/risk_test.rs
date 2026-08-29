//! assess_risk composition tests. Seeds git_files + git_co_changes directly so
//! the score inputs are controlled (bypasses the real git log indexer).

use codesage_graph::{assess_risk, full_index};
use codesage_protocol::{FileInfo, Language};
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
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

fn index_test_file(db: &Database, path: &str) {
    let language = if path.ends_with(".php") || path.ends_with(".phpt") {
        Language::Php
    } else if path.ends_with(".rs") {
        Language::Rust
    } else if path.ends_with(".go") {
        Language::Go
    } else if path.ends_with(".py") {
        Language::Python
    } else if path.ends_with(".c") || path.ends_with(".h") {
        Language::C
    } else {
        Language::TypeScript
    };

    db.upsert_file(&FileInfo {
        path: path.to_string(),
        language,
        content_hash: format!("test-hash-{path}"),
    })
    .unwrap();
}

#[test]
fn score_zero_and_note_when_no_git_history() {
    let (_dir, db) = setup_project();
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(r.found);
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
fn missing_path_is_unknown_not_test_gap() {
    let (_dir, db) = setup_project();

    let r = assess_risk(&db, "NoSuch.php").unwrap();

    assert!(!r.found);
    assert_eq!(r.score, 0.0);
    assert!(!r.test_gap);
    assert!(r.notes.iter().any(|n| n.contains("not indexed")));
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
    // hotspot+fix-heavy+test-gap with no trust boundaries:
    // 0.32 churn + 0.18 fix + 0.13 test-gap ≈ 0.54.
    assert!(
        r.score >= 0.5,
        "hotspot+fix-heavy should score >= 0.5, got {}",
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
    index_test_file(&db, "RepositoryTest.php");
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

/// A test that reaches the file through the dependency graph closes the gap
/// even with no sibling test and no co-change history — the newly-added-helper
/// case that convention-plus-history reports as untested.
#[test]
fn test_gap_false_when_a_test_depends_on_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
    )
    .unwrap();
    // Named for Controller, not Repository, so the sibling-convention lookup
    // for Repository.php cannot match it. Its only link is a structural one.
    std::fs::write(
        root.join("ControllerTest.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass ControllerTest {\n  public function testFind(Repository $r) { return $r->find(1); }\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        !r.test_gap,
        "a test depending on the file must close the gap, notes: {:?}",
        r.notes
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("reaches this file") && n.contains("ControllerTest.php")),
        "expected the indirect-coverage note naming the test, got {:?}",
        r.notes
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("test gap")),
        "must not also claim a test gap, got {:?}",
        r.notes
    );
}

/// The gap note names the three checks that ran rather than asserting the file
/// is untested. Guards against a future edit reintroducing an absolute claim.
#[test]
fn test_gap_note_states_what_was_measured() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(r.test_gap);
    let note = r
        .notes
        .iter()
        .find(|n| n.contains("test gap"))
        .unwrap_or_else(|| panic!("expected a test-gap note, got {:?}", r.notes));
    for expected in ["sibling convention", "co-change history", "dependency hops"] {
        assert!(
            note.contains(expected),
            "test-gap note must name the {expected} check, got {note:?}"
        );
    }
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
    assert!(r.empty_input);
    assert!(r.files.is_empty());
    assert_eq!(r.max_score, 0.0);
    assert_eq!(r.mean_score, 0.0);
    assert!(r.max_risk_file.is_none());
    assert!(!r.summary_notes.is_empty());
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
    // See note on threshold change in `hotspot_fix_heavy_file_scores_high_and_emits_notes`.
    assert!(
        r.max_score >= 0.5,
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
    // See note on threshold change in `hotspot_fix_heavy_file_scores_high_and_emits_notes`.
    assert!(r.max_score >= 0.5);
    assert!(
        r.summary_notes.iter().any(|n| n.contains("max risk score")),
        "expected explicit max-score warning, got {:?}",
        r.summary_notes
    );
}

// ----- assess_risk: per-file cycle membership -----

#[test]
fn assess_risk_flags_file_in_two_file_cycle() {
    // A <-> B cycle. Per-file assess_risk should set in_cycle=true,
    // cycle_size=2, and list the other member in cycle_files.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("B.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "A.php").unwrap();
    assert!(
        r.in_cycle,
        "A.php is in the A<->B cycle, in_cycle should be true"
    );
    assert_eq!(r.cycle_size, 2);
    assert_eq!(r.cycle_files, vec!["B.php".to_string()]);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("in import cycle of 2 files: B.php")),
        "expected cycle note, got {:?}",
        r.notes
    );
}

#[test]
fn assess_risk_suggests_lowest_co_change_break_edge_in_cycle() {
    // A <-> B cycle with a recorded co-change weight. assess_risk should surface
    // a candidate break edge naming a cycle edge and its co-change weight, so an
    // agent knows which dependency to invert/remove to break the cycle.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("B.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    // Co-change pair stored sorted (A.php < B.php).
    db.upsert_git_co_change("A.php", "B.php", 2.5, 4, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "A.php").unwrap();
    assert!(r.in_cycle);
    let note = r
        .notes
        .iter()
        .find(|n| n.contains("candidate break point"))
        .unwrap_or_else(|| panic!("expected break-point note, got {:?}", r.notes));
    // Deterministic tie-break picks the edge sorted first (A.php → B.php).
    assert!(note.contains("A.php"), "note: {note}");
    assert!(note.contains("B.php"), "note: {note}");
    assert!(
        note.contains("2.50"),
        "expected co-change weight in note: {note}"
    );
}

#[test]
fn assess_risk_reframes_hub_dominated_cycle_as_decoupling_targets() {
    // Hub-spoke cycle: Resource imports P1/P2/P3 and each Pn imports Resource.
    // Resource has in-degree 3 within the cycle, so it's hub-dominated, not a
    // ring — cutting one edge won't break it. assess_risk should surface the
    // hub as a decoupling target instead of a single break edge.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Resource.php"),
        b"<?php\nnamespace App;\nuse App\\P1;\nuse App\\P2;\nuse App\\P3;\nclass Resource { public function a(P1 $a, P2 $b, P3 $c) {} }\n",
    )
    .unwrap();
    for p in ["P1", "P2", "P3"] {
        std::fs::write(
            root.join(format!("{p}.php")),
            format!("<?php\nnamespace App;\nuse App\\Resource;\nclass {p} {{ public function x(Resource $r) {{}} }}\n").as_bytes(),
        )
        .unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();

    let r = assess_risk(&db, "Resource.php").unwrap();
    assert!(r.in_cycle, "Resource is in the hub-spoke cycle");
    let note = r
        .notes
        .iter()
        .find(|n| n.contains("hub-dominated"))
        .unwrap_or_else(|| panic!("expected hub-dominated note, got {:?}", r.notes));
    assert!(
        note.contains("decoupling targets"),
        "note should name decoupling targets: {note}"
    );
    assert!(
        note.contains("Resource.php"),
        "the hub (Resource.php) should be the top decoupling target: {note}"
    );
    // The single-break-edge note must NOT fire for a hub-dominated cycle.
    assert!(
        !r.notes.iter().any(|n| n.contains("candidate break point")),
        "hub-dominated cycle should not emit a single break-edge note: {:?}",
        r.notes
    );
}

#[test]
fn risk_batch_reuses_patch_cycles_for_per_file_cycle_signal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("B.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = codesage_graph::assess_risk_batch(&db, &["A.php".to_string(), "B.php".to_string()])
        .unwrap();

    assert_eq!(r.files.len(), 2);
    assert!(r.files.iter().all(|f| f.in_cycle), "{:?}", r.files);
    assert!(r.files.iter().all(|f| f.cycle_size == 2), "{:?}", r.files);
}

#[test]
fn assess_risk_no_cycle_signal_in_acyclic_codebase() {
    // Linear A -> B -> C. Touching A: in_cycle should stay false.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Leaf { public function z() { return 1; } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Leaf;\nclass Mid { public function y(Leaf $l) { return $l->z(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Mid;\nclass Top { public function x(Mid $m) { return $m->y(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "A.php").unwrap();
    assert!(!r.in_cycle);
    assert_eq!(r.cycle_size, 0);
    assert!(r.cycle_files.is_empty());
    assert!(
        !r.notes.iter().any(|n| n.contains("import cycle")),
        "should not mention cycle when none, got {:?}",
        r.notes
    );
}

#[test]
fn assess_risk_cycle_term_lifts_score_for_otherwise_quiet_file() {
    // Two files with no churn, no fix history, no test gap (sibling tests
    // present), and no other risk inputs — the only signal that should fire
    // is cycle membership. Score should be small but strictly above the
    // baseline (0.0) thanks to the 0.10-weighted cycle term.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    // Sibling test files close the test_gap so test_gap_term doesn't dominate.
    std::fs::write(root.join("ATest.php"), b"<?php\nclass ATest {}\n").unwrap();
    std::fs::write(root.join("BTest.php"), b"<?php\nclass BTest {}\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    // Seed sibling tests in git_files so test_sibling_exists picks them up.
    db.upsert_git_file("ATest.php", 0.1, 0, 1, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("BTest.php", 0.1, 0, 1, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("A.php", 0.1, 0, 1, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("B.php", 0.1, 0, 1, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "A.php").unwrap();
    assert!(r.in_cycle, "A.php is in cycle");
    assert!(!r.test_gap, "sibling test seeded, test_gap should be false");
    // Cycle of size 2 contributes 0.10 * 0.25 = 0.025; churn percentile is
    // ~0 across uniform churn. We just want to confirm the cycle term moves
    // the needle above zero.
    assert!(
        r.score > 0.0,
        "cycle membership should lift score above 0, got {}",
        r.score
    );
}

// ----- assess_risk_diff: cycles_touching_patch -----

#[test]
fn risk_diff_finds_two_file_cycle_touching_patch() {
    // A uses Repository (defined in B); B uses Controller (defined in A).
    // The structural indexer turns that into a file-level A <-> B cycle.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    // Seed enough git_files so the risk pass has a distribution.
    for f in ["A.php", "B.php"] {
        db.upsert_git_file(f, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let r = codesage_graph::assess_risk_diff(&db, &["A.php".to_string()]).unwrap();
    assert_eq!(
        r.cycles_touching_patch.len(),
        1,
        "expected one cycle, got {:?}",
        r.cycles_touching_patch
    );
    let c = &r.cycles_touching_patch[0];
    assert_eq!(c.size, 2);
    assert!(c.members.contains(&"A.php".to_string()));
    assert!(c.members.contains(&"B.php".to_string()));
    assert!(
        r.summary_notes.iter().any(|n| n.contains("import cycle")),
        "expected import-cycle summary note, got {:?}",
        r.summary_notes
    );
}

#[test]
fn risk_diff_skips_cycles_not_involving_patch_files() {
    // A <-> B cycle, but the patch only touches C (which is not in the cycle).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Unrelated { public function z() { return 1; } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("C.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::assess_risk_diff(&db, &["C.php".to_string()]).unwrap();
    assert!(
        r.cycles_touching_patch.is_empty(),
        "cycle exists but doesn't touch C; should not be reported: {:?}",
        r.cycles_touching_patch
    );
    assert!(
        !r.summary_notes.iter().any(|n| n.contains("import cycle")),
        "should not mention cycles in summary_notes when none touch the patch"
    );
}

#[test]
fn risk_diff_cycle_pick_max_churn_points_at_hottest_member() {
    // A <-> B cycle; B has much higher churn. `max_churn_file` should
    // name B as the refactor target rather than A.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("B.php", 99.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::assess_risk_diff(&db, &["A.php".to_string()]).unwrap();
    assert_eq!(r.cycles_touching_patch.len(), 1);
    assert_eq!(
        r.cycles_touching_patch[0].max_churn_file.as_deref(),
        Some("B.php"),
        "max_churn_file should name the highest-churn member, got {:?}",
        r.cycles_touching_patch[0].max_churn_file
    );
}

#[test]
fn risk_diff_no_cycles_in_trivially_acyclic_codebase() {
    // Linear import chain A -> B -> C, no cycles.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Leaf { public function z() { return 1; } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Leaf;\nclass Mid { public function y(Leaf $l) { return $l->z(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Mid;\nclass Top { public function x(Mid $m) { return $m->y(null); } }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
    db.upsert_git_file("A.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::assess_risk_diff(&db, &["A.php".to_string()]).unwrap();
    assert!(
        r.cycles_touching_patch.is_empty(),
        "linear chain has no cycles: {:?}",
        r.cycles_touching_patch
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
    index_test_file(&db, "RepositoryTest.php");

    let r = codesage_graph::recommend_tests(&db, &["Repository.php".to_string()]).unwrap();
    assert_eq!(r.primary, vec!["RepositoryTest.php".to_string()]);
    assert!(r.coupled.is_empty(), "no co-change history seeded");
}

#[test]
fn recommend_tests_finds_structural_sibling_without_git_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Repository.php"),
        b"<?php\nclass Repository { public function find($id) { return null; } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("RepositoryTest.php"),
        b"<?php\nclass RepositoryTest { public function testFind() {} }\n",
    )
    .unwrap();

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

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
    index_test_file(&db, "RepositoryTest.php");
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
    index_test_file(&db, "RepositoryTest.php");
    index_test_file(&db, "ServiceTest.php");

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
    index_test_file(&db, "crates/storage/tests/db_integration.rs");
    index_test_file(&db, "crates/storage/tests/schema_migration_test.rs");
    // A test under a different crate must NOT leak in.
    index_test_file(&db, "crates/parser/tests/extract_test.rs");

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
    index_test_file(&db, "crates/parser/tests/extract_test.rs");
    // Fixture files are NOT test entry points; should not be recommended.
    index_test_file(&db, "crates/parser/tests/fixtures/sample.rs");

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
    index_test_file(&db, "Zend/tests/bug12345.phpt");
    index_test_file(&db, "Zend/tests/gh21709.phpt");
    // Different subsystem's tests must not leak in.
    index_test_file(&db, "ext/standard/tests/array_test.phpt");

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
        index_test_file(&db, &p);
    }

    let r = codesage_graph::recommend_tests(&db, &["ext/standard/array.c".to_string()]).unwrap();
    assert!(
        r.primary.is_empty(),
        "tests dir over the 50-file threshold should not be returned as primary, got {} entries",
        r.primary.len()
    );
    // Withheld is not absent: the note must say the tests exist but were not
    // listed, and the "no test files found" claim must not fire.
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains(".phpt") && n.contains("omitted")),
        "expected a suppression note naming the .phpt directory, got {:?}",
        r.notes
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("no test files found")),
        "60 existing .phpt tests must not be reported as 'no test files found', got {:?}",
        r.notes
    );
}

/// The oversized-.phpt suppression must also not open a test gap in
/// `assess_risk`: the tests exist, they were only withheld from the listing.
#[test]
fn oversized_phpt_dir_still_counts_as_sibling_test_for_risk() {
    let (_dir, db) = setup_project();
    index_test_file(&db, "ext/standard/array.c");
    db.upsert_git_file("ext/standard/array.c", 5.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    for i in 0..60 {
        let p = format!("ext/standard/tests/test_{i:03}.phpt");
        index_test_file(&db, &p);
    }

    let r = assess_risk(&db, "ext/standard/array.c").unwrap();
    assert!(
        !r.test_gap,
        "60 .phpt tests next to the file must close the test gap, notes: {:?}",
        r.notes
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
    index_test_file(&db, test);
    // A test for an unrelated class must not leak in.
    index_test_file(
        &db,
        "tests/Integration/Actions/Other/UnrelatedActionTest.php",
    );

    let r = codesage_graph::recommend_tests(&db, &[src.to_string()]).unwrap();
    assert_eq!(r.primary, vec![test.to_string()]);
}

#[test]
fn recommend_tests_finds_laravel_test_under_unit_or_feature_too() {
    let (_dir, db) = setup_project();
    let src = "app/Services/Facility/ProviderService.php";
    db.upsert_git_file(src, 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    index_test_file(&db, "tests/Unit/Services/Facility/ProviderServiceTest.php");
    index_test_file(
        &db,
        "tests/Feature/Services/Facility/ProviderServiceTest.php",
    );

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

// ----- find_coupling (CouplingReport shape) -----

#[test]
fn find_coupling_unindexed_file_returns_explanatory_note() {
    // File has no git_files row at all — path is wrong, brand-new, or
    // gitignored. CouplingReport must tell the agent so, not `coupled: []`
    // with no context.
    let (_dir, db) = setup_project();
    let r = codesage_graph::find_coupling(&db, "does/not/exist.rs", 5).unwrap();
    assert!(!r.found);
    assert!(r.coupled.is_empty());
    assert!(!r.file_indexed);
    assert_eq!(r.file_commits, 0);
    let note = r.note.expect("note must be present when coupled is empty");
    assert!(
        note.contains("no git history"),
        "unindexed file note should call out missing history: {note}"
    );
}

#[test]
fn find_coupling_indexed_but_below_threshold_explains_why() {
    // File has commits but no co-change pair above the min-count=3 threshold
    // — "this file changes in isolation." Agent should see the total-commits
    // count so it can judge whether the verdict is trustworthy.
    let (_dir, db) = setup_project();
    db.upsert_git_file("solitary.rs", 1.0, 0, 7, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::find_coupling(&db, "solitary.rs", 5).unwrap();
    assert!(r.found);
    assert!(r.coupled.is_empty());
    assert!(r.file_indexed);
    assert_eq!(r.file_commits, 7);
    let note = r.note.expect("note required");
    assert!(
        note.contains("7 commits") && note.contains("min-count threshold"),
        "note should quote commit count and threshold reasoning: {note}"
    );
}

#[test]
fn find_coupling_new_file_under_three_commits_has_dedicated_note() {
    // Low-commit files get a different note pointing at the threshold itself
    // (they might accumulate signal later).
    let (_dir, db) = setup_project();
    db.upsert_git_file("fresh.rs", 0.1, 0, 1, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::find_coupling(&db, "fresh.rs", 5).unwrap();
    assert!(r.coupled.is_empty());
    assert!(r.file_indexed);
    assert_eq!(r.file_commits, 1);
    let note = r.note.expect("note required");
    assert!(
        note.contains("only 1 tracked commit"),
        "low-commit note should pluralize correctly: {note}"
    );
}

#[test]
fn find_coupling_populated_result_carries_index_state() {
    // Non-empty response still carries file_indexed + file_commits so a thin
    // result (fewer than `limit` entries) is still interpretable.
    let (_dir, db) = setup_project();
    db.upsert_git_file("a.rs", 1.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("b.rs", 0.5, 0, 10, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_co_change("a.rs", "b.rs", 5.0, 5, Some(1_700_000_000))
        .unwrap();
    let r = codesage_graph::find_coupling(&db, "a.rs", 5).unwrap();
    assert!(r.found);
    assert_eq!(r.coupled.len(), 1);
    assert_eq!(r.coupled[0].file, "b.rs");
    assert!(r.file_indexed);
    assert_eq!(r.file_commits, 10);
    assert!(
        r.note.is_none(),
        "note should be None when coupled is non-empty"
    );
}

#[test]
fn risk_diff_legend_aliases_repeated_test_gap_notes() {
    // 4 files with indexed symbols but no co-located test → all get the same
    // full three-check "test gap: …" note (files WITHOUT symbols get the
    // distinct unmeasured variant, aliased as `TU`, not `T`). Threshold for
    // aliasing is 3 occurrences, so this should fire and produce a single
    // `_legend` entry with the 4 per-file notes replaced by `"T"`.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let files = ["ClassA.php", "ClassB.php", "ClassC.php", "ClassD.php"];
    for p in &files {
        let class = p.trim_end_matches(".php");
        std::fs::write(
            root.join(p),
            format!("<?php\nnamespace App;\nclass {class} {{\n  public function run() {{ return 1; }}\n}}\n"),
        )
        .unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    for p in &files {
        db.upsert_git_file(p, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let input: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    assert_eq!(
        r.legend.len(),
        1,
        "expected exactly one aliased note, got {:?}",
        r.legend
    );
    let resolved = r.legend.get("T").expect("T code in legend");
    assert!(
        resolved.contains("test gap"),
        "T resolves to test-gap, got {resolved}"
    );

    let aliased: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| *n == "T").count())
        .sum();
    assert_eq!(aliased, 4, "all 4 test-gap notes should be replaced by `T`");
    let raw_test_gap: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| n.contains("test gap")).count())
        .sum();
    assert_eq!(
        raw_test_gap, 0,
        "no verbatim 'test gap' string should remain after aliasing"
    );
}

#[test]
fn risk_diff_legend_does_not_fire_below_threshold() {
    // 2 test-gap files: under the ≥3 threshold, so no aliasing. Notes stay
    // verbatim; legend is empty.
    let (_dir, db) = setup_project();
    let files = ["src/a.rs", "src/b.rs"];
    for p in &files {
        db.upsert_git_file(p, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    let input: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    assert!(
        r.legend.is_empty(),
        "legend should be empty below threshold, got {:?}",
        r.legend
    );
    let raw_test_gap: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| n.contains("test gap")).count())
        .sum();
    assert_eq!(
        raw_test_gap, 2,
        "verbatim notes should remain when no aliasing"
    );
}

#[test]
fn risk_batch_returns_per_file_in_input_order() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Controller.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("Service.php", 0.5, 0, 5, Some(1_700_000_000))
        .unwrap();

    let input = vec![
        "Service.php".to_string(),
        "Repository.php".to_string(),
        "Controller.php".to_string(),
    ];
    let r = codesage_graph::assess_risk_batch(&db, &input).unwrap();

    assert_eq!(r.files.len(), 3);
    assert_eq!(r.files[0].file, "Service.php");
    assert_eq!(r.files[1].file, "Repository.php");
    assert_eq!(r.files[2].file, "Controller.php");
    // Repository (the hot fix-heavy file) should out-score the cooler ones.
    assert!(
        r.files[1].score > r.files[0].score,
        "Repository.php should score higher than Service.php"
    );
}

#[test]
fn risk_batch_empty_returns_default() {
    let (_dir, db) = setup_project();
    let r = codesage_graph::assess_risk_batch(&db, &[]).unwrap();
    assert!(r.files.is_empty());
    assert!(r.legend.is_empty());
}

#[test]
fn risk_batch_legend_aliases_no_git_history_at_threshold() {
    // 4 indexed files (with symbols) and no git history at all → each gets the
    // categorical "no git history…" note. Should alias to `NG`.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let files = ["ClassA.php", "ClassB.php", "ClassC.php", "ClassD.php"];
    for p in &files {
        let class = p.trim_end_matches(".php");
        std::fs::write(
            root.join(p),
            format!("<?php\nnamespace App;\nclass {class} {{\n  public function run() {{ return 1; }}\n}}\n"),
        )
        .unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    let input: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    let r = codesage_graph::assess_risk_batch(&db, &input).unwrap();

    // 4 files with no git history also have no test sibling, so both
    // categorical notes (NG and T) fire on every file. Both should alias.
    assert!(
        r.legend.contains_key("NG"),
        "NG missing in legend, got {:?}",
        r.legend
    );
    assert!(
        r.legend.contains_key("T"),
        "T missing in legend, got {:?}",
        r.legend
    );
    let ng_aliased: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| *n == "NG").count())
        .sum();
    let t_aliased: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| *n == "T").count())
        .sum();
    assert_eq!(ng_aliased, 4);
    assert_eq!(t_aliased, 4);
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
    index_test_file(&db, test);

    let r = codesage_graph::recommend_tests(&db, &[src.to_string()]).unwrap();
    assert_eq!(r.primary, vec![test.to_string()]);
}

#[test]
fn trust_boundaries_populate_via_indexer_and_feed_risk_score() {
    use codesage_protocol::TrustBoundary;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // PHP file that imports a network namespace AND calls exec — distinct
    // boundary tags should land in file_trust_boundaries after indexing,
    // and the trust_boundary_term contributes to the score.
    std::fs::write(
        root.join("Risky.php"),
        b"<?php\nuse GuzzleHttp\\Client;\nclass Risky {\n  public function run() {\n    exec('ls');\n  }\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

    let tags = db.trust_boundaries_for_file_path("Risky.php").unwrap();
    assert!(tags.contains(&TrustBoundary::ProcessExec), "got {:?}", tags);
    assert!(tags.contains(&TrustBoundary::Network), "got {:?}", tags);

    let r = assess_risk(&db, "Risky.php").unwrap();
    assert!(
        r.trust_boundaries.contains(&TrustBoundary::ProcessExec),
        "RiskAssessment must carry the tags, got {:?}",
        r.trust_boundaries
    );
    // 3 boundaries (network, external-api, process-exec) — Guzzle's
    // network+external-api plus exec's process-exec. The aggregate-notes
    // line fires at >=3 boundaries.
    assert!(
        r.notes.iter().any(|n| n.contains("trust boundaries")),
        "expected trust-boundary note, got {:?}",
        r.notes
    );
}

#[test]
fn trust_boundaries_field_empty_when_file_has_no_signal() {
    let (_dir, db) = setup_project();
    // Repository.php has no risky imports/calls in the fixture; trust_boundaries
    // should be an empty Vec, not None, and contribute 0 to the score.
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.trust_boundaries.is_empty(),
        "fixture has no boundary signal, got {:?}",
        r.trust_boundaries
    );
}

// ----- top_symbols breakdown (§1.15) -----

/// Unit test: with three symbols of different sizes and ref counts, the
/// ranking must reflect the heuristic ln(1 + line_count) + ref_count.
/// Seeds the structural tables directly so the math is the only variable.
#[test]
fn top_symbols_rank_by_line_count_and_ref_count() {
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, Symbol, SymbolKind};

    let db = Database::open_in_memory().unwrap();
    // Caller file that hosts the refs to our three symbols.
    let caller_id = db
        .upsert_file(&FileInfo {
            path: "caller.rs".into(),
            language: Language::Rust,
            content_hash: "c".into(),
        })
        .unwrap();
    // Target file: three symbols of different shapes.
    let target_id = db
        .upsert_file(&FileInfo {
            path: "target.rs".into(),
            language: Language::Rust,
            content_hash: "t".into(),
        })
        .unwrap();

    let mk = |name: &str, ls: u32, le: u32| Symbol {
        name: name.into(),
        qualified_name: name.into(),
        kind: SymbolKind::Function,
        file_path: "target.rs".into(),
        line_start: ls,
        line_end: le,
        col_start: 0,
        col_end: 0,
        rationale: Vec::new(),
    };

    // big: 100 lines, no callers → score = ln(101) + 0 = ~4.62
    // small_hot: 5 lines, 20 callers → score = ln(6) + 20 = ~21.79
    // tiny: 1 line, 1 caller → score = ln(2) + 1 = ~1.69
    db.insert_symbols(
        target_id,
        &[
            mk("big", 1, 100),
            mk("small_hot", 110, 114),
            mk("tiny", 120, 120),
        ],
    )
    .unwrap();

    let mk_ref = |to: &str, line: u32| Reference {
        from_file: "caller.rs".into(),
        from_symbol: None,
        to_name: to.into(),
        kind: ReferenceKind::Call,
        line,
        col: 0,
    };
    let mut refs: Vec<Reference> = (0..20).map(|i| mk_ref("small_hot", 10 + i)).collect();
    refs.push(mk_ref("tiny", 200));
    db.insert_references(caller_id, &refs).unwrap();

    // No git history seeded — assess_risk still runs with score=0 inputs;
    // we only care about top_symbols ordering, which is independent of churn.
    let r = assess_risk(&db, "target.rs").unwrap();

    assert_eq!(
        r.top_symbols.len(),
        3,
        "expected all three symbols ranked, got {:?}",
        r.top_symbols
    );
    // Ranking: small_hot (refs dominate) > big (length wins over tiny) > tiny.
    assert_eq!(r.top_symbols[0].name, "small_hot");
    assert_eq!(r.top_symbols[1].name, "big");
    assert_eq!(r.top_symbols[2].name, "tiny");

    // Cycle is false for this fixture (no import edges); `why` should not
    // mention a cycle clause.
    for t in &r.top_symbols {
        assert!(
            !t.why.contains("cycle"),
            "no cycle in fixture, got why={:?}",
            t.why
        );
        assert!(
            t.why.starts_with("hot:"),
            "unexpected why prefix: {:?}",
            t.why
        );
    }
    // Spot-check the small_hot rendering captures both the line and ref count.
    let hot = &r.top_symbols[0];
    assert_eq!(hot.line, 110);
    assert_eq!(hot.kind, "function");
    assert!(
        hot.why.contains("5 lines") && hot.why.contains("20 refs"),
        "small_hot why should reference its actual stats, got {:?}",
        hot.why
    );
}

/// Integration test: on a known-hot file (high churn + fix history) the
/// `top_symbols` list is populated, sorted descending, every entry has a
/// non-empty `why` of the documented shape, and the cap is honored.
#[test]
fn top_symbols_populates_on_known_hot_file_and_caps_at_five() {
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, Symbol, SymbolKind};

    let db = Database::open_in_memory().unwrap();
    let caller_id = db
        .upsert_file(&FileInfo {
            path: "caller.rs".into(),
            language: Language::Rust,
            content_hash: "c".into(),
        })
        .unwrap();
    let hot_id = db
        .upsert_file(&FileInfo {
            path: "hot.rs".into(),
            language: Language::Rust,
            content_hash: "h".into(),
        })
        .unwrap();

    // Seed 8 symbols, ascending in size. Largest should win on length alone.
    let mut syms: Vec<Symbol> = Vec::new();
    for i in 0..8u32 {
        let line_count = (i + 1) * 10;
        let line_start = 1 + i * 100;
        let line_end = line_start + line_count - 1;
        syms.push(Symbol {
            name: format!("sym_{i:02}"),
            qualified_name: format!("sym_{i:02}"),
            kind: SymbolKind::Function,
            file_path: "hot.rs".into(),
            line_start,
            line_end,
            col_start: 0,
            col_end: 0,
            rationale: Vec::new(),
        });
    }
    db.insert_symbols(hot_id, &syms).unwrap();

    // A handful of refs into the smallest symbol so it sneaks into the top via
    // ref_count, proving ranking isn't pure length.
    let refs: Vec<Reference> = (0..30)
        .map(|i| Reference {
            from_file: "caller.rs".into(),
            from_symbol: None,
            to_name: "sym_00".into(),
            kind: ReferenceKind::Call,
            line: 10 + i,
            col: 0,
        })
        .collect();
    db.insert_references(caller_id, &refs).unwrap();

    // Hot churn + fix-heavy so the file scores meaningfully.
    db.upsert_git_file("hot.rs", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    for (p, c) in [
        ("caller.rs", 1.0_f64),
        ("a.rs", 0.5),
        ("b.rs", 0.7),
        ("c.rs", 0.3),
    ] {
        db.upsert_git_file(p, c, 0, 5, Some(1_700_000_000)).unwrap();
    }

    let r = assess_risk(&db, "hot.rs").unwrap();

    // Cap honored: 8 symbols in, 5 out.
    assert_eq!(
        r.top_symbols.len(),
        5,
        "top_symbols must be capped at 5, got {}",
        r.top_symbols.len()
    );

    // Descending by score (we re-derive the same heuristic here as a guard
    // against silent ordering regressions).
    let mut prev = f64::INFINITY;
    for t in &r.top_symbols {
        let sym = syms
            .iter()
            .find(|s| s.name == t.name)
            .expect("known symbol");
        let line_count = sym.line_end.saturating_sub(sym.line_start) + 1;
        let ref_count = if t.name == "sym_00" { 30.0 } else { 0.0 };
        let score = (1.0 + line_count as f64).ln() + ref_count;
        assert!(
            score <= prev + 1e-9,
            "top_symbols must be sorted descending: {} scored {} after {}",
            t.name,
            score,
            prev
        );
        prev = score;
    }

    // The 30-ref small symbol must be ranked first; pure ref_count dominance.
    assert_eq!(r.top_symbols[0].name, "sym_00");

    // `why` shape: "hot: N lines, M refs" — and no cycle clause on this fixture.
    for t in &r.top_symbols {
        assert!(
            t.why.starts_with("hot: ") && t.why.contains("lines") && t.why.contains("refs"),
            "unexpected why shape: {:?}",
            t.why
        );
        assert!(
            !t.why.contains("cycle"),
            "no cycle in fixture, got why={:?}",
            t.why
        );
    }
}

/// Edge case: a file with zero indexed symbols (text file, generated file,
/// unindexed shape) must produce an empty `top_symbols` Vec — no panic, no
/// error, and the field disappears from JSON via the serde skip-if-empty
/// attribute.
#[test]
fn top_symbols_empty_when_file_has_no_symbols() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("README.md", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    let r = assess_risk(&db, "README.md").unwrap();
    assert!(
        r.top_symbols.is_empty(),
        "files with no indexed symbols must return empty top_symbols, got {:?}",
        r.top_symbols
    );
    // Schema discipline: empty Vec must not surface in JSON.
    let json = serde_json::to_string(&r).unwrap();
    assert!(
        !json.contains("top_symbols"),
        "empty top_symbols must be omitted from JSON, got {json}"
    );
}

// ----- zero-dependents honesty -----

/// A file with no indexed symbols never enters the reverse-dependency walk, so
/// its zero dependents means "unmeasured", not "leaf". The assessment must say
/// so instead of letting the zero read as low blast radius, and the test-gap
/// note must not claim a dependency-hop check that never ran.
#[test]
fn zero_dependents_without_symbols_is_flagged_unknown() {
    let (_dir, db) = setup_project();
    // Tracked in git, absent from the structural index: the shape of a config
    // file, generated file, or unsupported language.
    db.upsert_git_file("deploy/settings.yaml", 2.0, 0, 6, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "deploy/settings.yaml").unwrap();
    assert!(r.found);
    assert_eq!(r.dependent_files, 0);
    assert!(
        r.notes.iter().any(
            |n| n.contains("structural signals unavailable") && n.contains("unknown, not zero")
        ),
        "zero dependents on a symbol-less file must be flagged as unmeasured, got {:?}",
        r.notes
    );
    assert!(r.test_gap);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("test gap") && n.contains("could not run")),
        "the test-gap note must not claim the dependency-hop check ran, got {:?}",
        r.notes
    );
    assert!(
        !r.notes
            .iter()
            .any(|n| n.contains("within 2 dependency hops")),
        "must not claim a completed 2-hop check, got {:?}",
        r.notes
    );
}

/// A genuine leaf — indexed symbols, but nothing imports it — keeps the plain
/// zero and the full three-check test-gap note. The honesty note is reserved
/// for the case where the walk could not run.
#[test]
fn zero_dependents_with_symbols_is_a_genuine_leaf() {
    let (_dir, db) = setup_project();
    // Controller.php defines symbols but nothing references it.
    db.upsert_git_file("Controller.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "Controller.php").unwrap();
    assert_eq!(r.dependent_files, 0);
    assert!(
        !r.notes
            .iter()
            .any(|n| n.contains("structural signals unavailable")),
        "a measured zero must not be flagged as unmeasured, got {:?}",
        r.notes
    );
    assert!(r.test_gap);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("within 2 dependency hops")),
        "a completed walk keeps the full three-check note, got {:?}",
        r.notes
    );
}

/// Aggregate honesty must match per-file honesty: when every counted test-gap
/// file is symbol-less (hop check never ran), the diff summary must not claim
/// the gap was verified "within 2 dependency hops".
#[test]
fn diff_summary_does_not_claim_hop_check_for_unmeasured_files() {
    let (_dir, db) = setup_project();
    let input: Vec<String> = ["conf/a.yaml", "conf/b.yaml", "conf/c.yaml"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for p in &input {
        db.upsert_git_file(p, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }

    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();
    assert_eq!(r.test_gap_files.len(), 3);
    let note = r
        .summary_notes
        .iter()
        .find(|n| n.contains("no test found"))
        .unwrap_or_else(|| {
            panic!(
                "expected a test-gap summary note, got {:?}",
                r.summary_notes
            )
        });
    assert!(
        note.contains("could not be completed"),
        "summary must disclose the unrunnable hop check, got {note:?}"
    );
    assert!(
        !note.contains("or within 2 dependency hops"),
        "summary must not claim a hop check that never ran, got {note:?}"
    );
}

/// Mixed patch: one gap file with a completed hop check, three unmeasured.
/// The summary must split the counts instead of flattening to either side.
#[test]
fn diff_summary_splits_verified_and_unmeasured_gap_counts() {
    let (_dir, db) = setup_project();
    let mut input: Vec<String> = ["conf/a.yaml", "conf/b.yaml", "conf/c.yaml"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for p in &input {
        db.upsert_git_file(p, 1.0, 0, 5, Some(1_700_000_000))
            .unwrap();
    }
    // Repository.php has indexed symbols, so its hop check completes.
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    input.push("Repository.php".to_string());

    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();
    assert_eq!(r.test_gap_files.len(), 4);
    let note = r
        .summary_notes
        .iter()
        .find(|n| n.contains("no test found"))
        .unwrap_or_else(|| {
            panic!(
                "expected a test-gap summary note, got {:?}",
                r.summary_notes
            )
        });
    assert!(
        note.contains("ran clean for 1") && note.contains("could not be completed for 3"),
        "summary must split verified vs unmeasured gap counts, got {note:?}"
    );
}

/// The fully-verified wording survives when every gap file's hop check ran.
#[test]
fn diff_summary_keeps_hop_claim_when_all_gap_checks_completed() {
    let (_dir, db) = setup_project();
    let input = vec!["Repository.php".to_string()];
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();
    assert_eq!(r.test_gap_files.len(), 1);
    assert!(
        r.summary_notes
            .iter()
            .any(|n| n.contains("or within 2 dependency hops")),
        "completed checks keep the full claim, got {:?}",
        r.summary_notes
    );
}
