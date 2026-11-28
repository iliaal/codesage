use codesage_graph::{
    ReachabilityOptions, TargetError, assess_risk, export_context_for_symbol, file_import_pairs,
    find_references, full_index, impact_analysis, impact_analysis_report, list_dependencies,
    recommend_tests_with_reachability, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, ExportRequest, FileInfo, FindReferencesRequest, ImpactOptions, ImpactRequest,
    ImpactTarget, Language, ReferenceKind, SymbolKind,
};
use codesage_storage::Database;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    for (file, source) in files {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    (dir, db)
}

#[test]
fn native_abi_labels_do_not_replace_rust_or_cpp_references() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "mod math; pub struct Exact; impl Exact { pub fn leaf() {} }\n\
             pub fn caller_rust() { math::leaf(); Exact::leaf(); }\n",
        ),
        ("src/math.rs", "pub fn leaf() {}\n"),
        (
            "native.c",
            "#include \"loop.h\"\n#include \"shared.h\"\n\
             #define PHP_METHOD(class, method) void class##_##method(void)\n\
             static void c_helper(void) {}\n\
             PHP_METHOD(math, leaf) { c_helper(); }\n\
             PHP_METHOD(math, only_native) {}\n\
             PHP_METHOD(missing, outside) {}\n\
             extern void only_native(void);\n\
             void caller_c(void) { only_native(); }\n\
             void c_api(void) {}\n",
        ),
        ("math.cpp", "namespace math { void leaf() {} }\n"),
        (
            "caller.cpp",
            "#include \"shared.h\"\n\
             void caller_cpp() { math::leaf(); }\n\
             void interop() { c_api(); SharedOwner::run(); }\n",
        ),
        (
            "shared.h",
            "#ifdef __cplusplus\nextern \"C\" {\n#endif\n\
             void c_api(void);\n\
             #ifdef __cplusplus\n}\n\
             namespace math { void leaf(); }\n\
             struct SharedOwner { static void run() {} };\n#endif\n",
        ),
        (
            "using.cpp",
            "#include \"shared.h\"\nusing math::leaf;\nvoid caller_using() { leaf(); }\n",
        ),
        (
            "loop.h",
            "#ifdef __cplusplus\nnamespace missing { void outside(); }\n\
             using missing::outside;\n#endif\n",
        ),
        (
            "absent.cpp",
            "#include \"loop.h\"\nvoid caller_absent() { outside(); }\n",
        ),
    ]);
    let c_method = "sym:native.c#math::leaf";
    assert_eq!(
        db.find_symbols("math::leaf", Some(SymbolKind::Method))
            .unwrap()[0]
            .handle()
            .to_string(),
        c_method
    );
    let references = |name: &str, kind| {
        find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: name.into(),
                kind: Some(kind),
            },
        )
        .unwrap()
        .results
    };
    let refs = |name: &str| references(name, ReferenceKind::Call);
    let reaches = |from: &str, to: &str| {
        trace_call_path(
            &db,
            &CallPathRequest {
                from: from.into(),
                to: to.into(),
                max_depth: 1,
            },
        )
        .unwrap()
        .found
    };
    let affected = |target: &str| {
        impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::Symbol {
                    name: target.into(),
                },
                depth: 1,
                source_only: false,
            },
        )
        .unwrap()
    };
    let leaf = refs("math::leaf");
    assert_eq!(leaf.len(), 2, "{leaf:?}");
    for (file, caller, real) in [
        ("src/lib.rs", "caller_rust", "sym:src/math.rs#leaf"),
        ("caller.cpp", "caller_cpp", "sym:math.cpp#math::leaf"),
    ] {
        let row = leaf.iter().find(|row| row.from_file == file).unwrap();
        assert_eq!(row.to.as_deref(), Some(real), "{leaf:?}");
        let caller_handle = format!("sym:{file}#{caller}");
        assert!(reaches(&caller_handle, real));
        assert!(!reaches(&caller_handle, c_method));
        let callers = affected(real);
        let mut files: Vec<_> = callers.iter().map(|row| row.file_path.as_str()).collect();
        files.sort_unstable();
        if file == "caller.cpp" {
            assert_eq!(files, ["caller.cpp", "using.cpp"]);
        } else {
            assert_eq!(files, [file]);
        }
    }
    assert!(affected(c_method).is_empty());
    let unbound_c = refs("only_native");
    assert_eq!(unbound_c.len(), 1, "{unbound_c:?}");
    assert_eq!(
        unbound_c[0].from_handle().unwrap().to_string(),
        "sym:native.c#caller_c"
    );
    assert!(unbound_c[0].to.is_none(), "{unbound_c:?}");
    assert!(!reaches(
        "sym:native.c#caller_c",
        "sym:native.c#math::only_native"
    ));
    let owner = refs("c_helper");
    assert_eq!(owner.len(), 1);
    assert_eq!(owner[0].from_handle().unwrap().to_string(), c_method);
    assert_eq!(owner[0].to.as_deref(), Some("sym:native.c#c_helper"));
    assert!(reaches(
        "sym:src/lib.rs#caller_rust",
        "sym:src/lib.rs#Exact::leaf"
    ));
    assert!(reaches("sym:caller.cpp#interop", "sym:native.c#c_api"));
    assert!(reaches(
        "sym:caller.cpp#interop",
        "sym:shared.h#SharedOwner::run"
    ));

    let actual_cpp = "sym:math.cpp#math::leaf";
    let genuine_using = references("math::leaf", ReferenceKind::Import);
    assert_eq!(genuine_using.len(), 1, "{genuine_using:?}");
    assert_eq!(genuine_using[0].from_file, "using.cpp");
    assert_eq!(genuine_using[0].to.as_deref(), Some(actual_cpp));
    assert!(reaches("sym:using.cpp#caller_using", actual_cpp));
    assert_eq!(
        db.import_targets_for_file("using.cpp").unwrap(),
        ["math.cpp"]
    );
    assert_eq!(
        db.list_file_dependencies("math.cpp").unwrap().imported_by,
        ["using.cpp"]
    );

    let display_only = "sym:native.c#missing::outside";
    let unresolved_using = references("missing::outside", ReferenceKind::Import);
    assert_eq!(unresolved_using.len(), 1, "{unresolved_using:?}");
    assert_eq!(unresolved_using[0].from_file, "loop.h");
    assert!(unresolved_using[0].to.is_none(), "{unresolved_using:?}");
    assert!(affected(display_only).is_empty());
    assert!(!reaches("sym:absent.cpp#caller_absent", display_only));
    assert!(db.file_language_matches("loop.h", Language::Cpp).unwrap());
    let native = list_dependencies(&db, "native.c").unwrap();
    assert!(native.imports.contains(&"file:loop.h".to_owned()));
    assert!(!native.imported_by.contains(&"loop.h".to_owned()));
    assert!(db.import_targets_for_file("loop.h").unwrap().is_empty());
    assert!(db.import_sources_for_file("native.c").unwrap().is_empty());
    assert!(
        db.list_file_dependencies("native.c")
            .unwrap()
            .imported_by
            .is_empty()
    );
    let pairs = file_import_pairs(&db).unwrap();
    assert!(pairs.eager.contains(&("native.c".into(), "loop.h".into())));
    assert!(!pairs.eager.contains(&("loop.h".into(), "native.c".into())));
    assert!(!assess_risk(&db, "native.c").unwrap().in_cycle);
    let file_impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "native.c".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert!(file_impact.iter().any(|row| row.file_path == "caller.cpp"));
    assert!(file_impact.iter().all(|row| row.file_path != "loop.h"));
}

