use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Language,
};
use codesage_storage::Database;
use serde_json::{Value, json};

struct Server {
    child: Child,
    responses: Receiver<String>,
    id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "0")
            .env("CODESAGE_BUNDLE_LINE_NUMBERS", "1")
            .env_remove("CODESAGE_ENVELOPE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            responses,
            id: 0,
        };
        server.request("initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"overlay-test", "version":"1"}}));
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .unwrap();
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params})
        )
        .unwrap();
        loop {
            let response: Value = serde_json::from_str(
                &self
                    .responses
                    .recv_timeout(Duration::from_secs(60))
                    .expect("MCP response"),
            )
            .unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn call(&mut self, root: &Path, tool: &str, mut arguments: Value) -> Value {
        arguments["project"] = json!(root);
        if !matches!(tool, "session_start" | "session_end" | "edit_check") {
            arguments["detail"] = json!("full");
        }
        let result = self.request("tools/call", json!({"name":tool, "arguments":arguments}));
        assert_ne!(result["isError"], true, "{tool}: {result}");
        result["structuredContent"].clone()
    }
}

fn cli(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(args)
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    for (path, source) in [
        (
            "Cargo.toml",
            "[package]\nname=\"overlay_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod leaf;\npub mod gone;\nuse leaf::old_leaf;\npub fn caller() { old_leaf(); }\n",
        ),
        ("src/leaf.rs", "pub fn old_leaf() {}\n"),
        ("src/gone.rs", "pub fn removed_definition() {}\n"),
    ] {
        std::fs::write(root.join(path), source).unwrap();
    }
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Overlay Test",
            "-c",
            "user.email=overlay@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    cli(root, &["init"]);
    cli(root, &["index", "--no-semantic"]);
    let db = Database::open(&root.join(".codesage/index.db")).unwrap();
    db.upsert_feature(&FeatureRecord {
        feature_id: "feat_1111111111111111".into(),
        title: "Overlay fixture".into(),
        summary: "Library fixture".into(),
        kind: FeatureKind::Library,
        source: "cargo-lib".into(),
        confidence: FeatureConfidence::High,
        entry_path: "src/lib.rs".into(),
        entry_symbol: Some("caller".into()),
        entrypoints: Vec::new(),
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Rust,
        tags: vec![],
        trust_boundaries: vec![],
        files: vec![
            FeatureFileRef {
                path: "src/lib.rs".into(),
                role: FeatureFileRole::Entry,
                reason: None,
            },
            FeatureFileRef {
                path: "src/leaf.rs".into(),
                role: FeatureFileRole::Owned,
                reason: None,
            },
        ],
    })
    .unwrap();
    drop(db);
    dir
}

