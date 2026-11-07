use codesage_graph::{
    DescribeOptions, describe, find_references, full_index, impact_analysis, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, DescribeDetail, FindReferencesRequest, ImpactRequest, ImpactTarget,
    ReferenceKind, SymbolKind,
};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"macro_binding_probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    for (file, source) in files {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
    assert_eq!(
        full_index(root.path(), &db, &[], false)
            .unwrap()
            .files_indexed,
        files.len()
    );
    (root, db)
}

fn handle(db: &Database, name: &str, kind: SymbolKind) -> String {
    let symbols = db.find_symbols(name, None).unwrap();
    let matches: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .collect();
    assert_eq!(matches.len(), 1, "{name}: {symbols:?}");
    matches[0].handle().to_string()
}

fn trace(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.to_string(),
            to: to.to_string(),
            max_depth: 1,
        },
    )
    .unwrap()
    .found
}

#[test]
fn reverse_impact_keeps_the_function_owner_among_macro_namesakes() {
    let macro_definition = "#[macro_export] macro_rules! dual { () => {} }";
    let function_definition = "pub fn dual() { crate::sink::sink(); }";
    for separator in [" ", "\n"] {
        for reverse in [false, true] {
            let definitions = if reverse {
                format!("{function_definition}{separator}{macro_definition}")
            } else {
                format!("{macro_definition}{separator}{function_definition}")
            };
            let source = format!(
                "{definitions}\npub mod sink;\npub mod macro_caller;\npub mod function_caller;\n"
            );
            let (_root, db) = project(&[
                ("src/lib.rs", &source),
                ("src/sink.rs", "pub fn sink() {}\n"),
                (
                    "src/macro_caller.rs",
                    "pub fn run_macro() { crate::dual!(); }\n",
                ),
                (
                    "src/function_caller.rs",
                    "pub fn run_function() { crate::dual(); }\n",
                ),
            ]);
            let report = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: "sym:src/sink.rs#sink".into(),
                    },
                    depth: 2,
                    source_only: false,
                },
            )
            .unwrap();
            let mut files: Vec<_> = report
                .iter()
                .map(|entry| (entry.file_path.as_str(), entry.distance))
                .collect();
            files.sort();
            assert_eq!(
                files,
                [("src/function_caller.rs", 2), ("src/lib.rs", 1)],
                "{source}: {report:?}"
            );
            assert!(
                report
                    .iter()
                    .flat_map(|entry| &entry.reasons)
                    .all(|reason| reason.kind == ReferenceKind::Call)
            );
        }
    }
}

#[test]
fn empty_same_line_macro_cannot_inherit_its_namesake_functions_callees() {
    let (_root, db) = project(&[(
        "src/lib.rs",
        "#[macro_export] macro_rules! dual { () => {} } pub fn dual() { helper(); }\npub fn helper() {}\npub fn macro_run() { dual!(); }\npub fn function_run() { dual(); }\n",
    )]);
    for (caller, found) in [("macro_run", false), ("function_run", true)] {
        let report = trace_call_path(
            &db,
            &CallPathRequest {
                from: format!("sym:src/lib.rs#{caller}"),
                to: "sym:src/lib.rs#helper".into(),
                max_depth: 2,
            },
        )
        .unwrap();
        assert_eq!(report.found, found, "{caller}: {report:?}");
    }
    let risk = codesage_graph::assess_risk(&db, "src/lib.rs").unwrap();
    let namesakes: Vec<_> = risk
        .top_symbols
        .iter()
        .filter(|symbol| symbol.name == "dual")
        .collect();
    assert_eq!(namesakes.len(), 2);
    for symbol in namesakes {
        assert!(!symbol.shared, "{symbol:?}");
        assert!(!symbol.bounded, "{symbol:?}");
        assert!(symbol.why.contains("1 refs"), "{symbol:?}");
    }
}

