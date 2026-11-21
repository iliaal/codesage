use codesage_graph::{assess_risk, find_references, full_index, impact_analysis, trace_call_path};
use codesage_protocol::{CallPathRequest, FindReferencesRequest, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::create_dir(dir.path().join(".codesage")).unwrap();
    let db = Database::open(&dir.path().join(".codesage/index.db")).unwrap();
    let stats = full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_failed, 0);
    (dir, db)
}

fn workspace(caller: &str) -> (tempfile::TempDir, Database) {
    indexed(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = ['crates/graph', 'crates/cli', 'crates/decoy']\n[workspace.dependencies]\ncodesage-graph = { path = 'crates/graph' }\n",
        ),
        (
            "crates/graph/Cargo.toml",
            "[package]\nname = 'codesage-graph'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "crates/cli/Cargo.toml",
            "[package]\nname = 'codesage'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\ncodesage-graph.workspace = true\n",
        ),
        (
            "crates/decoy/Cargo.toml",
            "[package]\nname = 'decoy'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "crates/graph/src/lib.rs",
            "mod search;\npub use search::{search_page};\n",
        ),
        ("crates/graph/src/search.rs", "pub fn search_page() {}\n"),
        ("crates/decoy/src/lib.rs", "pub fn search_page() {}\n"),
        ("crates/cli/src/main.rs", caller),
    ])
}

fn reaches(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 3,
        },
    )
    .unwrap()
    .found
}

#[test]
fn workspace_public_reexport_resolves_in_references_impact_trace_and_risk() {
    let (_dir, db) = workspace(
        "use codesage_graph::{search_page};\nfn main() {\ncodesage_graph::search_page();\nsearch_page();\n}\n",
    );
    let target = "sym:crates/graph/src/search.rs#search_page";
    assert!(reaches(&db, "main", target));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "search_page".into(),
            kind: None,
        },
    )
    .unwrap();
    let callers: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.from_file == "crates/cli/src/main.rs")
        .collect();
    assert_eq!(callers.len(), 3);
    assert!(
        callers.iter().all(|row| row.to.as_deref() == Some(target)),
        "{callers:?}"
    );
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert!(
        impact
            .iter()
            .any(|row| row.file_path == "crates/cli/src/main.rs"),
        "{impact:?}"
    );
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/decoy/src/lib.rs#search_page"
    ));
    let risk = assess_risk(&db, "crates/graph/src/search.rs").unwrap();
    assert!(
        risk.top_symbols
            .iter()
            .find(|symbol| symbol.name == "search_page")
            .unwrap()
            .why
            .contains("4 refs"),
        "{:?}",
        risk.top_symbols
    );
}

