use codesage_graph::{
    find_references, full_index, impact_analysis, incremental_index, trace_call_path,
};
use codesage_protocol::{
    CallPathReport, CallPathRequest, DEFAULT_EMBEDDING_DIM, FindReferencesRequest, ImpactRequest,
    ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == "panel" => receiver_scope_evaluation(),
        [mode, root] if mode == "prometheus" => prometheus_real_workload(root.into()),
        _ => anyhow::bail!("usage: call_edge_precision panel | prometheus PATH"),
    }
    Ok(())
}

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

fn direct_trace(db: &Database, from: &str, to: &str) -> CallPathReport {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 1,
        },
    )
    .unwrap()
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

fn prometheus_real_workload(root: std::path::PathBuf) {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../bench/call-edge-precision/prometheus-oracle.json"
    ))
    .unwrap();
    let head = codesage_graph::git_command(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim(),
        oracle["head"].as_str().unwrap()
    );
    for (path, hash) in oracle["hashes_sha256"].as_object().unwrap() {
        assert_eq!(
            codesage_parser::discover::content_hash(&std::fs::read(root.join(path)).unwrap()),
            hash.as_str().unwrap(),
            "source changed: {path}"
        );
    }
    let db = Database::open_in_memory().unwrap();
    let start = std::time::Instant::now();
    let stats = full_index(&root, &db, &[], false).unwrap();
    println!("index_ms={} stats={stats:?}", start.elapsed().as_millis());
    assert!(
        stats.files_indexed > 500,
        "full workload required: {stats:?}"
    );
    let cases = [
        (
            "web/ui/mantine-ui/src/lib/formatFloatValue.ts",
            "model/textparse/promparse.go",
            "parseFloat",
            false,
        ),
        (
            "web/ui/mantine-ui/src/lib/formatSeries.ts",
            "discovery/ionos/server.go",
            "join",
            false,
        ),
        (
            "web/ui/mantine-ui/src/lib/formatSeries.ts",
            "web/ui/mantine-ui/src/lib/escapeString.ts",
            "escapeString",
            true,
        ),
    ];
    let mut failures = Vec::new();
    for pass in 0..11 {
        let start = std::time::Instant::now();
        for (caller, definition, name, expected) in cases {
            let target = format!("sym:{definition}#{name}");
            let found = impact(&db, &target, caller);
            println!(
                "pass={pass} caller={caller} target={target} impact={found} expected={expected}"
            );
            if found != expected {
                failures.push((caller, target));
            }
        }
        println!(
            "query_pass={pass} elapsed_us={}",
            start.elapsed().as_micros()
        );
    }
    for edge in oracle["edges"].as_array().unwrap() {
        let caller = edge["from_file"].as_str().unwrap();
        let candidate = edge["candidate"].as_str().unwrap();
        let target_name = candidate
            .split('#')
            .nth(1)
            .unwrap()
            .split('@')
            .next()
            .unwrap();
        let name = target_name.rsplit('.').next().unwrap();
        let candidates = db.find_symbols(name, None).unwrap();
        let definition = candidate
            .strip_prefix("sym:")
            .unwrap()
            .split('#')
            .next()
            .unwrap();
        let target = candidates
            .iter()
            .find(|symbol| symbol.file_path == definition)
            .unwrap()
            .handle()
            .to_string();
        let from_name = edge["from_symbol"]
            .as_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap();
        let from = db
            .find_symbols(from_name, None)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.file_path == caller)
            .unwrap()
            .handle()
            .to_string();
        let rows = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: name.into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        let calls: Vec<_> = rows
            .results
            .iter()
            .filter(|row| {
                row.from_file == caller
                    && (edge["line"].as_u64() == Some(row.line as u64)
                        || edge["lines"].as_array().is_some_and(|lines| {
                            lines
                                .iter()
                                .any(|line| line.as_u64() == Some(row.line as u64))
                        }))
            })
            .collect();
        assert!(!calls.is_empty(), "missing oracle call: {edge}");
        let to = calls.iter().any(|row| row.to.as_deref() == Some(&target));
        let direct = direct_trace(&db, &from, &target);
        if direct.found {
            assert_eq!(direct.length, 1, "{direct:?}");
            assert_eq!(direct.steps[0].handle, from);
            assert_eq!(direct.steps[1].handle, target);
            assert!(
                calls
                    .iter()
                    .any(|row| Some(row.line) == direct.steps[1].call_line),
                "direct path must use an oracle callsite: {direct:?}, {calls:?}"
            );
        }
        let reasons = (caller != definition).then(|| {
            impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: target.clone(),
                    },
                    depth: 1,
                    source_only: false,
                },
            )
            .unwrap()
            .into_iter()
            .filter(|row| row.file_path == caller)
            .flat_map(|row| row.reasons)
            .filter(|reason| {
                reason.kind == ReferenceKind::Call
                    && calls
                        .iter()
                        .any(|row| row.line == reason.line && row.to_name == reason.via_symbol)
            })
            .collect::<Vec<_>>()
        });
        let indirect = edge["legitimate_indirect_via"].as_str().map(|via| {
            let first = direct_trace(&db, &from, via);
            let second = direct_trace(&db, via, &target);
            assert!(
                first.found && second.found,
                "known indirect path: {first:?}, {second:?}"
            );
            serde_json::json!({"via": via, "legs": [first, second]})
        });
        println!(
            "real_edge={}",
            serde_json::json!({"caller": caller, "target": target, "oracle": edge["label"], "oracle_scope": "direct call at labeled source lines", "calls": calls, "to": to, "direct_trace": direct, "direct_impact_reasons": reasons, "legitimate_indirect_path": indirect})
        );
        if edge["label"] == "true" {
            assert!(direct.found, "lost known true direct edge: {edge}");
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}

