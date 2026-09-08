use codesage_graph::{full_index, list_dependencies, trace_call_path};
use codesage_protocol::CallPathRequest;
use codesage_storage::Database;

#[test]
fn glob_imports_disambiguate_calls_and_keep_import_only_edges() {
    for (caller, target, facade, unrelated, source, definition, other) in [
        (
            "src/api/client.rs",
            "src/api.rs",
            "src/facade.rs",
            "src/other.rs",
            "use super::*; pub fn entry() { run(); }",
            "pub fn run() { correct(); } pub fn correct() {}",
            "pub fn run() { wrong(); } pub fn wrong() {}",
        ),
        (
            "pkg/client.py",
            "pkg/api.py",
            "facade.py",
            "other.py",
            "from .api import *\ndef entry():\n    run()\n",
            "def run():\n    correct()\ndef correct():\n    pass\n",
            "def run():\n    wrong()\ndef wrong():\n    pass\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let facade_source = if caller.ends_with(".rs") {
            "use crate::api::*;"
        } else {
            "from pkg.api import *\n"
        };
        for (path, contents) in [
            (caller, source),
            (target, definition),
            (unrelated, other),
            (facade, facade_source),
        ] {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, contents).unwrap();
        }
        let db = Database::open_in_memory().unwrap();
        full_index(dir.path(), &db, &[], false).unwrap();
        let dependencies = list_dependencies(&db, target).unwrap();
        assert!(dependencies.imported_by.contains(&caller.to_string()));
        assert!(dependencies.imported_by.contains(&facade.to_string()));
        assert!(
            list_dependencies(&db, unrelated)
                .unwrap()
                .imported_by
                .is_empty()
        );
        for (to, expected) in [("correct", true), ("wrong", false)] {
            let report = trace_call_path(
                &db,
                &CallPathRequest {
                    from: "entry".into(),
                    to: to.into(),
                    max_depth: 3,
                },
            )
            .unwrap();
            assert_eq!(report.found, expected, "{caller} -> {to}: {report:?}");
        }
        let references = db.references_in_file_range(caller, 1, u32::MAX).unwrap();
        assert!(references.iter().any(|r| r.to_name.ends_with('*')));
    }
}