#[test]
fn dependency_aliases_custom_library_names_and_renamed_exports_resolve() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers = ['api', 'app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname = 'implementation'\nversion = '0.0.0'\nedition = '2024'\n[lib]\nname = 'custom_api'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\nrenamed = { path = '../api', package = 'implementation' }\n",
        ),
        (
            "api/src/lib.rs",
            "mod internal;\npub use internal::{original as exported};\n",
        ),
        ("api/src/internal.rs", "pub fn original() {}\n"),
        (
            "app/src/main.rs",
            "use renamed::{exported as local};\nfn direct() { renamed::exported(); }\nfn imported() { local(); }\nfn wrong() { implementation::exported(); custom_api::exported(); }\n",
        ),
    ]);
    assert!(reaches(&db, "direct", "original"));
    assert!(reaches(&db, "imported", "original"));
    assert!(!reaches(&db, "wrong", "original"));

    let (_dir, db) = indexed(&[
        (
            "api/Cargo.toml",
            "[package]\nname = 'implementation'\nversion = '0.0.0'\nedition = '2024'\n[lib]\nname = 'custom_api'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\nimplementation = { path = '../api' }\n",
        ),
        ("api/src/lib.rs", "pub fn exported() {}\n"),
        (
            "app/src/main.rs",
            "fn main() { custom_api::exported(); }\nfn wrong() { implementation::exported(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "exported"));
    assert!(!reaches(&db, "wrong", "exported"));
}

#[test]
fn actual_codesage_search_page_call_resolves_from_cli_source() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let paths = [
        "Cargo.toml",
        "crates/graph/Cargo.toml",
        "crates/cli/Cargo.toml",
        "crates/graph/src/lib.rs",
        "crates/graph/src/search.rs",
        "crates/cli/src/main.rs",
    ];
    let sources: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read_to_string(root.join(path)).unwrap())
        .collect();
    let files: Vec<_> = paths
        .iter()
        .zip(&sources)
        .map(|(path, source)| (*path, source.as_str()))
        .collect();
    let (_dir, db) = indexed(&files);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "search_page".into(),
            kind: None,
        },
    )
    .unwrap();
    let calls: Vec<_> = refs
        .results
        .iter()
        .filter(|row| {
            row.from_file == "crates/cli/src/main.rs"
                && row.to_name == "codesage_graph::search_page"
        })
        .collect();
    assert!(!calls.is_empty());
    assert!(
        calls
            .iter()
            .all(|row| row.to.as_deref() == Some("sym:crates/graph/src/search.rs#search_page")),
        "{calls:?} bounds: {:?}",
        refs.to_resolution
    );
}

#[test]
fn private_imports_modules_and_definitions_are_not_public_exports() {
    for library in [
        "mod search; use search::search_page;",
        "mod search; pub(crate) use search::search_page;",
        "mod search;",
        "mod search; #[cfg(feature = \"unknown\")] pub use search::search_page;",
    ] {
        let (dir, db) = workspace(
            "fn main() { codesage_graph::search_page(); codesage_graph::search::search_page(); }\n",
        );
        std::fs::write(dir.path().join("crates/graph/src/lib.rs"), library).unwrap();
        full_index(dir.path(), &db, &[], false).unwrap();
        assert!(
            !reaches(&db, "main", "sym:crates/graph/src/search.rs#search_page"),
            "{library}"
        );
    }
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/search.rs"),
        "pub(crate) fn search_page() {}\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
}

#[test]
fn source_divergence_missing_dependencies_and_escape_paths_do_not_create_edges() {
    for path in [
        "crates/graph/src/lib.rs",
        "crates/graph/src/search.rs",
        "crates/cli/src/main.rs",
    ] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        let path = dir.path().join(path);
        let source = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("{source}\n// changed\n")).unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
    for dependency in [
        "",
        "[dependencies]\ncodesage-graph = { path = '../../../outside' }\n",
        "[dependencies]\ncodesage-graph = { path = '../decoy' }\n",
    ] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        std::fs::write(
            dir.path().join("crates/cli/Cargo.toml"),
            format!(
                "[package]\nname = 'codesage'\nversion = '0.0.0'\nedition = '2024'\n{dependency}"
            ),
        )
        .unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
}

#[test]
fn public_module_paths_and_chained_reexports_resolve_without_private_shortcuts() {
    let (dir, db) = workspace(
        "fn main() { codesage_graph::search_page(); }\nfn private() { codesage_graph::search::search_page(); }\n",
    );
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub mod bridge;\nmod search;\npub use bridge::search_page;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("crates/graph/src/bridge.rs"),
        "pub use crate::search::search_page;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    assert!(!reaches(
        &db,
        "private",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub mod search;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(reaches(
        &db,
        "private",
        "sym:crates/graph/src/search.rs#search_page"
    ));
}

#[test]
fn shadowed_imports_duplicate_definitions_and_export_cycles_are_unresolved() {
    let (_dir, db) = workspace(
        "use codesage_graph::search_page;\nfn main() { let search_page = || {}; search_page(); }\n",
    );
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/search.rs"),
        "pub fn search_page() {}\npub fn search_page() {}\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page@1"
    ));
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub use self::search_page;\nmod search;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page@1"
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_manifest_or_export_source_is_not_trusted() {
    for relative in ["crates/graph/Cargo.toml", "crates/graph/src/lib.rs"] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        let path = dir.path().join(relative);
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &path).unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
}

