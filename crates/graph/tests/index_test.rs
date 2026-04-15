use codesage_graph::{find_symbol, full_index, incremental_index};
use codesage_protocol::{FindSymbolRequest, SymbolKind};
use codesage_storage::Database;

fn setup_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("app.php"),
        b"<?php\nnamespace App;\nclass User {\n  public function name(): string { return ''; }\n}\nfunction helper() {}\n",
    ).unwrap();

    std::fs::write(
        root.join("main.py"),
        b"import os\n\ndef greet(name: str) -> str:\n    return f'hello {name}'\n\nclass Config:\n    def load(self):\n        pass\n",
    ).unwrap();

    std::fs::write(
        root.join("util.c"),
        b"#include <stdio.h>\n\n#define BUF_SIZE 256\n\nint add(int a, int b) {\n    return a + b;\n}\n",
    ).unwrap();

    let db = Database::open_in_memory().unwrap();
    (dir, db)
}

#[test]
fn full_index_discovers_and_indexes() {
    let (dir, db) = setup_project();
    let stats = full_index(dir.path(), &db, &[]).unwrap();

    assert_eq!(stats.files_indexed, 3);
    assert!(stats.symbols_found > 0);
    assert!(stats.references_found > 0);

    assert_eq!(db.file_count().unwrap(), 3);
    assert!(db.symbol_count().unwrap() > 0);
}

#[test]
fn find_symbol_by_name() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[]).unwrap();

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "User".to_string(),
            kind: None,
        },
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].kind, SymbolKind::Class);
    assert_eq!(results[0].file_path, "app.php");
}

#[test]
fn find_symbol_with_kind_filter() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[]).unwrap();

    let funcs = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "add".to_string(),
            kind: Some(SymbolKind::Function),
        },
    )
    .unwrap();

    assert_eq!(funcs.len(), 1);
    assert_eq!(funcs[0].file_path, "util.c");
}

#[test]
fn incremental_skips_unchanged() {
    let (dir, db) = setup_project();

    let stats1 = full_index(dir.path(), &db, &[]).unwrap();
    assert_eq!(stats1.files_indexed, 3);
    assert_eq!(stats1.files_skipped, 0);

    let stats2 = incremental_index(dir.path(), &db, &[]).unwrap();
    assert_eq!(stats2.files_indexed, 0);
    assert_eq!(stats2.files_skipped, 3);
}

#[test]
fn incremental_reindexes_changed_file() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[]).unwrap();

    std::fs::write(
        dir.path().join("main.py"),
        b"def new_function():\n    pass\n",
    )
    .unwrap();

    let stats = incremental_index(dir.path(), &db, &[]).unwrap();
    assert_eq!(stats.files_indexed, 1);
    assert_eq!(stats.files_skipped, 2);

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "new_function".to_string(),
            kind: None,
        },
    )
    .unwrap();
    assert_eq!(results.len(), 1);

    let old = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "greet".to_string(),
            kind: None,
        },
    )
    .unwrap();
    assert!(old.is_empty());
}

#[test]
fn incremental_removes_deleted_files() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[]).unwrap();
    assert_eq!(db.file_count().unwrap(), 3);

    std::fs::remove_file(dir.path().join("util.c")).unwrap();

    let stats = incremental_index(dir.path(), &db, &[]).unwrap();
    assert_eq!(stats.files_removed, 1);
    assert_eq!(db.file_count().unwrap(), 2);

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "add".to_string(),
            kind: None,
        },
    )
    .unwrap();
    assert!(results.is_empty());
}