fn edited(root: &Path) {
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod leaf;\nuse leaf::new_leaf;\npub fn caller() { new_leaf(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/leaf.rs"),
        "pub fn new_leaf() {}\npub fn newly_added() {}\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("src/gone.rs")).unwrap();
}

fn assert_overlay(value: &Value) {
    assert_eq!(value["index"]["structural"], "overlay", "{value}");
    assert_eq!(value["index"]["overlay"]["checked_all"], true, "{value}");
}

#[test]
fn edited_symbols_references_graph_bundles_and_sessions_use_current_source() {
    let dir = fixture();
    let root = dir.path();
    let mut server = Server::start();
    let clean = server.call(root, "session_start", json!({"session_id":"before"}));
    assert_ne!(clean["index"]["structural"], "overlay");
    let before = std::fs::read(root.join(".codesage/index.db")).unwrap();
    edited(root);
    let symbols = server.call(root, "find_symbol", json!({"target":"new_leaf"}));
    assert_overlay(&symbols);
    assert_eq!(symbols["results"][0]["name"], "new_leaf");
    for removed in ["old_leaf", "removed_definition"] {
        let value = server.call(root, "find_symbol", json!({"target":removed}));
        assert_overlay(&value);
        assert_eq!(value["results"], json!([]));
    }
    let references = server.call(root, "find_references", json!({"target":"new_leaf"}));
    assert_overlay(&references);
    assert!(
        references["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["from_symbol"] == "caller"),
        "{references}"
    );
    let impact = server.call(root, "impact_analysis", json!({"target":"new_leaf"}));
    assert_overlay(&impact);
    assert!(
        impact["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file_path"] == "src/lib.rs"),
        "{impact}"
    );
    let trace = server.call(
        root,
        "trace_call_path",
        json!({"from":"caller", "to":"new_leaf"}),
    );
    assert_overlay(&trace);
    assert_eq!(trace["found"], true, "{trace}");
    assert_eq!(trace["length"], 1, "{trace}");
    let card = server.call(root, "describe", json!({"target":"new_leaf"}));
    assert_overlay(&card);
    assert_eq!(
        card["card"]["sections"]["identity"]["data"]["lines"],
        json!([1, 1]),
        "{card}"
    );
    assert_eq!(
        card["card"]["sections"]["references"]["data"]["callers_total"], 1,
        "{card}"
    );
    let file_card = server.call(root, "describe", json!({"target":"file:src/leaf.rs"}));
    assert_overlay(&file_card);
    assert_eq!(
        file_card["card"]["sections"]["identity"]["data"]["indexed"]["dirty"], true,
        "{file_card}"
    );
    for tool in ["assess_risk", "assess_risk_batch", "assess_risk_diff"] {
        let args = if tool == "assess_risk" {
            json!({"target":"src/leaf.rs"})
        } else {
            json!({"targets":["src/leaf.rs"]})
        };
        let risk = server.call(root, tool, args);
        assert_overlay(&risk);
        assert!(risk.to_string().contains("new_leaf"), "{risk}");
        assert!(!risk.to_string().contains("old_leaf"), "{risk}");
    }
    let bundle = server.call(
        root,
        "feature_bundle",
        json!({"target":"feat_1111111111111111", "include_callees":true}),
    );
    assert_overlay(&bundle);
    assert!(
        bundle["primary"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["content"]
                .as_str()
                .is_some_and(|source| source.contains("pub fn new_leaf()"))),
        "{bundle}"
    );
    assert_eq!(bundle["index"]["features"], "indexed");
    let feature_export = server.call(
        root,
        "export_context",
        json!({"target":"feat_1111111111111111", "include_callees":true}),
    );
    assert_overlay(&feature_export);
    assert_eq!(feature_export["index"]["features"], "indexed");
    assert_eq!(feature_export["primary"], bundle["primary"]);
    let exported = server.call(
        root,
        "export_context",
        json!({"target":"sym:src/leaf.rs#new_leaf", "include_callers":true}),
    );
    assert_overlay(&exported);
    assert!(
        exported["primary"].as_array().unwrap().iter().any(|row| {
            row["content"]
                .as_str()
                .is_some_and(|source| source.contains("pub fn new_leaf()"))
        }),
        "{exported}"
    );
    for tool in ["list_dependencies", "find_feature", "find_similar"] {
        let result = server.call(root, tool, json!({"target":"sym:src/leaf.rs#new_leaf"}));
        assert_overlay(&result);
    }
    for tool in ["recommend_tests", "review_rehearsal"] {
        let result = server.call(root, tool, json!({"targets":["sym:src/leaf.rs#new_leaf"]}));
        assert_overlay(&result);
        assert!(!result.to_string().contains("old_leaf"), "{result}");
    }
    let features = server.call(root, "list_features", json!({}));
    assert_overlay(&features);
    assert_eq!(features["index"]["features"], "indexed");
    let trace = server.call(root, "from_trace", json!({
        "trace":"stack backtrace:\n   0: overlay_fixture::leaf::new_leaf\n             at ./src/leaf.rs:1:1\n"
    }));
    assert_overlay(&trace);
    assert_eq!(trace["frames"][0]["symbol"]["name"], "new_leaf", "{trace}");
    let overview = server.call(root, "project_overview", json!({}));
    assert_overlay(&overview);
    assert_eq!(
        overview["file_count"].as_u64().unwrap() + 1,
        clean["file_count"].as_u64().unwrap()
    );
    let after = server.call(root, "session_start", json!({"session_id":"after"}));
    assert_overlay(&after);
    assert_eq!(
        after["file_count"].as_u64().unwrap() + 1,
        clean["file_count"].as_u64().unwrap()
    );
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(root.join(".codesage/sessions/after.json")).unwrap())
            .unwrap();
    assert_eq!(
        snapshot["structural_overlay_paths"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let diff = server.call(root, "session_end", json!({"session_id":"before"}));
    assert_overlay(&diff);
    assert!(
        diff["removed_files"]
            .as_array()
            .unwrap()
            .contains(&json!("src/gone.rs")),
        "{diff}"
    );
    let edit = server.call(root,"edit_check",json!({"file_path":"src/leaf.rs", "target":"old_leaf", "replacement":"pub fn old_leaf() -> u32 { 1 }"}));
    assert_ne!(
        edit["index"]["structural"], "overlay",
        "edit_check must stay pinned to HEAD: {edit}"
    );
    assert!(
        edit["unknown"].as_array().unwrap().iter().any(|message| {
            message
                .as_str()
                .is_some_and(|text| text.contains("working file differs from HEAD"))
        }),
        "{edit}"
    );
    assert_eq!(
        std::fs::read(root.join(".codesage/index.db")).unwrap(),
        before,
        "overlay must not write the main database"
    );
    let db = Database::open_read_only(&root.join(".codesage/index.db")).unwrap();
    assert_eq!(db.find_symbols("old_leaf", None).unwrap().len(), 1);
    assert!(db.find_symbols("new_leaf", None).unwrap().is_empty());
    drop(db);
    std::fs::write(root.join("src/leaf.rs"), "pub fn next_edit() {}\n").unwrap();
    let next = server.call(root, "find_symbol", json!({"target":"next_edit"}));
    assert_overlay(&next);
    assert_eq!(next["results"][0]["name"], "next_edit");
    let previous = server.call(root, "find_symbol", json!({"target":"new_leaf"}));
    assert_eq!(previous["results"], json!([]));
    cli(root, &["index", "--no-semantic"]);
    let indexed = server.call(root, "find_symbol", json!({"target":"next_edit"}));
    assert_ne!(indexed["index"]["structural"], "overlay", "{indexed}");
    assert_eq!(indexed["results"][0]["name"], "next_edit");
}

#[test]
fn over_bound_queries_disclose_indexed_fallback_without_partial_results() {
    let dir = fixture();
    let root = dir.path();
    for number in 0..51 {
        std::fs::write(
            root.join(format!("src/file_{number:02}.rs")),
            format!("pub fn before_{number:02}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", "--no-semantic"]);
    for number in 0..51 {
        std::fs::write(
            root.join(format!("src/file_{number:02}.rs")),
            format!("pub fn after_{number:02}() {{}}\n"),
        )
        .unwrap();
    }
    let before = std::fs::read(root.join(".codesage/index.db")).unwrap();
    let mut server = Server::start();
    for (target, count) in [("after_00", 0), ("before_00", 1)] {
        let value = server.call(root, "find_symbol", json!({"target":target}));
        assert_eq!(value["index"]["structural"], "dirty", "{value}");
        assert_eq!(value["index"]["dirty_paths"].as_array().unwrap().len(), 51);
        assert!(
            value["index"]["overlay"]["reason"]
                .as_str()
                .unwrap()
                .contains("50 changed files")
        );
        assert_eq!(value["results"].as_array().unwrap().len(), count);
    }
    assert_eq!(
        std::fs::read(root.join(".codesage/index.db")).unwrap(),
        before
    );
}

#[test]
fn cached_overview_directory_and_session_rankings_use_changed_risk() {
    let dir = fixture();
    let root = dir.path();
    cli(root, &["git-index", "--full"]);
    let mut server = Server::start();
    let clean = server.call(root, "project_overview", json!({}));
    let old_score = clean["top_risk_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["file"] == "src/leaf.rs")
        .unwrap()["score"]
        .as_f64()
        .unwrap();
    assert!(
        clean["top_risk_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == "src/gone.rs")
    );
    server.call(
        root,
        "session_start",
        json!({"session_id":"ranking-before"}),
    );
    let clean_directory = server.call(root, "describe", json!({"target":"dir:src"}));
    assert_eq!(
        clean_directory["card"]["sections"]["risk"]["data"]["scored"],
        3
    );

    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod leaf;\nuse leaf::old_leaf;\npub fn caller() { old_leaf(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/leaf.rs"),
        "use std::fs;\nuse std::process;\nuse std::env;\npub fn old_leaf() {}\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("src/gone.rs")).unwrap();
    let risk = server.call(root, "assess_risk", json!({"target":"src/leaf.rs"}));
    assert_overlay(&risk);
    let current_score = risk["score"].as_f64().unwrap();
    assert!(
        current_score > old_score + 0.05,
        "before={old_score}, current={risk}"
    );

    let current = server.call(root, "project_overview", json!({}));
    assert_overlay(&current);
    let ranking = current["top_risk_files"].as_array().unwrap();
    assert_eq!(ranking.len(), 2, "{current}");
    assert!(ranking.iter().all(|row| row["file"] != "src/gone.rs"));
    assert_eq!(
        ranking
            .iter()
            .find(|row| row["file"] == "src/leaf.rs")
            .unwrap()["score"],
        risk["score"]
    );

    let directory = server.call(root, "describe", json!({"target":"dir:src"}));
    assert_overlay(&directory);
    let directory_risk = &directory["card"]["sections"]["risk"]["data"];
    assert_eq!(directory_risk["scored"], 2, "{directory}");
    let expected_mean = ranking
        .iter()
        .map(|row| row["score"].as_f64().unwrap())
        .sum::<f64>()
        / 2.0;
    assert!(
        (directory_risk["mean"].as_f64().unwrap() - expected_mean).abs() < 1e-9,
        "{directory}"
    );

    let after = server.call(root, "session_start", json!({"session_id":"ranking-after"}));
    assert_overlay(&after);
    let snapshot: Value = serde_json::from_slice(
        &std::fs::read(root.join(".codesage/sessions/ranking-after.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot["top_risk_files"], current["top_risk_files"]);
    let diff = server.call(root, "session_end", json!({"session_id":"ranking-before"}));
    assert_overlay(&diff);
    assert!(
        diff["risk_regressions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == "src/leaf.rs" && row["after"] == risk["score"]),
        "{diff}"
    );
}

#[test]
fn edited_test_calls_change_recommendations_and_rehearsal_evidence() {
    let dir = fixture();
    let root = dir.path();
    std::fs::create_dir(root.join("tests")).unwrap();
    std::fs::write(
        root.join("src/before.php"),
        "<?php function before_target() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/after.php"),
        "<?php function after_target() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/FlowTest.php"),
        "<?php function test_flow() { before_target(); }\n",
    )
    .unwrap();
    cli(root, &["index", "--no-semantic"]);
    let mut server = Server::start();
    let before = server.call(
        root,
        "recommend_tests",
        json!({"targets":["src/before.php"]}),
    );
    assert_eq!(before["reachable"].as_array().unwrap().len(), 1, "{before}");
    assert_eq!(before["reachable"][0]["path"], "tests/FlowTest.php");
    assert_eq!(before["reachable"][0]["via"], "src/before.php");
    let before_other = server.call(
        root,
        "recommend_tests",
        json!({"targets":["src/after.php"]}),
    );
    assert_eq!(before_other["reachable"], json!([]));

    std::fs::write(
        root.join("tests/FlowTest.php"),
        "<?php function test_flow() { after_target(); }\n",
    )
    .unwrap();
    let removed = server.call(
        root,
        "recommend_tests",
        json!({"targets":["src/before.php"]}),
    );
    assert_overlay(&removed);
    assert_eq!(removed["reachable"], json!([]), "{removed}");
    let added = server.call(
        root,
        "recommend_tests",
        json!({"targets":["src/after.php"]}),
    );
    assert_overlay(&added);
    assert_eq!(added["reachable"].as_array().unwrap().len(), 1, "{added}");
    assert_eq!(added["reachable"][0]["path"], "tests/FlowTest.php");
    assert_eq!(added["reachable"][0]["via"], "src/after.php");
    assert_eq!(added["reachable"][0]["distance"], 1);
    let note = "Reachable tests (call/import edges): tests/FlowTest.php";
    let removed_rehearsal = server.call(
        root,
        "review_rehearsal",
        json!({"targets":["src/before.php"]}),
    );
    assert_overlay(&removed_rehearsal);
    assert!(
        !removed_rehearsal["summary_notes"]
            .as_array()
            .unwrap()
            .contains(&json!(note)),
        "{removed_rehearsal}"
    );
    let added_rehearsal = server.call(
        root,
        "review_rehearsal",
        json!({"targets":["src/after.php"]}),
    );
    assert_overlay(&added_rehearsal);
    assert!(
        added_rehearsal["summary_notes"]
            .as_array()
            .unwrap()
            .contains(&json!(note)),
        "{added_rehearsal}"
    );
}

#[test]
fn overlay_bundles_keep_late_target_bodies_and_number_raw_path_comments() {
    let dir = fixture();
    let root = dir.path();
    std::fs::create_dir(root.join("app")).unwrap();
    std::fs::write(root.join("app/x.py"), "def before():\n    pass\n").unwrap();
    cli(root, &["index", "--no-semantic"]);
    let mut server = Server::start();
    for prefix in [
        "# Padding that must not displace the requested declaration.\n".repeat(800),
        "# pad\n".repeat(63),
    ] {
        let line = prefix.lines().count() + 1;
        std::fs::write(
            root.join("app/x.py"),
            format!("{prefix}def late_function():\n    return 'current late body'\n"),
        )
        .unwrap();
        for target in [
            "sym:app/x.py#late_function".to_string(),
            format!("app/x.py:{}", line + 1),
        ] {
            let bundle = server.call(root, "export_context", json!({"target":target}));
            assert_overlay(&bundle);
            assert!(
                bundle["primary"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["content"]
                        .as_str()
                        .is_some_and(|source| source.contains("def late_function():")
                            && source.contains("return 'current late body'"))),
                "{bundle}"
            );
        }
    }
    std::fs::write(
        root.join("app/x.py"),
        "# app/x.py\ndef changed():\n    pass\n",
    )
    .unwrap();
    let bundle = server.call(root, "export_context", json!({"target":"file:app/x.py"}));
    assert_overlay(&bundle);
    let source = bundle["primary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["content"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("");
    assert!(source.contains("   1 | # app/x.py\n"), "{bundle}");
    assert!(source.contains("   2 | def changed():\n"), "{bundle}");
    assert!(source.contains("   3 |     pass\n"), "{bundle}");
}

#[test]
fn deep_java_falls_back_without_losing_the_mcp_process() {
    let dir = fixture();
    let root = dir.path();
    std::fs::write(root.join("Subject.java"), "class Subject {}\n").unwrap();
    cli(root, &["index", "--no-semantic"]);
    let before = std::fs::read(root.join(".codesage/index.db")).unwrap();
    let mut server = Server::start();
    std::fs::write(root.join("Subject.java"), "class Current {}\n").unwrap();
    let current = server.call(root, "find_symbol", json!({"target":"Current"}));
    assert_overlay(&current);
    assert_eq!(current["results"][0]["name"], "Current");

    std::fs::write(
        root.join("Subject.java"),
        format!("{}{}", "class A { ".repeat(200), "}".repeat(200)),
    )
    .unwrap();
    let bounded = server.call(root, "find_symbol", json!({"target":"Subject"}));
    assert_eq!(bounded["index"]["structural"], "dirty", "{bounded}");
    assert_eq!(bounded["index"]["dirty_paths"], json!(["Subject.java"]));
    assert!(
        bounded["index"]["overlay"]["reason"]
            .as_str()
            .unwrap()
            .contains("Java syntax nesting exceeds 128 levels"),
        "{bounded}"
    );
    assert_eq!(bounded["results"][0]["name"], "Subject");
    std::fs::write(root.join("Subject.java"), "class After {}\n").unwrap();
    let after = server.call(root, "find_symbol", json!({"target":"After"}));
    assert_overlay(&after);
    assert_eq!(after["results"][0]["name"], "After");
    assert_eq!(
        std::fs::read(root.join(".codesage/index.db")).unwrap(),
        before
    );
}