#[test]
fn a_packages_binary_can_name_its_library_but_library_source_cannot_name_itself() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'own-api'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "pub fn exported() {}\npub fn wrong() { own_api::exported(); }\n",
        ),
        ("src/main.rs", "fn main() { own_api::exported(); }\n"),
    ]);
    assert!(reaches(&db, "main", "exported"));
    assert!(!reaches(&db, "wrong", "exported"));
}

#[test]
fn external_export_walk_reports_its_bound() {
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "mod bridge0;\npub use bridge0::search_page;\nmod search;\n",
    )
    .unwrap();
    for index in 0..40 {
        let directory = dir.path().join(format!(
            "crates/graph/src/{}",
            (0..index)
                .map(|i| format!("bridge{i}/"))
                .collect::<String>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("bridge{index}.rs")),
            format!(
                "pub mod bridge{}; pub use bridge{}::search_page;",
                index + 1,
                index + 1
            ),
        )
        .unwrap();
    }
    full_index(dir.path(), &db, &[], false).unwrap();
    let report = trace_call_path(
        &db,
        &CallPathRequest {
            from: "main".into(),
            to: "sym:crates/graph/src/search.rs#search_page".into(),
            max_depth: 3,
        },
    )
    .unwrap();
    assert!(!report.found);
    assert!(report.bounded);
}

#[test]
fn external_resolution_preserves_local_edges_with_unrelated_shadowing() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname='ordinary'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "src/main.rs",
            "fn helper() {}\nfn unrelated(helper: u32) { std::hint::black_box(helper); }\nfn main() { helper(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "helper"));
}

#[test]
fn external_resolution_preserves_local_edges_with_stale_source() {
    let (dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname='ordinary'\nversion='0.0.0'\nedition='2024'\n",
        ),
        ("src/main.rs", "fn helper() {}\nfn main() { helper(); }\n"),
    ]);
    assert!(reaches(&db, "main", "helper"));
    let path = dir.path().join("src/main.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, format!("{source}\n// unrelated comment\n")).unwrap();
    assert!(reaches(&db, "main", "helper"));
}

#[test]
fn an_unsupported_dependency_edition_does_not_resolve_to_the_wrong_module() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2015'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub mod inner;\npub mod target;\n"),
        ("api/src/target.rs", "pub fn foo() {}\n"),
        ("api/src/inner.rs", "mod target;\npub use target::foo;\n"),
        ("api/src/inner/target.rs", "pub fn foo() {}\n"),
        ("app/src/main.rs", "fn main() { api::inner::foo(); }\n"),
    ]);
    assert!(!reaches(&db, "main", "sym:api/src/inner/target.rs#foo"));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let calls: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.from_file == "app/src/main.rs")
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].to.is_none());
}

#[test]
fn pattern_bindings_do_not_become_external_function_calls() {
    for body in [
        "for local in [|| {}] { local(); }",
        "if let Some(local) = Some(|| {}) { local(); }",
        "while let Some(local) = Some(|| {}) { local(); break; }",
        "match Some(|| {}) { Some(local) => local(), None => () }",
        "let call = |(local,): (fn(),)| local(); call((|| {},));",
        "struct Container { local: fn() } let Container { local } = Container { local: || {} }; local();",
    ] {
        let caller = format!("use api::foo as local;\nfn main() {{ {body} }}\n");
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", &caller),
        ]);
        assert!(!reaches(&db, "main", "sym:api/src/lib.rs#foo"), "{body}");
    }
}

