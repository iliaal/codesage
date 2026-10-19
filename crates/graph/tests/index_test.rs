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
    let stats = full_index(dir.path(), &db, &[], false).unwrap();

    assert_eq!(stats.files_indexed, 3);
    assert!(stats.symbols_found > 0);
    assert!(stats.references_found > 0);

    assert_eq!(db.file_count().unwrap(), 3);
    assert!(db.symbol_count().unwrap() > 0);
}

#[test]
fn find_symbol_by_name() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[], false).unwrap();

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "User".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].kind, SymbolKind::Class);
    assert_eq!(results[0].file_path, "app.php");
}

#[test]
fn find_symbol_with_kind_filter() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[], false).unwrap();

    let funcs = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "add".to_string(),
            kind: Some(SymbolKind::Function),
        },
    )
    .unwrap()
    .results;

    assert_eq!(funcs.len(), 1);
    assert_eq!(funcs[0].file_path, "util.c");
}

#[test]
fn incremental_skips_unchanged() {
    let (dir, db) = setup_project();

    let stats1 = full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats1.files_indexed, 3);
    assert_eq!(stats1.files_skipped, 0);

    let stats2 = incremental_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats2.files_indexed, 0);
    assert_eq!(stats2.files_skipped, 3);
}

#[test]
fn incremental_reindexes_changed_file() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[], false).unwrap();

    std::fs::write(
        dir.path().join("main.py"),
        b"def new_function():\n    pass\n",
    )
    .unwrap();

    let stats = incremental_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_indexed, 1);
    assert_eq!(stats.files_skipped, 2);

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "new_function".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    assert_eq!(results.len(), 1);

    let old = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "greet".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    assert!(old.is_empty());
}

#[test]
fn incremental_removes_deleted_files() {
    let (dir, db) = setup_project();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(db.file_count().unwrap(), 3);

    std::fs::remove_file(dir.path().join("util.c")).unwrap();

    let stats = incremental_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_removed, 1);
    assert_eq!(db.file_count().unwrap(), 2);

    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "add".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    assert!(results.is_empty());
}

#[test]
fn full_index_keeps_a_file_whose_parse_recovered_from_unknown_macros() {
    let (dir, db) = setup_project();
    // php-src style: function-like macros at file scope that the C grammar
    // cannot parse without the preprocessor. Tree-sitter recovers around them.
    std::fs::write(
        dir.path().join("ext.c"),
        b"ZEND_BEGIN_ARG_INFO_EX(arginfo_mul, 0, 0, 2)\nZEND_END_ARG_INFO()\n\nint mul(int a, int b) {\n    return a * b;\n}\n",
    )
    .unwrap();

    let stats = full_index(dir.path(), &db, &[], false).unwrap();

    assert_eq!(stats.files_failed, 0, "{stats:?}");
    assert_eq!(stats.files_indexed, 4, "{stats:?}");
    assert_eq!(stats.files_degraded, 1, "{stats:?}");
    assert_eq!(stats.files_degraded_by_language.get("c"), Some(&1));
    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: "mul".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].file_path, "ext.c");
}