#[test]
fn qualified_macro_ignores_an_exact_spelling_method_in_another_module() {
    let (_root, db) = project(&[
        (
            "src/lib.rs",
            "pub mod macros;\nmod other;\npub fn run() { macros::dual!(); }\n",
        ),
        (
            "src/macros.rs",
            "macro_rules! dual { () => {} }\npub(crate) use dual;\n",
        ),
        (
            "src/other.rs",
            "pub struct macros;\nimpl macros { pub fn dual() {} }\n",
        ),
    ]);
    let macro_handle = "sym:src/macros.rs#dual";
    let rows = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "dual".into(),
            kind: Some(ReferenceKind::MacroCall),
        },
    )
    .unwrap()
    .results;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].to.as_deref(), Some(macro_handle));
    assert!(trace(&db, "sym:src/lib.rs#run", macro_handle));
    assert!(!trace(
        &db,
        "sym:src/lib.rs#run",
        "sym:src/other.rs#macros::dual"
    ));
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: macro_handle.into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert_eq!(impact.len(), 1);
    assert_eq!(impact[0].file_path, "src/lib.rs");
    assert_eq!(impact[0].reasons[0].kind, ReferenceKind::MacroCall);
}

#[test]
fn qualified_same_line_macro_and_function_are_separate_namespace_candidates() {
    for (calls, kinds) in [
        (
            "crate::dual!(); crate::dual();",
            [ReferenceKind::MacroCall, ReferenceKind::Call],
        ),
        (
            "crate::dual(); crate::dual!();",
            [ReferenceKind::Call, ReferenceKind::MacroCall],
        ),
    ] {
        let caller = format!("pub fn run() {{ {calls} }}\n");
        let (_root, db) = project(&[
            (
                "src/lib.rs",
                "#[macro_export] macro_rules! dual { () => {} } pub fn dual() {}\npub mod caller;\n",
            ),
            ("src/caller.rs", &caller),
        ]);
        let report = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "dual".into(),
                kind: None,
            },
        )
        .unwrap();
        assert_eq!(report.definition_count, 2);
        assert_eq!(report.results.len(), 2);
        assert_eq!(
            report
                .results
                .iter()
                .map(|row| row.kind)
                .collect::<Vec<_>>(),
            kinds
        );
        assert!(
            report
                .results
                .iter()
                .all(|row| row.to.as_deref() == Some("sym:src/lib.rs#dual@1"))
        );
        for kind in kinds {
            let filtered = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "dual".into(),
                    kind: Some(kind),
                },
            )
            .unwrap();
            assert_eq!(filtered.results.len(), 1);
            assert_eq!(filtered.results[0].kind, kind);
            assert_eq!(
                filtered.results[0].to.as_deref(),
                Some("sym:src/lib.rs#dual@1")
            );
        }
    }
}

fn impact_calls(db: &Database, target: &str) -> Vec<(String, ReferenceKind, u32)> {
    let mut calls: Vec<_> = impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .flat_map(|entry| {
        entry
            .reasons
            .into_iter()
            .filter(|reason| matches!(reason.kind, ReferenceKind::Call | ReferenceKind::MacroCall))
            .map(move |reason| (entry.file_path.clone(), reason.kind, reason.line))
    })
    .collect();
    calls.sort_by(|a, b| (&a.0, a.2).cmp(&(&b.0, b.2)));
    calls
}

#[test]
fn unindexed_macro_and_local_closure_do_not_bind_to_the_other_namespace() {
    let (root, db) = project(&[(
        "src/lib.rs",
        "pub fn format() {}\nmacro_rules! only_macro { () => {} }\npub fn run() {\n    format!(\"{}\", 1);\n    let only_macro = || ();\n    only_macro();\n}\n",
    )]);
    let options = DescribeOptions {
        detail: DescribeDetail::Full,
        sections: Some(vec!["references".into(), "callees".into()]),
        ..DescribeOptions::default()
    };
    for (name, definition_kind, reference_kind, line) in [
        ("format", SymbolKind::Function, ReferenceKind::MacroCall, 4),
        ("only_macro", SymbolKind::Macro, ReferenceKind::Call, 6),
    ] {
        let target = handle(&db, name, definition_kind);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: name.into(),
                kind: None,
            },
        )
        .unwrap();
        assert_eq!(refs.results.len(), 1, "{name}: {refs:?}");
        let row = &refs.results[0];
        assert_eq!(
            (row.kind, row.line, row.from_symbol.as_deref()),
            (reference_kind, line, Some("run"))
        );
        assert!(row.to.is_none(), "{name}: {row:?}");
        assert_eq!(db.find_references(name, None).unwrap().len(), 1);
        assert!(impact_calls(&db, &target).is_empty(), "{name}");
        assert!(!trace(&db, "sym:src/lib.rs#run", &target), "{name}");
        let card = describe(root.path(), &db, &target, &options)
            .unwrap()
            .card
            .unwrap();
        assert_eq!(
            card.sections["references"].data["callers_total"], 0,
            "{name}"
        );
    }
    let card = describe(root.path(), &db, "sym:src/lib.rs#run", &options)
        .unwrap()
        .card
        .unwrap();
    assert_eq!(card.sections["callees"].data["total"], 0);
}