fn receiver_scope_evaluation() {
    let cases: serde_json::Value = serde_json::from_str(include_str!(
        "../../../bench/call-edge-precision/cases.json"
    ))
    .unwrap();
    for case in cases.as_array().unwrap() {
        let files: Vec<_> = case["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| (pair[0].as_str().unwrap(), pair[1].as_str().unwrap()))
            .collect();
        let (root, db) = project(&files);
        let name = case["target"].as_str().unwrap();
        let definition = case["definition"].as_str().unwrap();
        let symbols = db.find_symbols(name, None).unwrap();
        let target = symbols
            .iter()
            .find(|symbol| symbol.file_path == definition)
            .unwrap()
            .handle()
            .to_string();
        let entry_name = case["entry"].as_str().unwrap();
        let entry = db
            .find_symbols(entry_name, None)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.file_path == files[0].0)
            .unwrap()
            .handle()
            .to_string();
        let snapshot = |db: &Database| {
            let rows = find_references(
                db,
                &FindReferencesRequest {
                    symbol_name: name.into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            let raw: Vec<_> = rows
                .results
                .iter()
                .filter(|row| row.from_file == files[0].0)
                .collect();
            assert!(
                !raw.is_empty(),
                "fixture {} must exercise a raw call",
                case["id"]
            );
            if case["id"] == "python_recursion" {
                assert!(
                    raw.iter()
                        .any(|row| row.line == 2 && row.to.as_deref() == Some(&target)),
                    "{raw:?}"
                );
            }
            let direct = direct_trace(db, &entry, &target);
            serde_json::json!({"id": case["id"], "oracle": case["oracle"],
                "to": raw.iter().any(|row| row.to.as_deref() == Some(&target)),
                "direct_trace": direct,
                "impact": (definition != files[0].0).then(|| impact(db, &target, files[0].0)),
                "raw_calls": raw.len()})
        };
        let cold = snapshot(&db);
        if case["oracle"] == "true" {
            assert_eq!(cold["to"], true, "lost known true target: {}", case["id"]);
            assert_eq!(
                cold["direct_trace"]["found"], true,
                "lost known true direct path: {}",
                case["id"]
            );
        }
        assert_eq!(cold, snapshot(&db), "repeated {}", case["id"]);
        drop(db);
        let db = Database::open(&root.path().join("index.db")).unwrap();
        assert_eq!(cold, snapshot(&db), "reopened {}", case["id"]);
        std::fs::write(root.path().join(files[0].0), format!("{}\n", files[0].1)).unwrap();
        incremental_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(cold, snapshot(&db), "edited {}", case["id"]);
        let rebuilt = Database::open_in_memory().unwrap();
        full_index(root.path(), &rebuilt, &[], false).unwrap();
        assert_eq!(cold, snapshot(&rebuilt), "rebuilt {}", case["id"]);
        println!("{cold}");
    }
}
