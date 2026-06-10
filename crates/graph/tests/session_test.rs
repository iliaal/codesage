//! session_start / session_end integration tests.

use codesage_graph::{full_index, session_end, session_start};
use codesage_storage::Database;

/// Lays out a tiny .codesage/ directory under the temp dir so session_start
/// has somewhere to write the snapshot file.
fn setup_project_with_codesage_dir() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".codesage")).unwrap();
    let db = Database::open_in_memory().unwrap();
    (dir, db)
}

fn write_acyclic_php(root: &std::path::Path) {
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Mid;\nclass Top { public function x(Mid $m) { return $m->y(null); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Leaf;\nclass Mid { public function y(Leaf $l) { return $l->z(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Leaf { public function z() { return 1; } }\n",
    )
    .unwrap();
}

fn write_cyclic_php(root: &std::path::Path) {
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
}

#[test]
fn session_end_passes_when_no_changes() {
    let (dir, db) = setup_project_with_codesage_dir();
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let snap = session_start(dir.path(), &db, "default").unwrap();
    assert_eq!(snap.file_count, 3);
    assert!(
        snap.cycles.is_empty(),
        "acyclic baseline should have no cycles"
    );

    let diff = session_end(dir.path(), &db, "default").unwrap();
    assert!(diff.pass, "no changes between start and end → must pass");
    assert!(diff.new_cycles.is_empty());
    assert!(diff.resolved_cycles.is_empty());
    assert!(diff.new_files.is_empty());
    assert!(diff.removed_files.is_empty());
    assert!(diff.risk_regressions.is_empty());
    assert_eq!(diff.max_risk_regression, 0.0);
    assert!(
        diff.summary_notes
            .iter()
            .any(|n| n.contains("no structural regressions")),
        "expected clean-pass note, got {:?}",
        diff.summary_notes
    );
}

#[test]
fn session_end_fails_when_new_cycle_introduced() {
    let (dir, db) = setup_project_with_codesage_dir();
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let _snap = session_start(dir.path(), &db, "default").unwrap();

    // Replace the linear A/B/C with a cyclic A<->B in the same DB.
    // Removing prior files isn't easy through full_index; the simplest
    // path is to wipe the DB and reindex with the cyclic layout.
    let db = Database::open_in_memory().unwrap();
    write_cyclic_php(dir.path());
    // Remove C so it doesn't linger from the prior layout.
    let _ = std::fs::remove_file(dir.path().join("C.php"));
    full_index(dir.path(), &db, &[], false).unwrap();

    // Snapshot was written to disk during session_start; the new in-memory DB
    // doesn't matter for the snapshot read. session_end re-derives current
    // state from `db` which now has the cyclic graph.
    let diff = session_end(dir.path(), &db, "default").unwrap();
    assert!(!diff.pass, "new A<->B cycle should fail the gate");
    assert_eq!(
        diff.new_cycles.len(),
        1,
        "expected exactly one new cycle, got {:?}",
        diff.new_cycles
    );
    let cycle = &diff.new_cycles[0];
    assert!(cycle.contains(&"A.php".to_string()));
    assert!(cycle.contains(&"B.php".to_string()));
    assert!(
        diff.summary_notes
            .iter()
            .any(|n| n.contains("new import cycle")),
        "expected new-cycle note, got {:?}",
        diff.summary_notes
    );
}

#[test]
fn session_end_reports_resolved_cycle() {
    let (dir, db) = setup_project_with_codesage_dir();
    write_cyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let snap = session_start(dir.path(), &db, "default").unwrap();
    assert_eq!(snap.cycles.len(), 1, "baseline should have the A<->B cycle");

    // Replace cyclic layout with acyclic.
    let db = Database::open_in_memory().unwrap();
    let _ = std::fs::remove_file(dir.path().join("A.php"));
    let _ = std::fs::remove_file(dir.path().join("B.php"));
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let diff = session_end(dir.path(), &db, "default").unwrap();
    assert!(
        diff.pass,
        "no NEW cycles introduced (only resolved), should pass"
    );
    assert_eq!(diff.resolved_cycles.len(), 1);
    assert!(diff.new_cycles.is_empty());
    assert!(
        diff.summary_notes
            .iter()
            .any(|n| n.contains("cycle(s) resolved")),
        "expected resolved-cycle note, got {:?}",
        diff.summary_notes
    );
}

#[test]
fn session_end_errors_when_snapshot_missing() {
    let (dir, db) = setup_project_with_codesage_dir();
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let err = session_end(dir.path(), &db, "never-started").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("session_start") || msg.contains("never-started"),
        "expected helpful error, got: {msg}"
    );
}

#[test]
fn session_start_overwrites_existing_snapshot() {
    // Re-running session_start with the same id should reset the baseline.
    // Verified by confirming the second snapshot's file_count reflects the
    // current state, not the prior one.
    let (dir, db) = setup_project_with_codesage_dir();
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    let snap1 = session_start(dir.path(), &db, "default").unwrap();
    assert_eq!(snap1.file_count, 3);

    // Add a fourth file and re-snapshot.
    std::fs::write(
        dir.path().join("D.php"),
        b"<?php\nnamespace App;\nclass Standalone {}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();

    let snap2 = session_start(dir.path(), &db, "default").unwrap();
    assert_eq!(snap2.file_count, 4, "second snapshot should see 4 files");

    // session_end against the (overwritten) snapshot should now compare
    // against the 4-file baseline; with no further changes it must pass.
    let diff = session_end(dir.path(), &db, "default").unwrap();
    assert!(diff.pass);
    assert!(diff.new_files.is_empty());
}

#[test]
fn session_start_rejects_invalid_session_id() {
    let (dir, db) = setup_project_with_codesage_dir();
    write_acyclic_php(dir.path());
    full_index(dir.path(), &db, &[], false).unwrap();

    assert!(session_start(dir.path(), &db, "../etc/passwd").is_err());
    assert!(session_start(dir.path(), &db, "a/b").is_err());
    assert!(session_start(dir.path(), &db, "").is_err());
}
