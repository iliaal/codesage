//! assess_risk composition tests. Seeds git_files + git_co_changes directly so
//! the score inputs are controlled (bypasses the real git log indexer).

use codesage_graph::{assess_risk, full_index};
use codesage_protocol::{FileInfo, Language};
use codesage_storage::Database;

/// One class, two callers, and one test supply the structural risk signals.
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
    } else if path.ends_with(".java") {
        Language::Java
    } else if path.ends_with(".cpp")
        || path.ends_with(".hpp")
        || path.ends_with(".cc")
        || path.ends_with(".cxx")
    {
        Language::Cpp
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
fn missing_path_is_unknown_not_test_gap() {
    let (_dir, db) = setup_project();

    let r = assess_risk(&db, "NoSuch.php").unwrap();

    assert!(!r.found);
    assert_eq!(r.score, 0.0);
    assert!(!r.test_gap);
    assert!(r.notes.iter().any(|n| n.contains("not indexed")));
    // Nothing was measured, so the 0.0 is a placeholder, not a low score.
    assert!(r.unscored, "an unknown path measures nothing");
    assert!(
        r.notes.iter().any(|n| n.contains("placeholder")),
        "expected the placeholder-score note, got {:?}",
        r.notes
    );
}

#[test]
fn hotspot_fix_heavy_file_scores_high_and_emits_notes() {
    let (_dir, db) = setup_project();

    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
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
    db.upsert_git_file("Repository.php", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    index_test_file(&db, "RepositoryTest.php");
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

/// Structural test reachability closes gaps missed by convention and co-change history.
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

/// Scope the absence claim to the checks that ran.
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
    // Extra git rows do not add dependency edges to the two-caller fixture.
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
    // Clustering removes display detail, not membership in risk rollups.
    let (_dir, db) = setup_project();
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
    assert!(r.max_score >= 0.5);
    assert!(
        r.summary_notes.iter().any(|n| n.contains("max risk score")),
        "expected explicit max-score warning, got {:?}",
        r.summary_notes
    );
}

#[test]
fn assess_risk_flags_file_in_two_file_cycle() {
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
    db.upsert_git_co_change("A.php", "B.php", 2.5, 4, Some(1_700_000_000))
        .unwrap();

    let r = assess_risk(&db, "A.php").unwrap();
    assert!(r.in_cycle);
    let note = r
        .notes
        .iter()
        .find(|n| n.contains("candidate break point"))
        .unwrap_or_else(|| panic!("expected break-point note, got {:?}", r.notes));
    assert!(note.contains("A.php"), "note: {note}");
    assert!(note.contains("B.php"), "note: {note}");
    assert!(
        note.contains("2.50"),
        "expected co-change weight in note: {note}"
    );
}

#[test]
fn assess_risk_reframes_hub_dominated_cycle_as_decoupling_targets() {
    // Cutting one edge cannot break every cycle in a hub-and-spoke graph.
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
    // Isolate cycle pressure by removing churn, fixes, and test gaps.
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
    std::fs::write(root.join("ATest.php"), b"<?php\nclass ATest {}\n").unwrap();
    std::fs::write(root.join("BTest.php"), b"<?php\nclass BTest {}\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root, &db, &[], false).unwrap();
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
    assert!(
        r.score > 0.0,
        "cycle membership should lift score above 0, got {}",
        r.score
    );
}

#[test]
fn risk_diff_finds_two_file_cycle_touching_patch() {
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
    // Rust integration tests are crate-scoped, without per-source-file names.
    db.upsert_git_file("crates/storage/src/db.rs", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    index_test_file(&db, "crates/storage/tests/db_integration.rs");
    index_test_file(&db, "crates/storage/tests/schema_migration_test.rs");
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
    db.upsert_git_file("Zend/zend_compile.c", 5.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    index_test_file(&db, "Zend/tests/bug12345.phpt");
    index_test_file(&db, "Zend/tests/gh21709.phpt");
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
    // Withheld tests still disprove an absence claim.
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
fn recommend_tests_finds_java_maven_mirror_test() {
    let (_dir, db) = setup_project();
    index_test_file(&db, "src/test/java/com/app/FooTest.java");
    index_test_file(&db, "src/test/java/com/app/BarTest.java");

    let r = codesage_graph::recommend_tests(&db, &["src/main/java/com/app/Foo.java".to_string()])
        .unwrap();
    assert_eq!(
        r.primary,
        vec!["src/test/java/com/app/FooTest.java".to_string()]
    );
}

#[test]
fn recommend_tests_finds_c_and_cpp_affix_siblings() {
    let (_dir, db) = setup_project();
    index_test_file(&db, "src/foo_test.c");
    index_test_file(&db, "tests/test_bar.cpp");
    index_test_file(&db, "tests/unrelated_test.c");

    let r =
        codesage_graph::recommend_tests(&db, &["src/foo.c".to_string(), "src/bar.cpp".to_string()])
            .unwrap();
    assert!(r.primary.contains(&"src/foo_test.c".to_string()));
    assert!(r.primary.contains(&"tests/test_bar.cpp".to_string()));
    assert!(
        !r.primary.contains(&"tests/unrelated_test.c".to_string()),
        "unrelated C test must not leak in: {:?}",
        r.primary
    );
}

#[test]
fn recommend_tests_first_dot_stem_matches_dotted_basename() {
    let (_dir, db) = setup_project();
    index_test_file(&db, "src/foo.test.ts");

    let r = codesage_graph::recommend_tests(&db, &["src/foo.ts".to_string()]).unwrap();
    assert_eq!(r.primary, vec!["src/foo.test.ts".to_string()]);

    let r = codesage_graph::recommend_tests(&db, &["src/foo.test.ts".to_string()]).unwrap();
    assert!(
        !r.primary.contains(&"src/foo.test.ts".to_string()),
        "edited test file must not recommend itself: {:?}",
        r.primary
    );
}

#[test]
fn recommend_tests_finds_laravel_mirror_tree_tests() {
    let (_dir, db) = setup_project();
    // Nested Laravel test paths require mirror-tree matching, not flat siblings.
    let src = "app/Actions/CredentialingApplication/ExportZipAction.php";
    let test = "tests/Integration/Actions/CredentialingApplication/ExportZipActionTest.php";
    db.upsert_git_file(src, 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();
    index_test_file(&db, test);
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

#[test]
fn find_coupling_unindexed_file_returns_explanatory_note() {
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
    let (_dir, db) = setup_project();
    db.upsert_git_file("a.rs", 1.0, 0, 10, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("b.rs", 0.5, 0, 10, Some(1_700_000_000))
        .unwrap();
    // Seed recurrence so this case does not exercise the one-off-page note.
    db.upsert_git_co_change_full(
        "a.rs",
        "b.rs",
        &codesage_storage::db::CoChangeWrite {
            weight: 5.0,
            count: 5,
            window_mask: 0b11,
            first_observed_at: Some(1_690_000_000),
            last_observed_at: Some(1_700_000_000),
        },
    )
    .unwrap();
    let r = codesage_graph::find_coupling(&db, "a.rs", 5).unwrap();
    assert!(r.found);
    assert_eq!(r.coupled.len(), 1);
    assert_eq!(r.coupled[0].file, "b.rs");
    assert!(r.file_indexed);
    assert_eq!(r.file_commits, 10);
    assert!(
        r.note.is_none(),
        "note should be None when coupled is non-empty and recurring"
    );
}

#[test]
fn risk_diff_legend_aliases_repeated_test_gap_notes() {
    // Indexed symbols allow the full three-check note; symbol-less files use `TU`.
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
    // Guzzle contributes network and external-api; exec adds process-exec.
    assert!(
        r.notes.iter().any(|n| n.contains("trust boundaries")),
        "expected trust-boundary note, got {:?}",
        r.notes
    );
}

#[test]
fn trust_boundaries_field_empty_when_file_has_no_signal() {
    let (_dir, db) = setup_project();
    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.trust_boundaries.is_empty(),
        "fixture has no boundary signal, got {:?}",
        r.trust_boundaries
    );
}

/// Seed structural rows to isolate the symbol-ranking formula.
#[test]
fn top_symbols_rank_by_line_count_and_ref_count() {
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, Symbol, SymbolKind};

    let db = Database::open_in_memory().unwrap();
    let caller_id = db
        .upsert_file(&FileInfo {
            path: "caller.rs".into(),
            language: Language::Rust,
            content_hash: "c".into(),
        })
        .unwrap();
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

    // Symbol ranking is independent of churn, so no git history is needed.
    let r = assess_risk(&db, "target.rs").unwrap();

    assert_eq!(
        r.top_symbols.len(),
        3,
        "expected all three symbols ranked, got {:?}",
        r.top_symbols
    );
    assert_eq!(r.top_symbols[0].name, "small_hot");
    assert_eq!(r.top_symbols[1].name, "big");
    assert_eq!(r.top_symbols[2].name, "tiny");

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
    let hot = &r.top_symbols[0];
    assert_eq!(hot.line, 110);
    assert_eq!(hot.kind, "function");
    assert!(
        hot.why.contains("5 lines") && hot.why.contains("20 refs"),
        "small_hot why should reference its actual stats, got {:?}",
        hot.why
    );
}

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

    // Calls to the smallest symbol distinguish ref-count ranking from length alone.
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

    assert_eq!(
        r.top_symbols.len(),
        5,
        "top_symbols must be capped at 5, got {}",
        r.top_symbols.len()
    );

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

    assert_eq!(r.top_symbols[0].name, "sym_00");

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
    let json = serde_json::to_string(&r).unwrap();
    assert!(
        !json.contains("top_symbols"),
        "empty top_symbols must be omitted from JSON, got {json}"
    );
}

/// A symbol-less file has no traversal seeds: zero dependents means unmeasured.
#[test]
fn zero_dependents_without_symbols_is_flagged_unknown() {
    let (_dir, db) = setup_project();
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

/// An indexed leaf retains a measured zero and the completed hop-check note.
#[test]
fn zero_dependents_with_symbols_is_a_genuine_leaf() {
    let (_dir, db) = setup_project();
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

/// Aggregate notes must not claim hop checks for symbol-less inputs.
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

/// Separate completed and unmeasured hop checks in a mixed patch.
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

/// Repository.php with the seeded history below: churn percentile 1.0 of five
/// rows, 40/80 fixes, two dependents (Controller + Service), no coupling, and
/// a test gap. Pins the measured composite so the unscored state cannot move
/// a score that WAS measured.
const REPOSITORY_SCORED: f64 = 0.32 * 1.0 + 0.18 * 0.5 + 0.09 * (2.0 / 20.0) + 0.13;

/// Controller.php: churn percentile 3/5, no fixes, no dependents, test gap.
const CONTROLLER_SCORED: f64 = 0.32 * 0.6 + 0.13;

/// Seed the five-row churn distribution the two constants above assume.
fn seed_hot_repository_history(db: &Database) {
    db.upsert_git_file("Repository.php", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    for (p, c) in [
        ("Controller.php", 1.0_f64),
        ("Service.php", 2.0),
        ("other_a.php", 0.5),
        ("other_b.php", 0.7),
    ] {
        db.upsert_git_file(p, c, 0, 5, Some(1_700_000_000)).unwrap();
    }
}

#[test]
fn no_git_row_is_unscored_not_a_low_score() {
    let (_dir, db) = setup_project();
    let r = assess_risk(&db, "Repository.php").unwrap();

    assert!(r.found, "the file is structurally indexed");
    assert!(
        r.unscored,
        "a file with no git_files row must report the unscored state, got score {}",
        r.score
    );
    assert_eq!(r.total_commits, 0);
    assert_eq!(r.fix_count, 0);
    assert_eq!(r.churn_percentile, 0.0);
    assert!(
        r.score < 0.2,
        "structural-only composite should stay low, got {}",
        r.score
    );
    assert!(
        r.notes.iter().any(|n| n.contains("no git history")),
        "expected 'no git history' note, got {:?}",
        r.notes
    );
    let unscored_note = r
        .notes
        .iter()
        .find(|n| n.starts_with("unscored:"))
        .unwrap_or_else(|| panic!("expected an `unscored:` note, got {:?}", r.notes));
    assert!(
        unscored_note.contains("unmeasured, not zero"),
        "the note must name the failure mode, got {unscored_note:?}"
    );
    assert!(
        unscored_note.contains("exclude this file"),
        "the note must say the aggregates skip this file, got {unscored_note:?}"
    );
}

#[test]
fn measured_file_keeps_its_score_and_no_unscored_state() {
    let (_dir, db) = setup_project();
    seed_hot_repository_history(&db);

    let r = assess_risk(&db, "Repository.php").unwrap();

    assert!(!r.unscored, "a file with a git_files row is measured");
    assert_eq!(r.churn_percentile, 1.0);
    assert_eq!(r.fix_ratio, 0.5);
    assert_eq!(r.dependent_files, 2);
    assert_eq!(r.coupled_files, 0);
    assert!(r.test_gap);
    assert!(r.trust_boundaries.is_empty());
    assert!(
        (r.score - REPOSITORY_SCORED).abs() < 1e-12,
        "measured score moved: {} != {REPOSITORY_SCORED}",
        r.score
    );
    assert!(
        !r.notes.iter().any(|n| n.starts_with("unscored:")),
        "a measured file must carry no unscored note, got {:?}",
        r.notes
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("no git history")),
        "a measured file must carry no missing-history note, got {:?}",
        r.notes
    );
}

#[test]
fn risk_diff_excludes_unscored_files_from_max_and_mean_but_still_lists_them() {
    let (_dir, db) = setup_project();
    seed_hot_repository_history(&db);
    // Indexed structurally, never seen by `git-index`: the excluded-path case.
    index_test_file(&db, "new_module.php");

    let input = vec![
        "Repository.php".to_string(),
        "Controller.php".to_string(),
        "new_module.php".to_string(),
    ];
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    assert_eq!(r.files.len(), 3, "no file may vanish from the response");
    let unscored: Vec<&codesage_protocol::RiskAssessment> =
        r.files.iter().filter(|f| f.unscored).collect();
    assert_eq!(unscored.len(), 1);
    assert_eq!(unscored[0].file, "new_module.php");
    assert_eq!(r.unscored_files, vec!["new_module.php".to_string()]);
    assert_eq!(r.scored_file_count, 2);

    let expected_mean = (REPOSITORY_SCORED + CONTROLLER_SCORED) / 2.0;
    assert!(
        (r.max_score - REPOSITORY_SCORED).abs() < 1e-12,
        "max must be the hot scored file, got {}",
        r.max_score
    );
    assert!(
        (r.mean_score - expected_mean).abs() < 1e-12,
        "mean must average the two scored files only, got {} != {expected_mean}",
        r.mean_score
    );
    assert_eq!(r.max_risk_file.as_deref(), Some("Repository.php"));

    // The pre-fix behavior: the unmeasured file's structural score dragged the mean down.
    let all_files_mean =
        (REPOSITORY_SCORED + CONTROLLER_SCORED + unscored[0].score) / r.files.len() as f64;
    assert!(
        (r.mean_score - all_files_mean).abs() > 0.05,
        "the exclusion must actually change the mean ({} vs {all_files_mean})",
        r.mean_score
    );

    let note = r
        .summary_notes
        .iter()
        .find(|n| n.contains("no indexed git history"))
        .unwrap_or_else(|| {
            panic!(
                "expected an unscored summary note, got {:?}",
                r.summary_notes
            )
        });
    assert!(
        note.contains("1 of 3 file(s)") && note.contains("new_module.php"),
        "the note must count and name the excluded files, got {note:?}"
    );
    assert!(
        note.contains("not low-risk"),
        "the note must refuse the safety reading, got {note:?}"
    );
}

/// A repository whose git history was never indexed: every file is unscored,
/// so the aggregates carry no measurement at all and must say so.
#[test]
fn risk_diff_over_repo_without_git_index_reports_nothing_scored() {
    let (_dir, db) = setup_project();

    let input = vec!["Repository.php".to_string(), "Controller.php".to_string()];
    let r = codesage_graph::assess_risk_diff(&db, &input).unwrap();

    assert!(!r.empty_input, "the caller did supply files");
    assert_eq!(r.files.len(), 2);
    assert_eq!(r.scored_file_count, 0);
    assert_eq!(r.max_score, 0.0);
    assert_eq!(r.mean_score, 0.0);
    assert!(
        r.max_risk_file.is_none(),
        "no scored file can be the riskiest, got {:?}",
        r.max_risk_file
    );
    assert_eq!(r.unscored_files.len(), 2);
    assert!(
        r.files.iter().all(|f| f.unscored),
        "every entry must carry the unscored state"
    );
    assert!(
        r.files.iter().any(|f| f.score > 0.0),
        "structural signals still score, which is exactly why max/mean must not average them"
    );
    let note = r
        .summary_notes
        .iter()
        .find(|n| n.contains("no indexed git history"))
        .unwrap_or_else(|| {
            panic!(
                "expected an unscored summary note, got {:?}",
                r.summary_notes
            )
        });
    assert!(
        note.contains("by convention rather than measurements")
            && note.contains("codesage git-index"),
        "the note must disown the 0.00 aggregates and give the remedy, got {note:?}"
    );
}

#[test]
fn risk_batch_agrees_with_assess_risk_on_scored_and_unscored_files() {
    let (_dir, db) = setup_project();
    seed_hot_repository_history(&db);
    index_test_file(&db, "new_module.php");

    // Two files keep note aliasing off, so `notes` compare verbatim.
    let input = vec!["Repository.php".to_string(), "new_module.php".to_string()];
    let batch = codesage_graph::assess_risk_batch(&db, &input).unwrap();

    assert!(batch.legend.is_empty(), "no aliasing below three files");
    assert_eq!(batch.files.len(), 2);
    for (path, batched) in input.iter().zip(batch.files.iter()) {
        let single = assess_risk(&db, path).unwrap();
        assert_eq!(&batched.file, path);
        assert_eq!(
            single.unscored, batched.unscored,
            "{path}: unscored state must match the per-file call"
        );
        assert_eq!(
            single.score.to_bits(),
            batched.score.to_bits(),
            "{path}: single {} != batch {}",
            single.score,
            batched.score
        );
        assert_eq!(single.notes, batched.notes, "{path}: notes must match");
    }
    assert!(!batch.files[0].unscored);
    assert!(batch.files[1].unscored);
    assert!(
        (batch.files[0].score - REPOSITORY_SCORED).abs() < 1e-12,
        "batch must reproduce the pinned measured score, got {}",
        batch.files[0].score
    );
}

/// The unscored note is aliasable, so a wide patch of new files stays cheap.
#[test]
fn risk_batch_aliases_the_unscored_note_at_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let files = ["ClassA.php", "ClassB.php", "ClassC.php"];
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

    let full = r
        .legend
        .get("US")
        .unwrap_or_else(|| panic!("US missing from legend, got {:?}", r.legend));
    assert!(full.starts_with("unscored:"), "US resolves to {full}");
    let aliased: usize = r
        .files
        .iter()
        .map(|f| f.notes.iter().filter(|n| *n == "US").count())
        .sum();
    assert_eq!(aliased, 3, "every unscored note should alias");
    assert!(
        r.files.iter().all(|f| f.unscored),
        "aliasing must not hide the structured flag"
    );
}

const DAY: i64 = 86_400;
/// Old enough that every wall-clock window in the risk pipeline excludes it,
/// and far enough past the 730-day boundary that the fixture cannot straddle
/// it mid-run.
const PINNED_AGE_DAYS: i64 = 1_000;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git command starts");
    assert!(status.success(), "git {args:?} failed");
}

/// Same isolation the git-history integration fixtures use: no signing, no
/// inherited hooks, no dependence on the developer's global git identity.
fn init_hermetic_repo(root: &std::path::Path) {
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "review@example.invalid"]);
    run_git(root, &["config", "user.name", "Review"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join(".git/disabled-hooks")).unwrap();
    run_git(root, &["config", "core.hooksPath", ".git/disabled-hooks"]);
}

/// Commit the working tree at `unix_ts` under `author`, pinning both git dates
/// so the indexer's anchor and the author-event timestamps are controlled.
fn commit_as_at(root: &std::path::Path, subject: &str, author: &str, unix_ts: i64) {
    run_git(root, &["add", "."]);
    let date = format!("{unix_ts} +0000");
    let status = std::process::Command::new("git")
        .args(["commit", "-qm", subject])
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_NAME", author)
        .env("GIT_AUTHOR_EMAIL", format!("{author}@example.invalid"))
        .current_dir(root)
        .status()
        .expect("git commit starts");
    assert!(status.success(), "git commit at {unix_ts} failed");
}

/// A checkout whose newest commit predates the 730-day author window measured
/// from the wall clock. The indexer anchors that window on HEAD and writes
/// every author event, so the read side has to anchor the same way or it
/// reports "no qualifying commits" over data it is holding.
#[test]
fn author_concentration_is_reported_when_head_predates_the_wall_clock_window() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_hermetic_repo(root);

    let head_at = unix_now() - PINNED_AGE_DAYS * DAY;
    for (i, author) in ["ada", "ada", "grace"].iter().enumerate() {
        std::fs::write(
            root.join("lib.rs"),
            format!("pub fn run() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
        commit_as_at(
            root,
            &format!("feat: revision {i}"),
            author,
            head_at - (2 - i as i64) * 30 * DAY,
        );
    }

    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    codesage_graph::git_history_index(&db, root).unwrap();

    // Guard the regime: without it the assertion below could pass on a
    // fixture the wall clock never excluded in the first place.
    let events = db.git_author_events("lib.rs").unwrap();
    assert_eq!(
        events.len(),
        3,
        "three commits touch lib.rs, got {events:?}"
    );
    let wall_clock_cutoff = unix_now() - 730 * DAY;
    assert!(
        events.iter().all(|(_, ts)| *ts < wall_clock_cutoff),
        "fixture must sit outside a wall-clock author window, got {events:?}"
    );

    let r = assess_risk(&db, "lib.rs").unwrap();
    let concentration = r.author_concentration.unwrap_or_else(|| {
        panic!(
            "author concentration must be reported for a HEAD-anchored window, notes {:?}",
            r.notes
        )
    });
    assert_eq!(
        concentration.author_count, 2,
        "two identities touch lib.rs, got {concentration:?}"
    );
    assert_eq!(concentration.bus_factor, 1);
    assert!(
        (concentration.dominant_share - 2.0 / 3.0).abs() < 0.2,
        "ada holds two of three commits, got {concentration:?}"
    );
    assert!(
        !r.notes
            .iter()
            .any(|n| n.contains("author concentration unavailable")),
        "no unavailability note may remain, got {:?}",
        r.notes
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.starts_with("author concentration:")),
        "the measurement note must be present, got {:?}",
        r.notes
    );
}

/// The count is still the cause of an empty coupling page, so the count keeps
/// its wording; the window is added because it decides which commits the count
/// could have come from.
#[test]
fn find_coupling_low_commit_note_adds_the_window_on_a_pinned_checkout() {
    let (_dir, db) = setup_project();
    let pinned = unix_now() - PINNED_AGE_DAYS * DAY;
    db.set_git_index_state_with_anchor("fixture-head", Some(("HEAD", pinned)))
        .unwrap();
    db.upsert_git_file("cold.rs", 0.1, 0, 1, Some(pinned))
        .unwrap();

    let r = codesage_graph::find_coupling(&db, "cold.rs", 5).unwrap();
    let note = r.note.expect("note required");
    assert!(
        note.contains("only 1 tracked commit"),
        "the measured cause must survive: {note}"
    );
    assert!(
        note.contains("730d@HEAD"),
        "the note must name the indexed window: {note}"
    );
    assert!(
        note.contains("touching this file"),
        "the note must name what the newest commit was read from: {note}"
    );
    assert!(
        note.contains("730 days before HEAD"),
        "the note must name the slice that was scanned: {note}"
    );
}

/// A genuinely new file keeps the plain count wording: a spurious window claim
/// would misattribute a thin history on a current index.
#[test]
fn find_coupling_low_commit_note_stays_plain_for_a_recent_commit() {
    let (_dir, db) = setup_project();
    db.upsert_git_file("fresh.rs", 0.1, 0, 1, Some(unix_now() - DAY))
        .unwrap();

    let r = codesage_graph::find_coupling(&db, "fresh.rs", 5).unwrap();
    let note = r.note.expect("note required");
    assert!(
        note.contains("only 1 tracked commit"),
        "the count note must still fire: {note}"
    );
    assert!(
        !note.contains("730d@HEAD"),
        "a current index must not carry a window disclosure: {note}"
    );
}

/// Seed a visible co-change pair whose observations sit at `observed_at`, so
/// the read side has a repo-wide newest-commit reference for paths that carry
/// no `git_files` row of their own.
fn seed_index_newest_commit(db: &Database, observed_at: i64) {
    db.upsert_git_co_change_full(
        "a.rs",
        "b.rs",
        &codesage_storage::db::CoChangeWrite {
            weight: 3.0,
            count: 3,
            window_mask: 0b1,
            first_observed_at: Some(observed_at),
            last_observed_at: Some(observed_at),
        },
    )
    .unwrap();
}

/// An unscored file on a pinned checkout: the note has to offer the window as a
/// cause beside "too new" and "never indexed", and must not become a third
/// line duplicating `UNSCORED_NOTE`.
#[test]
fn unscored_note_names_the_window_on_a_pinned_checkout() {
    let (_dir, db) = setup_project();
    let pinned = unix_now() - PINNED_AGE_DAYS * DAY;
    db.set_git_index_state_with_anchor("fixture-head", Some(("HEAD", pinned)))
        .unwrap();
    seed_index_newest_commit(&db, pinned);

    let r = assess_risk(&db, "Repository.php").unwrap();
    assert!(
        r.unscored,
        "the fixture seeds no git_files row for this path"
    );
    let history_note = r
        .notes
        .iter()
        .find(|n| n.contains("no git history"))
        .unwrap_or_else(|| panic!("expected the no-git-history note, got {:?}", r.notes));
    assert!(
        history_note.contains("730d@HEAD"),
        "the note must name the indexed window: {history_note}"
    );
    assert!(
        history_note.contains("in this index"),
        "with no row of its own the reference is the whole index: {history_note}"
    );
    assert!(
        history_note.contains("last commit older than the indexed window"),
        "the window must be offered as a cause: {history_note}"
    );
    assert_eq!(
        r.notes
            .iter()
            .filter(|n| n.contains("no git history"))
            .count(),
        1,
        "the window must extend the existing note, not add a third: {:?}",
        r.notes
    );
    assert_eq!(
        r.notes
            .iter()
            .filter(|n| n.starts_with("unscored:"))
            .count(),
        1,
        "exactly one unscored note, got {:?}",
        r.notes
    );
}

/// The same path on a current index keeps the original two causes and no
/// window disclosure.
#[test]
fn unscored_note_stays_plain_on_a_current_index() {
    let (_dir, db) = setup_project();
    seed_index_newest_commit(&db, unix_now() - DAY);

    let r = assess_risk(&db, "Repository.php").unwrap();
    let history_note = r
        .notes
        .iter()
        .find(|n| n.contains("no git history"))
        .unwrap_or_else(|| panic!("expected the no-git-history note, got {:?}", r.notes));
    assert!(
        history_note.contains("file too new"),
        "the original causes must survive: {history_note}"
    );
    assert!(
        !history_note.contains("730d@HEAD"),
        "a current index must not carry a window disclosure: {history_note}"
    );
}
