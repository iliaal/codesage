use codesage_graph::{
    ReachabilityOptions, TargetError, export_context_for_symbol, find_references, full_index,
    impact_analysis_report, list_dependencies, recommend_tests_with_reachability, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, ExportRequest, FileInfo, FindReferencesRequest, ImpactOptions, ImpactRequest,
    ImpactTarget, Language, ReferenceKind,
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