#[test]
fn namespace_bindings_and_unsupported_globs_do_not_name_a_homonymous_dependency() {
    let cases = [
        (
            "extern",
            "extern crate other as api; fn main() { api::foo(); }",
            "main",
        ),
        (
            "nested extern",
            "fn main() { extern crate other as api; api::foo(); }",
            "main",
        ),
        (
            "extern import source",
            "extern crate other as api; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        (
            "alias import source",
            "use other as api; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        ("glob", "use other::*; fn main() { api::foo(); }", "main"),
        (
            "nested glob",
            "fn main() { use other::*; api::foo(); }",
            "main",
        ),
        (
            "glob import source",
            "use other::*; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        (
            "generic type",
            "trait Marker { fn foo(); } fn invoke<api: Marker>() { api::foo(); }",
            "invoke",
        ),
    ];
    let mut false_edges = Vec::new();
    for (label, caller, from) in cases {
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {} pub mod api;\n"),
            ("other/src/api.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", caller),
        ]);
        if reaches(&db, from, "sym:api/src/lib.rs#foo") {
            false_edges.push(label);
        }
    }
    assert!(
        false_edges.is_empty(),
        "wrong dependency edges: {false_edges:?}"
    );
}

#[test]
fn a_same_file_parent_glob_without_the_dependency_name_keeps_qualified_calls() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() {}\n"),
        (
            "app/src/main.rs",
            "fn main() { api::foo(); } mod caller { use super::*; pub fn invoke() { api::foo(); } }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
    assert!(reaches(&db, "invoke", "sym:api/src/lib.rs#foo"));
}

#[test]
fn macro_invocations_that_can_change_the_callers_namespace_are_unresolved() {
    let cases = [
        "macro_rules! shadow { () => { use other as api; } } shadow!(); fn main() { api::foo(); }",
        "macro_rules! shadow { () => { use other as api; } } fn main() { shadow!(); api::foo(); }",
        "#[rewrite] fn main() { api::foo(); }",
        "macro_rules! assert { () => { use other as api; } } fn main() { assert!(); api::foo(); }",
        "macro_rules! shadow { () => { use other as api; } } fn main() { shadow!(); assert!(api::foo() == ()); }",
        "fn main() { assert!({ use other as api; api::foo() } == ()); }",
        "use api::foo; fn main() { let v = [0]; assert!(v.iter().all(|foo| foo())); }",
        "use api::foo; fn main() { assert!(match 1 { foo => foo() }); }",
        "use api::foo; fn main() { assert!({ let foo = || (); foo() } == ()); }",
        "use api::foo; fn main() { let v = [0]; v.iter().all(|foo| foo()); }",
    ];
    let mut false_edges = Vec::new();
    for caller in cases {
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", caller),
        ]);
        if reaches(&db, "main", "sym:api/src/lib.rs#foo") {
            false_edges.push(caller);
        }
    }
    assert!(
        false_edges.is_empty(),
        "macro namespace edges: {false_edges:?}"
    );
}

