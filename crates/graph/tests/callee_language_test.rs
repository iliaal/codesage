use codesage_graph::{
    ReachabilityOptions, export_context_for_symbol, find_references, full_index, impact_analysis,
    incremental_index, recommend_tests_with_reachability, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, DEFAULT_EMBEDDING_DIM, ExportRequest, FindReferencesRequest, ImpactRequest,
    ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let db = Database::open_for_model(
        &root.path().join("index.db"),
        "fixture",
        DEFAULT_EMBEDDING_DIM,
    )
    .unwrap();
    let stats = full_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_indexed, files.len(), "{stats:?}");
    for (path, source) in files {
        db.insert_chunks(
            path,
            "c",
            &[(
                source,
                1,
                source.lines().count() as u32,
                &vec![0.0; DEFAULT_EMBEDDING_DIM],
            )],
        )
        .unwrap();
    }
    (root, db)
}

fn trace(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 4,
        },
    )
    .unwrap()
    .found
}

fn impact(db: &Database, target: &str, caller: &str) -> bool {
    impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .iter()
    .any(|row| row.file_path == caller && row.reasons.iter().any(|r| r.kind == ReferenceKind::Call))
}

#[test]
fn c_standard_library_call_does_not_reach_go_namesake() {
    let (root, db) = project(&[
        (
            "caller.c",
            "#include <stdio.h>\nint own_local(void);\nint greet(void) { return puts(\"hello\") + own_local(); }\n",
        ),
        ("own.c", "int own_local(void) { return 1; }\n"),
        ("helper.go", "package helpers\nfunc puts() {}\n"),
        (
            "tests/caller_test.c",
            "int greet(void);\nvoid test_greet(void) { greet(); }\n",
        ),
    ]);
    let context = export_context_for_symbol(
        &db,
        "greet",
        &ExportRequest {
            query: None,
            symbol: Some("greet".into()),
            include_callers: false,
            include_callees: true,
            limit: 20,
        },
    )
    .unwrap();
    assert!(
        context.related.iter().any(|row| row.file_path == "own.c"),
        "{context:?}"
    );
    assert!(
        !context
            .related
            .iter()
            .any(|row| row.file_path == "helper.go"),
        "{context:?}"
    );
    let check = |db: &Database| {
        let rows = find_references(
            db,
            &FindReferencesRequest {
                symbol_name: "puts".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(rows.results.len(), 1, "{rows:?}");
        assert!(rows.results[0].to.is_none(), "{rows:?}");
        assert!(!trace(db, "greet", "sym:helper.go#puts"));
        assert!(!impact(db, "sym:helper.go#puts", "caller.c"));
        assert!(trace(db, "greet", "own_local"));
        assert!(impact(db, "own_local", "caller.c"));
        let unrelated = recommend_tests_with_reachability(
            db,
            &["helper.go".into()],
            &ReachabilityOptions::default(),
        )
        .unwrap();
        assert!(unrelated.reachable.is_empty(), "{unrelated:?}");
        let genuine = recommend_tests_with_reachability(
            db,
            &["own.c".into()],
            &ReachabilityOptions::default(),
        )
        .unwrap();
        assert!(
            genuine
                .reachable
                .iter()
                .any(|r| r.path == "tests/caller_test.c"),
            "{genuine:?}"
        );
    };
    check(&db);
    check(&db);
    drop(db);
    let db = Database::open(&root.path().join("index.db")).unwrap();
    check(&db);
    std::fs::write(
        root.path().join("helper.go"),
        "package helpers\nfunc different() {}\n",
    )
    .unwrap();
    incremental_index(root.path(), &db, &[], false).unwrap();
    std::fs::write(
        root.path().join("helper.go"),
        "package helpers\nfunc puts() {}\n",
    )
    .unwrap();
    incremental_index(root.path(), &db, &[], false).unwrap();
    check(&db);
    let rebuilt = Database::open_in_memory().unwrap();
    full_index(root.path(), &rebuilt, &[], false).unwrap();
    check(&rebuilt);
}

#[test]
fn incompatible_homonym_does_not_hide_a_genuine_c_definition() {
    let (_root, db) = project(&[
        (
            "caller.c",
            "int helper(void);\nint entry(void) { return helper(); }\n",
        ),
        ("helper.c", "int helper(void) { return 1; }\n"),
        ("decoy.go", "package decoy\nfunc helper() {}\n"),
    ]);
    assert!(trace(&db, "entry", "sym:helper.c#helper"));
    assert!(!trace(&db, "entry", "sym:decoy.go#helper"));
    assert!(impact(&db, "sym:helper.c#helper", "caller.c"));
    assert!(!impact(&db, "sym:decoy.go#helper", "caller.c"));
}

#[test]
fn c_bare_call_cannot_name_a_cpp_method() {
    let (_root, db) = project(&[
        (
            "caller.c",
            "#include <stdio.h>\nint entry(void) { return puts(\"hello\"); }\n",
        ),
        (
            "decoy.cpp",
            "class Printer { public: int puts(const char* s) { return 1; } };\n",
        ),
    ]);
    let methods = db.find_symbols("puts", None).unwrap();
    assert_eq!(methods.len(), 1, "{methods:?}");
    assert_eq!(methods[0].kind, codesage_protocol::SymbolKind::Method);
    assert!(!trace(&db, "entry", "sym:decoy.cpp#Printer::puts"));
    assert!(!impact(&db, "sym:decoy.cpp#Printer::puts", "caller.c"));
}

#[test]
fn language_families_preserve_cpp_and_typescript_interoperation() {
    for files in [
        vec![
            (
                "caller.c",
                "int helper(void);\nint entry(void) { return helper(); }\n",
            ),
            (
                "helper.cpp",
                "extern \"C\" int helper(void) { return 1; }\n",
            ),
        ],
        vec![
            (
                "caller.cpp",
                "extern \"C\" int helper(void);\nint entry() { return helper(); }\n",
            ),
            ("helper.c", "int helper(void) { return 1; }\n"),
        ],
        vec![
            (
                "caller.ts",
                "import { helper } from './helper.js';\nexport function entry() { return helper(); }\n",
            ),
            ("helper.js", "export function helper() { return 1; }\n"),
        ],
        vec![
            (
                "caller.js",
                "import { helper } from './helper.js';\nexport function entry() { return helper(); }\n",
            ),
            ("helper.ts", "export function helper() { return 1; }\n"),
        ],
    ] {
        let (_root, db) = project(&files);
        assert!(trace(&db, "entry", "helper"), "{files:?}");
        assert!(impact(&db, "helper", files[0].0), "{files:?}");
    }
}

#[test]
fn cpp_header_callers_keep_method_edges() {
    let (_root, db) = project(&[
        (
            "caller.h",
            "class Service { public: int work() { return 1; } int entry() { return work(); } };\n",
        ),
        ("main.cpp", "#include \"caller.h\"\n"),
    ]);
    assert!(trace(
        &db,
        "sym:caller.h#Service::entry",
        "sym:caller.h#Service::work"
    ));
}