#[test]
fn native_frameless_function_resolves_its_abi_spelling() {
    let (_dir, db) = indexed(&[
        (
            "native.c",
            "#define ZEND_FRAMELESS_FUNCTION(name, n) void zflf_##name##_##n(void)\n\
             static void c_helper(void) {}\n\
             ZEND_FRAMELESS_FUNCTION(leaf, 0) { c_helper(); }\n",
        ),
        (
            "caller.c",
            "extern void leaf(void);\nextern void zflf_leaf_0(void);\n\
             void actual_call(void) { zflf_leaf_0(); }\n\
             void display_call(void) { leaf(); }\n",
        ),
    ]);
    let target = "sym:native.c#zflf_leaf_0";
    let functions = db.find_symbols("leaf", Some(SymbolKind::Function)).unwrap();
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, "leaf");
    assert_eq!(functions[0].handle().to_string(), target);
    assert!(db.symbol_exists("zflf_leaf_0").unwrap());
    let refs = |name: &str| {
        find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: name.into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap()
        .results
    };
    for spelling in ["zflf_leaf_0", target] {
        let actual = refs(spelling);
        assert_eq!(actual.len(), 1, "{actual:?}");
        assert_eq!(actual[0].from_symbol.as_deref(), Some("actual_call"));
        assert_eq!(actual[0].to.as_deref(), Some(target));
    }
    let display = refs("leaf");
    assert_eq!(display.len(), 1);
    assert_eq!(display[0].from_symbol.as_deref(), Some("display_call"));
    assert!(display[0].to.is_none());
    let owner = refs("c_helper");
    assert_eq!(owner.len(), 1);
    assert_eq!(owner[0].from_handle().unwrap().to_string(), target);
    for (caller, expected) in [("actual_call", true), ("display_call", false)] {
        let trace = trace_call_path(
            &db,
            &CallPathRequest {
                from: format!("sym:caller.c#{caller}"),
                to: target.into(),
                max_depth: 1,
            },
        )
        .unwrap();
        assert_eq!(trace.found, expected, "{trace:?}");
    }
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
    assert_eq!(impact.len(), 1, "{impact:?}");
    assert_eq!(impact[0].file_path, "caller.c");
    assert_eq!(impact[0].reasons.len(), 1);
    assert_eq!(impact[0].reasons[0].line, 3);
}