#[test]
fn header_source_additions_include_edits_and_removals_match_fresh_index() {
    use codesage_protocol::Language;
    let root = tempfile::tempdir().unwrap();
    for directory in ["c", "cpp", "include"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    std::fs::write(
        root.path().join("c/core.h"),
        "struct Core { int value; };\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("cpp/local.h"),
        "class Local { public: void run() { helper(); } };\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("include/api.h"),
        "struct Remote { int value; };\nvoid remote_call(void) { helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("c/degraded.h"),
        concat!(
            "void stable(void) { helper(); }\n",
            "struct Item {\n",
            "    union {\n",
            "        struct {\n",
            "            PACK_FIELDS(int flags, int unused, int count, int unused2)\n",
            "        } values;\n",
            "        int flags;\n",
            "    } fields;\n",
            "    int count;\n",
            "};\n",
            "#if WORD_SIZE == 4\n",
            "static inline int convert(int input) {\n",
            "    return input * sizeof(int);\n",
            "}\n",
            "#elif WORD_SIZE == 8\n",
            "static inline int convert(int input) {\n",
            "    return input;\n",
            "}\n",
            "#endif\n",
        ),
    )
    .unwrap();
    let incremental = Database::open_in_memory().unwrap();
    full_index(root.path(), &incremental, &[], false).unwrap();
    let pure_c_symbols = incremental.symbols_for_file("c/degraded.h").unwrap();
    assert!(
        pure_c_symbols
            .iter()
            .any(|symbol| symbol.name == "stable" && symbol.line_start == 1)
    );
    for source in [
        Some("#include <api.h>\nvoid worker() {}\n"),
        Some("void worker() {}\n"),
        None,
    ] {
        let path = root.path().join("cpp/worker.cpp");
        if let Some(source) = source {
            std::fs::write(path, source).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
        incremental_index(root.path(), &incremental, &[], false).unwrap();
        let fresh = Database::open_in_memory().unwrap();
        full_index(root.path(), &fresh, &[], false).unwrap();
        let languages = |db: &Database| {
            db.all_files_with_id_and_language()
                .unwrap()
                .into_iter()
                .map(|(_, path, language)| (path, language))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let actual = languages(&incremental);
        assert_eq!(actual, languages(&fresh));
        assert_eq!(actual["c/core.h"], Language::C);
        assert_eq!(
            actual["c/degraded.h"],
            if source.is_some() {
                Language::Cpp
            } else {
                Language::C
            }
        );
        let recovered = incremental.symbols_for_file("c/degraded.h").unwrap();
        if source.is_some() {
            assert!(recovered.iter().any(|symbol| symbol.name == "Item"
                && symbol.line_start == 2
                && symbol.line_end == 10));
            assert_eq!(
                recovered
                    .iter()
                    .filter(|symbol| symbol.name == "convert")
                    .map(|symbol| (symbol.line_start, symbol.line_end))
                    .collect::<Vec<_>>(),
                [(12, 14), (16, 18)]
            );
        } else {
            assert_eq!(recovered, pure_c_symbols);
        }
        assert_eq!(
            actual["cpp/local.h"],
            if source.is_some() {
                Language::Cpp
            } else {
                Language::C
            }
        );
        assert_eq!(
            actual["include/api.h"],
            if source.is_some_and(|s| s.contains("#include")) {
                Language::Cpp
            } else {
                Language::C
            }
        );
        for path in actual.keys() {
            assert_eq!(
                incremental.symbols_for_file(path).unwrap(),
                fresh.symbols_for_file(path).unwrap(),
                "{path}"
            );
            assert_eq!(
                incremental
                    .references_in_file_range(path, 1, u32::MAX)
                    .unwrap(),
                fresh.references_in_file_range(path, 1, u32::MAX).unwrap(),
                "{path}"
            );
        }
    }
}

#[test]
fn degraded_counts_are_by_actual_parser_language_and_only_this_pass() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("broken.c"), "int c_broken( {\n").unwrap();
    std::fs::write(root.path().join("broken.cpp"), "class Cpp { invalid(\n").unwrap();
    std::fs::write(root.path().join("clean.py"), "def clean():\n    pass\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    let stats = full_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_degraded, 2);
    assert_eq!(
        stats.files_degraded_by_language,
        [("c".into(), 1), ("cpp".into(), 1)].into_iter().collect()
    );
    let unchanged = incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(unchanged.files_degraded, 0);
    assert!(unchanged.files_degraded_by_language.is_empty());
}

#[test]
fn partial_cpp_writes_cannot_make_old_header_decisions_look_current() {
    use codesage_protocol::{FileInfo, Language};
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("cpp")).unwrap();
    std::fs::create_dir(root.path().join("include")).unwrap();
    std::fs::write(root.path().join("cpp/local.h"), "class Local {};\n").unwrap();
    std::fs::write(
        root.path().join("include/api.h"),
        "struct Api { int value; };\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    for source in [
        Some("#include <api.h>\nvoid marker() {}\n"),
        Some("void marker() {}\n"),
        None,
    ] {
        if let Some(source) = source {
            std::fs::write(root.path().join("cpp/main.cpp"), source).unwrap();
            codesage_graph::index_files(
                root.path(),
                &db,
                &[FileInfo {
                    path: "cpp/main.cpp".into(),
                    language: Language::Cpp,
                    content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
                    is_test: false,
                }],
                false,
            )
            .unwrap();
        } else {
            std::fs::remove_file(root.path().join("cpp/main.cpp")).unwrap();
            codesage_graph::remove_files(&db, &["cpp/main.cpp".into()]).unwrap();
        }
        codesage_graph::incremental_index(root.path(), &db, &[], false).unwrap();
        let fresh = Database::open_in_memory().unwrap();
        codesage_graph::full_index(root.path(), &fresh, &[], false).unwrap();
        let languages = |db: &Database| {
            db.all_files_with_id_and_language()
                .unwrap()
                .into_iter()
                .map(|(_, path, language)| (path, language))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(
            languages(&db),
            languages(&fresh),
            "after partial write {source:?}"
        );
        for path in ["cpp/local.h", "include/api.h"] {
            assert_eq!(
                db.symbols_for_file(path).unwrap(),
                fresh.symbols_for_file(path).unwrap(),
                "{path}"
            );
            assert_eq!(
                db.references_in_file_range(path, 1, u32::MAX).unwrap(),
                fresh.references_in_file_range(path, 1, u32::MAX).unwrap(),
                "{path}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn unreadable_cpp_context_retains_headers_and_recovers_with_identical_hash() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("api.h"), "class Api {};\n").unwrap();
    let cpp = root.path().join("main.cpp");
    std::fs::write(&cpp, "void marker() {}\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    let symbols = db.symbols_for_file("api.h").unwrap();
    assert!(symbols.iter().any(|symbol| symbol.name == "Api"));
    let permissions = std::fs::metadata(&cpp).unwrap().permissions();
    std::fs::set_permissions(&cpp, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = std::fs::read(&cpp).is_err();
    let result = codesage_graph::incremental_index(root.path(), &db, &[], false);
    std::fs::set_permissions(&cpp, permissions).unwrap();
    if !unreadable {
        eprintln!("skipping: permission bits cannot make the C++ source unreadable");
        return;
    }
    let failed = result.unwrap();
    assert!(failed.failed_paths.iter().any(|path| path == "main.cpp"));
    assert_eq!(db.symbols_for_file("api.h").unwrap(), symbols);
    assert!(
        !db.file_interpretation_matches("api.h", codesage_graph::STRUCTURAL_INTERPRETATION)
            .unwrap()
    );
    codesage_graph::incremental_index(root.path(), &db, &[], false).unwrap();
    let fresh = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &fresh, &[], false).unwrap();
    assert_eq!(
        db.symbols_for_file("api.h").unwrap(),
        fresh.symbols_for_file("api.h").unwrap()
    );
    assert!(
        db.file_interpretation_matches("api.h", codesage_graph::STRUCTURAL_INTERPRETATION)
            .unwrap()
    );
}

#[test]
fn editing_an_existing_header_does_not_reparse_unchanged_peers() {
    use codesage_protocol::{FileInfo, Language};
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("c")).unwrap();
    std::fs::create_dir(root.path().join("cpp")).unwrap();
    for name in ["first", "second", "third"] {
        std::fs::write(
            root.path().join(format!("c/{name}.h")),
            format!("int {name}(void) {{ return 1; }}\n"),
        )
        .unwrap();
    }
    std::fs::write(root.path().join("cpp/main.cpp"), "void marker() {}\n").unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    std::fs::write(
        root.path().join("c/first.h"),
        "int first(void) { return 2; }\n",
    )
    .unwrap();
    let changed = codesage_graph::incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!((changed.files_indexed, changed.files_skipped), (1, 3));
    let source = "int second(void) { return 3; }\n";
    std::fs::write(root.path().join("c/second.h"), source).unwrap();
    codesage_graph::index_files(
        root.path(),
        &db,
        &[FileInfo {
            path: "c/second.h".into(),
            language: Language::C,
            content_hash: codesage_parser::discover::content_hash(source.as_bytes()),
            is_test: false,
        }],
        false,
    )
    .unwrap();
    let unchanged = codesage_graph::incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!((unchanged.files_indexed, unchanged.files_skipped), (0, 4));
    let fresh = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &fresh, &[], false).unwrap();
    for name in ["first", "second", "third"] {
        let path = format!("c/{name}.h");
        assert_eq!(
            db.symbols_for_file(&path).unwrap(),
            fresh.symbols_for_file(&path).unwrap()
        );
    }
}
