use codesage_graph::{full_index, incremental_index};
use codesage_protocol::Language;
use codesage_storage::Database;
use std::path::Path;

const HEADER: &str = "namespace example {\nclass Widget {\npublic:\nvoid run();\n};\n}\n";

fn language(db: &Database, path: &str) -> Language {
    db.all_files_with_id_and_language()
        .unwrap()
        .into_iter()
        .find(|(_, file, _)| file == path)
        .unwrap()
        .2
}

fn assert_matches_full(root: &Path, db: &Database, expected: Language) {
    let rebuilt = Database::open_in_memory().unwrap();
    full_index(root, &rebuilt, &[], false).unwrap();
    assert_eq!(language(db, "api.h"), expected);
    assert_eq!(language(db, "api.h"), language(&rebuilt, "api.h"));
    assert_eq!(
        db.symbols_for_file("api.h").unwrap(),
        rebuilt.symbols_for_file("api.h").unwrap()
    );
    assert_eq!(std::fs::read_to_string(root.join("api.h")).unwrap(), HEADER);
}

#[test]
fn adding_first_cpp_source_reinterprets_unchanged_header() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("api.h"), HEADER).unwrap();
    std::fs::write(
        root.path().join("util.c"),
        "int utility(void) { return 0; }",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    assert_matches_full(root.path(), &db, Language::C);

    std::fs::write(root.path().join("main.cpp"), "int main() { return 0; }").unwrap();
    let changed = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(changed.files_indexed, 2);
    assert_eq!(changed.files_skipped, 1);
    assert_matches_full(root.path(), &db, Language::Cpp);
    let symbols = db.symbols_for_file("api.h").unwrap();
    assert!(symbols.iter().any(|s| s.name == "Widget"));
    assert!(symbols.iter().any(|s| s.name == "run"));
    assert_eq!(language(&db, "util.c"), Language::C);

    let unchanged = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(unchanged.files_indexed, 0);
    assert_eq!(unchanged.files_skipped, 3);
}

#[test]
fn removing_last_cpp_source_reinterprets_unchanged_header() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("api.h"), HEADER).unwrap();
    for path in ["main.cpp", "other.cc"] {
        std::fs::write(root.path().join(path), "int entry() { return 0; }").unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    assert_matches_full(root.path(), &db, Language::Cpp);

    std::fs::remove_file(root.path().join("main.cpp")).unwrap();
    let still_cpp = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(still_cpp.files_removed, 1);
    assert_eq!(still_cpp.files_indexed, 0);
    assert_eq!(still_cpp.files_skipped, 2);
    assert_matches_full(root.path(), &db, Language::Cpp);

    std::fs::remove_file(root.path().join("other.cc")).unwrap();
    let changed = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(changed.files_removed, 1);
    assert_eq!(changed.files_indexed, 1);
    assert_eq!(changed.files_skipped, 0);
    assert_matches_full(root.path(), &db, Language::C);
    assert!(
        !db.symbols_for_file("api.h")
            .unwrap()
            .iter()
            .any(|s| s.name == "Widget" || s.name == "run")
    );

    let unchanged = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(unchanged.files_indexed, 0);
    assert_eq!(unchanged.files_skipped, 1);
}