#[test]
fn calls_inside_std_expression_macros_resolve_through_dependencies() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        (
            "api/src/lib.rs",
            "pub fn foo() -> i32 { 1 }\npub fn bar() -> i32 { 2 }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1);\n}\nfn other() {\n    let _v = std::vec![api::bar()];\n}\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
    assert!(reaches(&db, "other", "sym:api/src/lib.rs#bar"));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let rows: Vec<_> = refs
        .results
        .iter()
        .map(|row| (row.from_file.as_str(), row.line, row.to.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [("app/src/main.rs", 2, Some("sym:api/src/lib.rs#foo"))]
    );
}

#[test]
fn imported_and_nested_macros_that_rebind_std_names_keep_calls_unresolved() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "other/Cargo.toml",
            "[package]\nname='other'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2021'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() -> i32 { 1 }\n"),
        (
            "other/src/lib.rs",
            "pub fn foo() -> i32 { 2 }\n#[macro_export]\nmacro_rules! assert { () => { use other as api; }; ($e:expr) => { () }; }\n",
        ),
        (
            "app/src/bin/a.rs",
            "macro_rules! shadow { () => { use other as api; } }\nfn main() {\n    println!(\"{}\", { shadow!(); api::foo() });\n}\n",
        ),
        (
            "app/src/bin/h.rs",
            "macro_rules! shadow { () => { use other as api; } }\nfn main() {\n    let s = format!(\"{}\", { shadow!(); api::foo() });\n    println!(\"{s}\");\n}\n",
        ),
        (
            "app/src/bin/b.rs",
            "use other::assert;\nfn main() {\n    assert!();\n    std::println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/c.rs",
            "#[macro_use]\nextern crate other;\nfn main() {\n    assert!();\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/g/m.rs",
            "macro_rules! assert { () => { use other as api; } }\n",
        ),
        (
            "app/src/bin/g/main.rs",
            "#[macro_use]\nmod m;\nfn main() {\n    assert!();\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/e.rs",
            "use other::assert;\nfn main() {\n    println!(\"{:?}\", assert!(api::foo()));\n}\n",
        ),
        (
            "app/src/bin/e2.rs",
            "macro_rules! assert { ($e:expr) => { () } }\nfn main() {\n    println!(\"{:?}\", assert!(api::foo()));\n}\n",
        ),
        (
            "app/src/bin/ctl.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1);\n}\n",
        ),
        (
            "app/src/bin/d.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1 // why\n    );\n}\n",
        ),
        (
            "app/src/bin/f.rs",
            "fn main() {\n    println!(\"{x}\", x = api::foo());\n}\n",
        ),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let api = Some("sym:api/src/lib.rs#foo");
    let mut bound: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.to.as_deref() == api)
        .map(|row| (row.from_file.as_str(), row.line))
        .collect();
    bound.sort_unstable();
    assert_eq!(
        bound,
        [
            ("app/src/bin/ctl.rs", 2),
            ("app/src/bin/d.rs", 2),
            ("app/src/bin/f.rs", 2),
        ]
    );
}

fn foo_bindings(db: &Database, symbol: &str) -> Vec<(String, u32, Option<String>)> {
    let mut rows: Vec<_> = find_references(
        db,
        &FindReferencesRequest {
            symbol_name: symbol.into(),
            kind: None,
        },
    )
    .unwrap()
    .results
    .into_iter()
    .filter(|row| row.kind == codesage_protocol::ReferenceKind::Call)
    .map(|row| (row.from_file, row.line, row.to))
    .collect();
    rows.sort();
    rows
}

fn api_other_app(files: &[(&'static str, &'static str)]) -> (tempfile::TempDir, Database) {
    let mut all = vec![
        ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "other/Cargo.toml",
            "[package]\nname='other'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2021'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() -> i32 { 1 }\n"),
    ];
    all.extend_from_slice(files);
    indexed(&all)
}

#[test]
fn a_std_named_macro_from_another_file_leaves_its_arguments_unbound() {
    let (_dir, db) = api_other_app(&[
        ("other/src/lib.rs", "pub fn foo() -> i32 { 2 }\n"),
        (
            "app/src/main.rs",
            "#[macro_use]\nmod macros;\nmod x;\nmod y;\nfn main() {\n    x::g();\n    println!(\"{}\", x::f());\n}\n",
        ),
        (
            "app/src/macros.rs",
            "macro_rules! assert { () => { use other as api; }; ($e:expr) => { () }; }\n",
        ),
        (
            "app/src/x.rs",
            "pub fn f() -> i32 {\n    assert!();\n    api::foo()\n}\npub fn helper() -> i32 { 1 }\npub fn g() {\n    assert!(helper());\n}\npub fn nested() -> i32 { 3 }\npub fn h() {\n    println!(\"{:?}\", assert!(nested()));\n}\n",
        ),
        (
            "app/src/y.rs",
            "pub fn k() -> i32 {\n    crate::x::helper()\n}\n",
        ),
    ]);
    assert_eq!(foo_bindings(&db, "foo"), [("app/src/x.rs".into(), 3, None)]);
    assert_eq!(
        foo_bindings(&db, "helper"),
        [
            ("app/src/x.rs".into(), 7, None),
            (
                "app/src/y.rs".into(),
                2,
                Some("sym:app/src/x.rs#helper".into())
            ),
        ]
    );
    assert_eq!(
        foo_bindings(&db, "nested"),
        [("app/src/x.rs".into(), 11, None)]
    );
    assert!(!reaches(&db, "g", "sym:app/src/x.rs#helper"));
    assert!(!reaches(&db, "h", "sym:app/src/x.rs#nested"));
    assert!(reaches(&db, "k", "sym:app/src/x.rs#helper"));
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "sym:app/src/x.rs#helper".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    let callers = serde_json::to_string(&impact).unwrap();
    assert!(!callers.contains("x.rs#g"), "{callers}");
}