#[test]
fn nested_modules_named_like_entrypoints_do_not_suppress_the_real_root() {
    for (root, outer, nested, api, target) in [
        (
            "src/lib.rs",
            "src/outer.rs",
            "src/outer/lib.rs",
            "src/api.rs",
            vec!["--lib"],
        ),
        (
            "src/bin/tool/main.rs",
            "src/bin/tool/outer.rs",
            "src/bin/tool/outer/main.rs",
            "src/bin/tool/api.rs",
            vec!["--bin", "tool"],
        ),
    ] {
        let declaration = if root.ends_with("lib.rs") {
            "mod lib;"
        } else {
            "mod main;"
        };
        let source =
            "mod outer; mod api; use crate::api::f; pub fn run() { f(); } fn main() { run(); }";
        let (dir, db) = indexed(&[
            (
                "Cargo.toml",
                "[package]\nname=\"roots_probe\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            ),
            (root, source),
            (outer, declaration),
            (nested, ""),
            (api, "pub fn f() {}"),
        ]);
        let output = std::process::Command::new("cargo")
            .args(["check", "--offline", "--quiet"])
            .args(target)
            .arg("--target-dir")
            .arg(dir.path().join("target"))
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "f".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(refs.results.len(), 1);
        assert_eq!(refs.results[0].to, Some(format!("sym:{api}#f")), "{root}");
        assert!(
            list_dependencies(&db, api)
                .unwrap()
                .imported_by
                .contains(&root.to_string())
        );
        assert!(
            list_dependencies(&db, nested)
                .unwrap()
                .imported_by
                .contains(&outer.to_string())
        );
    }
}

#[test]
fn qualified_calls_preserve_the_definitions_owner_scope() {
    for (call, definition, qualified) in [
        ("api::f()", "pub fn f() {}", Some("f")),
        (
            "api::inner::f()",
            "pub mod inner { pub fn f() {} } pub fn g() {}",
            Some("f"),
        ),
        (
            "api::f()",
            "pub mod inner { pub fn g() {} } pub fn f() {}",
            Some("f"),
        ),
        ("api::f()", "pub mod inner { pub fn f() {} }", None),
        (
            "api::f()",
            "pub mod inner { pub fn f() {} } pub fn f() {}",
            None,
        ),
    ] {
        let source = format!("mod api; pub fn run() {{ {call}; }}");
        let definitions = format!("{definition} pub struct S; impl S {{ pub fn f() {{}} }}");
        let (_dir, db) = indexed(&[("src/lib.rs", &source), ("src/api.rs", &definitions)]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "f".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(refs.results.len(), 1);
        assert_eq!(
            refs.results[0].to,
            qualified.map(|name| format!("sym:src/api.rs#{name}")),
            "{call}: {:?}",
            db.find_symbols("f", None).unwrap()
        );
        assert!(
            !trace_call_path(
                &db,
                &CallPathRequest {
                    from: "run".into(),
                    to: "S::f".into(),
                    max_depth: 3,
                }
            )
            .unwrap()
            .found
        );
        if qualified.is_none() {
            // `f` also names `S::f`, so address the plain definitions by
            // handle. Two of them share this file, name, and line, so one
            // handle names both: the trace then refuses as ambiguous rather
            // than picking one. Either way no chain to a plain `f` exists.
            let plain: Vec<String> = db
                .find_symbols("f", None)
                .unwrap()
                .iter()
                .filter(|s| s.file_path == "src/api.rs" && s.qualified_name == "f")
                .map(|s| s.handle().to_string())
                .collect();
            assert!(!plain.is_empty(), "{call}");
            for handle in plain {
                let request = CallPathRequest {
                    from: "run".into(),
                    to: handle.clone(),
                    max_depth: 3,
                };
                match trace_call_path(&db, &request) {
                    Ok(report) => assert!(!report.found, "{call}: {handle}"),
                    Err(error) => assert!(
                        matches!(
                            error.downcast_ref::<TargetError>(),
                            Some(TargetError::Ambiguous { .. })
                        ),
                        "{call}: {handle}: {error:#}"
                    ),
                }
            }
        }
    }
}

#[test]
fn module_declarations_do_not_compete_with_an_explicit_function_import() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "mod a; mod b; use a::f; pub fn run() { f(); }",
        ),
        ("src/a.rs", "pub fn f() {}"),
        ("src/b.rs", "pub fn f() {}"),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "f".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to.as_deref(), Some("sym:src/a.rs#f"));
}

