use codesage_graph::{full_index, impact_analysis_report, list_dependencies};
use codesage_protocol::{ImpactOptions, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

#[test]
fn import_only_facades_keep_reverse_edges_and_transitive_file_impact() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("impl.py", "def run():\n    return 1\n"),
        ("api.py", "from impl import run\n"),
        (
            "client.py",
            "from api import run\ndef main():\n    return run()\n",
        ),
        ("outer.py", "import client\n"),
        (
            "run.py",
            "# unrelated module with the imported binding's name\n",
        ),
        (
            "other/api.py",
            "# unrelated module with the facade's name\n",
        ),
    ] {
        let file = dir.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(db.symbols_for_file("api.py").unwrap().is_empty());
    assert_eq!(
        list_dependencies(&db, "api.py").unwrap().imported_by,
        ["client.py"]
    );
    for path in ["run.py", "other/api.py"] {
        assert!(
            list_dependencies(&db, path).unwrap().imported_by.is_empty(),
            "{path}"
        );
    }
    let report = impact_analysis_report(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "api.py".into(),
            },
            depth: 2,
            source_only: false,
        },
        &ImpactOptions::default(),
    )
    .unwrap();
    assert!(report.counts_floor);
    assert_eq!(
        report
            .results
            .iter()
            .map(|e| (e.file_path.as_str(), e.distance))
            .collect::<Vec<_>>(),
        [("client.py", 1), ("outer.py", 2)]
    );
}

#[test]
fn relative_python_facades_resolve_within_the_importing_package() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("pkg/api.py", "from .impl import run as exported\n"),
        ("pkg/impl.py", "def run():\n    return 1\n"),
        ("pkg/client.py", "from . import api\n"),
        ("pkg/nested/client.py", "from ..api import exported\n"),
        ("other/api.py", "# unrelated facade\n"),
        ("api.py", "# unrelated root facade\n"),
    ] {
        let file = dir.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(
        list_dependencies(&db, "pkg/api.py").unwrap().imported_by,
        ["pkg/client.py", "pkg/nested/client.py"]
    );
    for path in ["api.py", "other/api.py"] {
        assert!(list_dependencies(&db, path).unwrap().imported_by.is_empty());
    }
    assert_eq!(
        list_dependencies(&db, "pkg/impl.py").unwrap().imported_by,
        ["pkg/api.py"]
    );
}

#[test]
fn javascript_import_only_barrels_have_file_impact() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("api.js", "export { run } from './impl.js';\n"),
        ("impl.js", "export function run() { return 1; }\n"),
        ("client.js", "import { run } from './api.js';\nrun();\n"),
    ] {
        std::fs::write(dir.path().join(path), source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    let report = impact_analysis_report(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "api.js".into(),
            },
            depth: 1,
            source_only: false,
        },
        &ImpactOptions::default(),
    )
    .unwrap();
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].file_path, "client.js");
    assert_eq!(report.results[0].reasons[0].line, 1);
}