#[test]
fn a_use_renaming_a_macro_to_a_std_name_leaves_the_scope_unresolved() {
    let (_dir, db) = api_other_app(&[
        (
            "other/src/lib.rs",
            "pub fn foo() -> i32 { 2 }\n#[macro_export]\nmacro_rules! check { () => { use other as api; } }\n",
        ),
        (
            "app/src/main.rs",
            "mod x;\nfn main() {\n    println!(\"{}\", x::f());\n}\n",
        ),
        (
            "app/src/x.rs",
            "use other::check as assert;\npub fn f() -> i32 {\n    assert!();\n    api::foo()\n}\n",
        ),
    ]);
    assert_eq!(foo_bindings(&db, "foo"), [("app/src/x.rs".into(), 4, None)]);
}

#[test]
fn c_defines_with_std_macro_names_do_not_shadow_rust_macros() {
    let (_dir, db) = api_other_app(&[
        ("other/src/lib.rs", "pub fn foo() -> i32 { 2 }\n"),
        (
            "csrc/x.c",
            "#define vec 1\nint main(void) { return vec; }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    let _v = vec![api::foo()];\n}\n",
        ),
    ]);
    assert_eq!(
        foo_bindings(&db, "foo"),
        [(
            "app/src/main.rs".into(),
            2,
            Some("sym:api/src/lib.rs#foo".into())
        )]
    );
}

#[test]
fn macros_in_unrelated_function_bodies_do_not_hide_qualified_calls() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() {}\n"),
        (
            "app/src/main.rs",
            "macro_rules! expression { () => { () } } fn unrelated() { expression!(); } fn main() { api::foo(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
}

#[test]
fn build_scripts_do_not_resolve_through_normal_dependencies() {
    let mut false_edges = Vec::new();
    for build_path in [
        "build.rs",
        "src/build.rs",
        "src/main.rs",
        "src/bin/builder.rs",
    ] {
        let manifest = format!(
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\nbuild='{build_path}'\n[dependencies]\napi={{path='../api'}}\n[build-dependencies]\nother={{path='../other'}}\n"
        );
        let build_file = format!("app/{build_path}");
        let mut files = vec![
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n[lib]\nname='api'\n",
            ),
            ("app/Cargo.toml", manifest.as_str()),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {}\n"),
            (build_file.as_str(), "fn main() { api::foo(); }\n"),
        ];
        if build_path != "src/main.rs" {
            files.push((
                "app/src/main.rs",
                if build_path == "src/build.rs" {
                    "mod build; fn main() {}"
                } else {
                    "fn main() {}"
                },
            ));
        }
        let (_dir, db) = indexed(&files);
        if reaches(
            &db,
            &format!("sym:{build_file}#main"),
            "sym:api/src/lib.rs#foo",
        ) {
            false_edges.push(build_path);
        }
    }
    assert!(
        false_edges.is_empty(),
        "build dependency edges: {false_edges:?}"
    );
}