#[test]
fn module_declarations_do_not_bind_external_imports_to_local_functions() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "mod helper; use std::mem::drop; pub fn run() { drop(()); }",
        ),
        ("src/helper.rs", "pub fn drop() {}"),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "drop".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to, None);
}

#[test]
fn qualified_module_calls_resolve_without_importing_the_function() {
    let (_dir, db) = indexed(&[
        ("src/lib.rs", "mod a; mod b; pub fn run() { a::f(); }"),
        ("src/a.rs", "pub fn f() {}"),
        ("src/b.rs", "pub fn f() {}"),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "f".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to.as_deref(), Some("sym:src/a.rs#f"));
}

#[test]
fn qualified_module_calls_start_in_the_callers_module() {
    let (_dir, db) = indexed(&[
        ("src/lib.rs", "mod outer; mod a;"),
        ("src/outer.rs", "mod a; pub fn run() { a::f(); }"),
        ("src/outer/a.rs", "pub fn f() {}"),
        ("src/a.rs", "pub fn f() {}"),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "f".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to.as_deref(), Some("sym:src/outer/a.rs#f"));
}

#[test]
fn rust_context_limit_is_disclosed_by_every_public_resolution_report() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "mod api; mod tests; use api::f; pub fn run() { f(); }",
        ),
        ("src/api.rs", "pub fn f() {}"),
        ("src/decoy.rs", "pub fn f() {}"),
        (
            "src/tests/mod.rs",
            "use crate::api::f; #[test] fn checks_f() { f(); }",
        ),
    ]);
    let call = CallPathRequest {
        from: "run".into(),
        // `src/decoy.rs` defines `f` too; the handle names the imported one.
        to: "sym:src/api.rs#f".into(),
        max_depth: 3,
    };
    assert!(trace_call_path(&db, &call).unwrap().found);
    let baseline = recommend_tests_with_reachability(
        &db,
        &["src/api.rs".into()],
        &ReachabilityOptions::default(),
    )
    .unwrap();
    assert!(!baseline.reach_walk_capped);
    assert_eq!(baseline.reachable_total, 1);
    for index in 0..16_385 {
        db.upsert_file(&FileInfo {
            path: format!("tests/unrelated_{index}.rs"),
            language: Language::Rust,
            content_hash: "empty".into(),
            is_test: false,
        })
        .unwrap();
    }
    let mut missing = Vec::new();
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "f".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    if !refs
        .to_resolution
        .is_some_and(|resolution| resolution.capped)
    {
        missing.push("find_references.to_resolution.capped");
    }
    let trace = trace_call_path(&db, &call).unwrap();
    if !trace.bounded {
        missing.push("trace_call_path.bounded");
    }
    let impact = impact_analysis_report(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "src/api.rs".into(),
            },
            depth: 2,
            source_only: false,
        },
        &ImpactOptions::default(),
    )
    .unwrap();
    if serde_json::to_value(impact).unwrap()["bounded"] != true {
        missing.push("impact_analysis.bounded");
    }
    let tests = recommend_tests_with_reachability(
        &db,
        &["src/api.rs".into()],
        &ReachabilityOptions::default(),
    )
    .unwrap();
    if !tests.reach_walk_capped {
        missing.push("recommend_tests.reach_walk_capped");
    }
    let bundle = export_context_for_symbol(
        &db,
        "run",
        &ExportRequest {
            query: None,
            symbol: Some("run".into()),
            include_callers: false,
            include_callees: true,
            limit: 10,
        },
    )
    .unwrap();
    if serde_json::to_value(bundle).unwrap()["bounded"] != true {
        missing.push("export_context.bounded");
    }
    assert!(
        missing.is_empty(),
        "context exhaustion was not disclosed: {missing:?}"
    );
}
