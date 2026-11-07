use codesage_graph::{file_import_pairs, full_index, impact_analysis, list_dependencies};
use codesage_protocol::{ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    (dir, db)
}

fn affected(db: &Database, path: &str) -> Vec<String> {
    impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::File { path: path.into() },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|entry| entry.file_path)
    .collect()
}

#[test]
fn conventional_include_roots_supply_forward_reverse_and_impact_edges() {
    let (_dir, db) = indexed(&[
        ("src/main.c", "#include \"foo.h\"\n"),
        ("include/foo.h", "int foo(void);\n"),
        ("lib/src/bar.c", "#include \"lib/bar.h\"\n"),
        ("lib/include/lib/bar.h", "int bar(void);\n"),
        ("app/main.cpp", "#include \"private.h\"\n"),
        ("src/private.h", "int private_value(void);\n"),
    ]);
    let pairs = file_import_pairs(&db).unwrap();
    for (from, to) in [
        ("src/main.c", "include/foo.h"),
        ("lib/src/bar.c", "lib/include/lib/bar.h"),
        ("app/main.cpp", "src/private.h"),
    ] {
        let deps = list_dependencies(&db, from).unwrap();
        assert_eq!(deps.imports, vec![format!("file:{to}")], "{from}");
        assert!(deps.unresolved_imports.is_empty(), "{deps:?}");
        assert_eq!(list_dependencies(&db, to).unwrap().imported_by, vec![from]);
        assert!(pairs.eager.contains(&(from.into(), to.into())), "{pairs:?}");
        assert_eq!(affected(&db, to), vec![from]);
    }
}

#[test]
fn local_headers_take_precedence_but_competing_fallback_roots_stay_unresolved() {
    let (_dir, db) = indexed(&[
        ("local/main.c", "#include \"config.h\"\n"),
        ("local/config.h", "#define LOCAL 1\n"),
        ("app/main.c", "#include \"config.h\"\n"),
        ("cuda/main.cu", "#include \"config.h\"\n"),
        ("config.h", "#define ROOT 1\n"),
        ("include/config.h", "#define PUBLIC 1\n"),
        ("src/config.h", "#define PRIVATE 1\n"),
        ("app/explicit.c", "#include \"../include/config.h\"\n"),
        (
            "app/unknown.c",
            "#include <config.h>\n#include \"foreign.h\"\n",
        ),
        ("vendor/foreign.h", "#define FOREIGN 1\n"),
    ]);
    assert_eq!(
        list_dependencies(&db, "local/main.c").unwrap().imports,
        vec!["file:local/config.h"]
    );
    let ambiguous = list_dependencies(&db, "app/main.c").unwrap();
    assert!(ambiguous.imports.is_empty(), "{ambiguous:?}");
    assert_eq!(ambiguous.unresolved_imports.len(), 1);
    assert_eq!(
        ambiguous.unresolved_imports[0].candidates,
        vec![
            "file:config.h",
            "file:include/config.h",
            "file:src/config.h"
        ]
    );
    assert_eq!(
        list_dependencies(&db, "app/explicit.c").unwrap().imports,
        vec!["file:include/config.h"]
    );
    let unknown = list_dependencies(&db, "app/unknown.c").unwrap();
    assert!(unknown.imports.is_empty(), "{unknown:?}");
    assert_eq!(unknown.unresolved_imports.len(), 2);
    assert!(
        unknown
            .unresolved_imports
            .iter()
            .all(|row| row.candidates.is_empty())
    );
    assert_eq!(
        file_import_pairs(&db).unwrap().eager,
        vec![
            ("app/explicit.c".into(), "include/config.h".into()),
            ("local/main.c".into(), "local/config.h".into()),
        ]
    );
    for header in ["config.h", "src/config.h", "vendor/foreign.h"] {
        assert!(
            list_dependencies(&db, header)
                .unwrap()
                .imported_by
                .is_empty()
        );
        assert!(affected(&db, header).is_empty(), "{header}");
    }
    assert_eq!(
        list_dependencies(&db, "include/config.h")
            .unwrap()
            .imported_by,
        vec!["app/explicit.c"]
    );
    assert_eq!(affected(&db, "include/config.h"), vec!["app/explicit.c"]);
}

#[test]
fn cpp_symbol_imports_survive_include_ambiguity_filtering() {
    let (_dir, db) = indexed(&[
        ("target.hpp", "namespace ns { struct Foo {}; }\n"),
        ("wrapper.hpp", "#include \"target.hpp\"\n"),
        ("main.cpp", "#include \"wrapper.hpp\"\nusing ns::Foo;\n"),
        (
            "namespace.cpp",
            "#include \"wrapper.hpp\"\nusing namespace ns;\n",
        ),
    ]);
    assert_eq!(
        list_dependencies(&db, "target.hpp").unwrap().imported_by,
        ["main.cpp", "wrapper.hpp"]
    );
}