#[test]
fn imported_macro_and_function_homonyms_keep_distinct_edges_in_both_orders() {
    for calls in ["dual!(); dual();", "dual(); dual!();"] {
        let caller = format!(
            "use crate::dual;\npub fn both() {{ {calls} }}\npub fn macro_only() {{ dual!(); }}\npub fn function_only() {{ dual(); }}\n"
        );
        let (root, db) = project(&[
            (
                "src/lib.rs",
                "#[macro_export]\nmacro_rules! dual { () => { () }; }\npub fn dual() {}\nmod caller;\n",
            ),
            ("src/caller.rs", &caller),
        ]);
        let macro_target = handle(&db, "dual", SymbolKind::Macro);
        let function_target = handle(&db, "dual", SymbolKind::Function);
        assert_ne!(macro_target, function_target);
        let options = DescribeOptions {
            detail: DescribeDetail::Full,
            sections: Some(vec!["references".into(), "callees".into()]),
            ..DescribeOptions::default()
        };
        for kinds in [
            [ReferenceKind::MacroCall, ReferenceKind::Call],
            [ReferenceKind::Call, ReferenceKind::MacroCall],
        ] {
            let raw = db.find_references("dual", None).unwrap();
            let union = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "dual".into(),
                    kind: None,
                },
            )
            .unwrap();
            assert_eq!(union.results.len(), raw.len());
            assert_eq!(
                union
                    .results
                    .iter()
                    .filter(|row| matches!(
                        row.kind,
                        ReferenceKind::Call | ReferenceKind::MacroCall
                    ))
                    .count(),
                4
            );
            for row in union
                .results
                .iter()
                .filter(|row| matches!(row.kind, ReferenceKind::Call | ReferenceKind::MacroCall))
            {
                let expected = if row.kind == ReferenceKind::MacroCall {
                    &macro_target
                } else {
                    &function_target
                };
                assert_eq!(
                    row.to.as_deref(),
                    Some(expected.as_str()),
                    "{calls}: {row:?}"
                );
            }
            for kind in kinds {
                let (target, exclusive_caller, other_caller, line) =
                    if kind == ReferenceKind::MacroCall {
                        (&macro_target, "macro_only", "function_only", 3)
                    } else {
                        (&function_target, "function_only", "macro_only", 4)
                    };
                let refs = find_references(
                    &db,
                    &FindReferencesRequest {
                        symbol_name: target.clone(),
                        kind: Some(kind),
                    },
                )
                .unwrap();
                assert_eq!(refs.results.len(), 2, "{calls}: {refs:?}");
                assert!(
                    refs.results
                        .iter()
                        .all(|row| row.kind == kind && row.to.as_deref() == Some(target.as_str()))
                );
                assert_eq!(
                    refs.results.iter().map(|row| row.line).collect::<Vec<_>>(),
                    [2, line]
                );
                assert_eq!(
                    impact_calls(&db, target),
                    [
                        ("src/caller.rs".into(), kind, 2),
                        ("src/caller.rs".into(), kind, line)
                    ]
                );
                assert!(trace(&db, "sym:src/caller.rs#both", target));
                assert!(trace(
                    &db,
                    &format!("sym:src/caller.rs#{exclusive_caller}"),
                    target
                ));
                assert!(!trace(
                    &db,
                    &format!("sym:src/caller.rs#{other_caller}"),
                    target
                ));
                let incoming = describe(root.path(), &db, target, &options)
                    .unwrap()
                    .card
                    .unwrap();
                assert_eq!(incoming.sections["references"].data["callers_total"], 2);
                assert_eq!(
                    incoming.sections["references"].data["by_kind"][kind.as_str()]["product"],
                    2
                );
                let outgoing = describe(
                    root.path(),
                    &db,
                    &format!("sym:src/caller.rs#{exclusive_caller}"),
                    &options,
                )
                .unwrap()
                .card
                .unwrap();
                assert_eq!(
                    outgoing.sections["callees"].data["top"],
                    serde_json::json!([target])
                );
            }
        }
        let outgoing = describe(root.path(), &db, "sym:src/caller.rs#both", &options)
            .unwrap()
            .card
            .unwrap();
        assert_eq!(outgoing.sections["callees"].data["total"], 2);
    }
}
