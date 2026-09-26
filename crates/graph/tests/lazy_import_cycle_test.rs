//! A function-body import is a use, not a load-time dependency. The lazy
//! fixture pair is cyclic only through `a.run`'s `from b import other`; the
//! eager fixture hoists that import to module scope and must still cycle.

use codesage_graph::{
    assess_risk, assess_risk_diff, build_review_rehearsal, file_import_pairs, full_index,
    impact_analysis, list_dependencies, session_end, session_start,
};
use codesage_protocol::{ImpactRequest, ImpactTarget, ReferenceKind};
use codesage_storage::Database;
use std::path::Path;

fn fixture_dir(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lazy_imports")
        .join(name)
}

fn index_fixture(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".codesage")).unwrap();
    for entry in std::fs::read_dir(fixture_dir(name)).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    (dir, db)
}

#[test]
fn python_function_body_import_does_not_close_a_cycle() {
    let (_dir, db) = index_fixture("python_lazy");

    assert_eq!(
        db.enumerate_file_import_edges().unwrap(),
        vec![("b.py".to_string(), "a.py".to_string())]
    );
    assert_eq!(
        db.lazy_import_pairs().unwrap(),
        vec![("a.py".to_string(), "b.py".to_string())]
    );

    for file in ["a.py", "b.py"] {
        let risk = assess_risk(&db, file).unwrap();
        assert!(!risk.in_cycle, "{file}: {risk:?}");
        assert_eq!(risk.cycle_size, 0);
        assert!(risk.cycle_files.is_empty());
        assert_eq!(risk.lazy_edges, 1, "{file}: {risk:?}");
        assert!(
            !risk.notes.iter().any(|n| n.contains("import cycle")),
            "{file}: {:?}",
            risk.notes
        );
        let json = serde_json::to_value(&risk).unwrap();
        assert_eq!(json["lazy_edges"], 1);
    }

    let diff = assess_risk_diff(&db, &["a.py".to_string(), "b.py".to_string()]).unwrap();
    assert!(diff.cycles_touching_patch.is_empty(), "{diff:?}");
    assert!(diff.files.iter().all(|f| f.lazy_edges == 1), "{diff:?}");
}

#[test]
fn python_module_scope_import_still_closes_the_cycle() {
    let (_dir, db) = index_fixture("python_eager");

    let mut edges = db.enumerate_file_import_edges().unwrap();
    edges.sort();
    assert_eq!(
        edges,
        vec![
            ("a.py".to_string(), "b.py".to_string()),
            ("b.py".to_string(), "a.py".to_string()),
        ]
    );
    assert!(db.lazy_import_pairs().unwrap().is_empty());

    let risk = assess_risk(&db, "a.py").unwrap();
    assert!(risk.in_cycle, "{risk:?}");
    assert_eq!(risk.cycle_size, 2);
    assert_eq!(risk.cycle_files, vec!["b.py".to_string()]);
    assert_eq!(risk.lazy_edges, 0);
    let json = serde_json::to_value(&risk).unwrap();
    assert!(json.get("lazy_edges").is_none(), "{json}");
}

#[test]
fn lazy_edge_remains_visible_to_dependencies_and_impact() {
    let (_dir, db) = index_fixture("python_lazy");

    let deps = list_dependencies(&db, "a.py").unwrap();
    assert!(deps.found);
    assert!(
        deps.imports.iter().any(|i| i == "b.other" || i == "b"),
        "a.py imports should keep the lazy edge: {:?}",
        deps.imports
    );
    let deps_b = list_dependencies(&db, "b.py").unwrap();
    assert!(
        deps_b.imported_by.iter().any(|p| p == "a.py"),
        "b.py imported_by should keep the lazy importer: {:?}",
        deps_b.imported_by
    );

    let entries = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "other".to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert!(
        entries.iter().any(|e| e.file_path == "a.py"),
        "impact of `other` must reach the lazy importer: {entries:?}"
    );

    let lazy_rows: Vec<_> = db
        .find_references("other", None)
        .unwrap()
        .into_iter()
        .filter(|r| r.from_file == "a.py" && r.kind != ReferenceKind::Call)
        .collect();
    assert_eq!(lazy_rows.len(), 2, "{lazy_rows:?}");
    assert!(lazy_rows.iter().all(|r| r.lazy), "{lazy_rows:?}");
    let json = serde_json::to_value(&lazy_rows[0]).unwrap();
    assert_eq!(json["lazy"], true);
    let eager = db
        .find_references("run", None)
        .unwrap()
        .into_iter()
        .find(|r| r.from_file == "b.py")
        .unwrap();
    assert!(!eager.lazy);
    let json = serde_json::to_value(&eager).unwrap();
    assert!(json.get("lazy").is_none(), "{json}");
}

