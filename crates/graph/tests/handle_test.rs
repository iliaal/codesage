//! Handles round-trip across the structural tools: the `sym:` handle
//! `find_symbol` emits is the one `find_references` names in `to`, the one
//! `impact_analysis` reasons about, and the one each `trace_call_path` step
//! carries. Overloads in one file get `@line` handles.

use codesage_graph::{
    find_references, find_symbol, full_index, impact_analysis_report, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, FindReferencesRequest, FindSymbolRequest, Handle, ImpactOptions,
    ImpactRequest, ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod helper;\nuse crate::helper::inner;\npub fn outer() -> u32 { inner() }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/helper.rs"), "pub fn inner() -> u32 { 7 }\n").unwrap();
    std::fs::write(
        root.join("src/over.cpp"),
        "int helper() { return 1; }\nint run(int x) { return helper() + x; }\nint run(double x) { return helper(); }\nint stop() { return 0; }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

fn symbols(db: &Database, name: &str) -> Vec<codesage_protocol::Symbol> {
    find_symbol(
        db,
        &FindSymbolRequest {
            name: name.to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results
}

#[test]
fn handles_round_trip_through_find_symbol_references_impact_and_trace() {
    let (_dir, db) = setup();

    let inner = symbols(&db, "inner");
    assert_eq!(inner.len(), 1, "{inner:?}");
    let inner_handle = inner[0].handle();
    assert_eq!(inner_handle.to_string(), "sym:src/helper.rs#inner");
    assert_eq!(
        Handle::parse(&inner_handle.to_string()),
        Some(inner_handle.clone())
    );
    let wire: serde_json::Value = serde_json::to_value(&inner[0]).unwrap();
    assert_eq!(wire["handle"], "sym:src/helper.rs#inner");

    let outer = symbols(&db, "outer");
    assert_eq!(outer.len(), 1, "{outer:?}");
    let outer_handle = outer[0].handle().to_string();
    assert_eq!(outer_handle, "sym:src/lib.rs#outer");

    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "inner".to_string(),
            kind: None,
        },
    )
    .unwrap();
    assert_eq!(refs.definition_count, 1);
    let call = refs
        .results
        .iter()
        .find(|r| r.from_symbol.is_some())
        .unwrap_or_else(|| panic!("a call from outer: {:?}", refs.results));
    let wire: serde_json::Value = serde_json::to_value(call).unwrap();
    assert_eq!(wire["from"], outer_handle, "{wire}");
    assert_eq!(wire["to"], inner_handle.to_string(), "{wire}");
    let file_scope = refs
        .results
        .iter()
        .find(|r| r.from_symbol.is_none())
        .unwrap_or_else(|| panic!("a file-scope import: {:?}", refs.results));
    let wire: serde_json::Value = serde_json::to_value(file_scope).unwrap();
    assert!(wire.get("from").is_none(), "omitted at file scope: {wire}");
    assert!(
        refs.to_resolution.is_none(),
        "a small fixture never caps: {:?}",
        refs.to_resolution
    );

    // The handle names the impact target by its qualified part.
    let Handle::Symbol { qualified, .. } = &inner_handle else {
        panic!("symbol handle expected");
    };
    let impact = impact_analysis_report(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: qualified.clone(),
            },
            depth: 2,
            source_only: false,
        },
        &ImpactOptions {
            include_siblings: true,
            ..ImpactOptions::default()
        },
    )
    .unwrap();
    for sibling in &impact.sibling_symbols {
        assert!(
            Handle::parse(&sibling.handle).is_some(),
            "sibling handle: {}",
            sibling.handle
        );
    }
    assert!(
        impact.results.iter().any(|e| e.file_path == "src/lib.rs"),
        "{:?}",
        impact.results
    );

    let trace = trace_call_path(
        &db,
        &CallPathRequest {
            from: "outer".to_string(),
            to: "inner".to_string(),
            max_depth: 6,
        },
    )
    .unwrap();
    assert!(trace.found, "{trace:?}");
    let handles: Vec<&str> = trace.steps.iter().map(|s| s.handle.as_str()).collect();
    assert_eq!(handles, [outer_handle.as_str(), "sym:src/helper.rs#inner"]);
    for step in &trace.steps {
        assert!(Handle::parse(&step.handle).is_some(), "{}", step.handle);
    }
}

#[test]
fn overloads_in_one_file_carry_line_handles() {
    let (_dir, db) = setup();
    let runs = symbols(&db, "run");
    assert_eq!(runs.len(), 2, "{runs:?}");
    let mut handles: Vec<String> = runs.iter().map(|s| s.handle().to_string()).collect();
    handles.sort();
    let expected: Vec<String> = {
        let mut lines: Vec<u32> = runs.iter().map(|s| s.line_start).collect();
        lines.sort_unstable();
        lines
            .iter()
            .map(|l| format!("sym:src/over.cpp#run@{l}"))
            .collect()
    };
    assert_eq!(handles, expected);
    assert_ne!(runs[0].line_start, runs[1].line_start);
    for h in &handles {
        assert!(
            matches!(Handle::parse(h), Some(Handle::Symbol { line: Some(_), .. })),
            "{h}"
        );
    }

    let stop = symbols(&db, "stop");
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].handle().to_string(), "sym:src/over.cpp#stop");

    let by_file = db.symbols_for_file("src/over.cpp").unwrap();
    let flagged: Vec<(&str, bool)> = by_file
        .iter()
        .map(|s| (s.name.as_str(), s.overloaded))
        .collect();
    assert_eq!(
        flagged,
        [
            ("helper", false),
            ("run", true),
            ("run", true),
            ("stop", false)
        ]
    );

    let all_refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "helper".to_string(),
            kind: None,
        },
    )
    .unwrap();
    let module_imports: Vec<_> = all_refs
        .results
        .iter()
        .filter(|r| r.kind == ReferenceKind::Import)
        .collect();
    assert_eq!(module_imports.len(), 1);
    assert_eq!(module_imports[0].from_file, "src/lib.rs");
    assert_eq!(module_imports[0].to_name, "./helper");
    assert_eq!(module_imports[0].to, None);
    let calls: Vec<_> = all_refs
        .results
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .collect();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert_eq!(call.to.as_deref(), Some("sym:src/over.cpp#helper"));
    }

    // Both overloads call `helper`; each caller row names its own overload,
    // with the same `@line` handle `find_symbol` emitted for it.
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "helper".to_string(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    let mut froms: Vec<String> = refs
        .results
        .iter()
        .filter(|r| r.from_symbol.as_deref() == Some("run"))
        .map(|r| {
            let wire: serde_json::Value = serde_json::to_value(r).unwrap();
            wire["from"].as_str().unwrap().to_string()
        })
        .collect();
    froms.sort();
    froms.dedup();
    assert_eq!(froms, handles, "{:?}", refs.results);
    for r in &refs.results {
        assert_eq!(r.to.as_deref(), Some("sym:src/over.cpp#helper"), "{r:?}");
    }
}