#[test]
fn rehearsal_and_session_stop_objecting_to_a_lazy_cycle() {
    let (dir, db) = index_fixture("python_lazy");
    let files = vec!["a.py".to_string(), "b.py".to_string()];

    let report = build_review_rehearsal(dir.path(), &db, &files).unwrap();
    assert!(
        !report
            .objections
            .iter()
            .any(|o| o.category == "import-cycle"),
        "{:?}",
        report.objections
    );

    let snapshot = session_start(dir.path(), &db, "lazy").unwrap();
    assert!(snapshot.cycles.is_empty(), "{snapshot:?}");
    assert_eq!(snapshot.lazy_edges, 1);
    let diff = session_end(dir.path(), &db, "lazy").unwrap();
    assert!(diff.pass, "{diff:?}");
    assert!(diff.new_cycles.is_empty());
    assert_eq!(diff.lazy_edges, 1);
    let json = serde_json::to_value(&diff).unwrap();
    assert_eq!(json["lazy_edges"], 1);

    let (eager_dir, eager_db) = index_fixture("python_eager");
    let report = build_review_rehearsal(eager_dir.path(), &eager_db, &files).unwrap();
    let cycle = report
        .objections
        .iter()
        .find(|o| o.category == "import-cycle")
        .expect("module-scope import pair still objects");
    assert!(
        !cycle.evidence.iter().any(|e| e.contains("lazy_edges")),
        "{cycle:?}"
    );
    let snapshot = session_start(eager_dir.path(), &eager_db, "eager").unwrap();
    assert_eq!(snapshot.cycles.len(), 1);
    assert_eq!(snapshot.lazy_edges, 0);
    let json = serde_json::to_value(&snapshot).unwrap();
    assert!(json.get("lazy_edges").is_none(), "{json}");
}

#[test]
fn javascript_require_in_a_function_body_is_a_lazy_path_edge_that_does_not_close_a_cycle() {
    let (_dir, db) = index_fixture("js_lazy");

    let lazy = db
        .find_references("./b", None)
        .unwrap()
        .into_iter()
        .find(|r| r.from_file == "a.js")
        .expect("require('./b') inside run() is indexed");
    assert!(lazy.lazy, "{lazy:?}");
    let eager = db
        .find_references("./a", None)
        .unwrap()
        .into_iter()
        .find(|r| r.from_file == "b.js")
        .expect("module-scope require('./a') is indexed");
    assert!(!eager.lazy, "{eager:?}");

    // Module-path strings name no symbol, so the symbol-joined storage half
    // stays empty; the cycle graph resolves them to files the way
    // `list_dependencies` does, and the lazy mark then decides the pair.
    assert!(db.enumerate_file_import_edges().unwrap().is_empty());
    assert!(db.lazy_import_pairs().unwrap().is_empty());
    let pairs = file_import_pairs(&db).unwrap();
    assert_eq!(pairs.eager, vec![("b.js".to_string(), "a.js".to_string())]);
    assert_eq!(
        pairs.lazy_only,
        vec![("a.js".to_string(), "b.js".to_string())]
    );

    for file in ["a.js", "b.js"] {
        let risk = assess_risk(&db, file).unwrap();
        assert!(!risk.in_cycle, "{file}: {risk:?}");
        assert_eq!(risk.cycle_size, 0);
        assert_eq!(risk.lazy_edges, 1, "{file}: {risk:?}");
    }
    let diff = assess_risk_diff(&db, &["a.js".to_string(), "b.js".to_string()]).unwrap();
    assert!(diff.cycles_touching_patch.is_empty(), "{diff:?}");
}
