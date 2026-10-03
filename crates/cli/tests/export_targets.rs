use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use codesage_graph::full_index;
use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Language,
};
use codesage_storage::Database;
use serde_json::{Value, json};

const MODEL: &str = "export-target-model-unavailable";

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"export_fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join(".codesage/config.toml"),
        format!("[embedding]\nmodel = \"{MODEL}\"\ndevice = \"cpu\"\n"),
    )
    .unwrap();
    let sources = [
        (
            "src/lib.rs",
            "pub mod anchor;\npub mod helper;\npub mod alternate;\npub fn invoke() { anchor::unique_anchor(); }\n",
        ),
        (
            "src/anchor.rs",
            "use crate::helper::leaf;\npub fn unique_anchor() { leaf(); }\npub fn search() {}\npub struct Worker;\nimpl Worker { pub fn run() {} }\n",
        ),
        ("src/helper.rs", "pub fn leaf() {}\n"),
        ("src/alternate.rs", "pub fn search() {}\n"),
    ];
    for (path, source) in sources {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    let db = Database::open_for_model(&root.path().join(".codesage/index.db"), MODEL, 4).unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    for (path, source) in sources {
        db.insert_chunks(
            path,
            "rust",
            &[(
                source,
                1,
                source.lines().count() as u32,
                &[1.0, 0.0, 0.0, 0.0],
            )],
        )
        .unwrap();
    }
    for (id, kind, route, command) in [
        (
            "feat_1111111111111111",
            FeatureKind::Route,
            Some("GET /anchor"),
            None,
        ),
        (
            "feat_2222222222222222",
            FeatureKind::CliCommand,
            None,
            Some("anchor"),
        ),
        (
            "feat_3333333333333333",
            FeatureKind::Route,
            Some("GET /duplicate"),
            None,
        ),
        (
            "feat_4444444444444444",
            FeatureKind::Route,
            Some("GET /duplicate"),
            None,
        ),
        (
            "feat_5555555555555555",
            FeatureKind::CliCommand,
            None,
            Some("duplicate"),
        ),
        (
            "feat_6666666666666666",
            FeatureKind::CliCommand,
            None,
            Some("duplicate"),
        ),
    ] {
        db.upsert_feature(&FeatureRecord {
            feature_id: id.to_string(),
            title: "Anchor".to_string(),
            summary: String::new(),
            kind,
            source: "fixture".to_string(),
            confidence: FeatureConfidence::High,
            entry_path: "src/anchor.rs".to_string(),
            entry_symbol: Some("unique_anchor".to_string()),
            entry_route: route.map(str::to_string),
            entry_command: command.map(str::to_string),
            test_command: None,
            language: Language::Rust,
            tags: Vec::new(),
            trust_boundaries: Vec::new(),
            files: vec![FeatureFileRef {
                path: "src/anchor.rs".to_string(),
                role: FeatureFileRole::Entry,
                reason: None,
            }],
        })
        .unwrap();
    }
    root
}

fn export(root: &Path, target: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args([
            "export",
            target,
            "--format",
            "json",
            "--callers",
            "--callees",
        ])
        .args(extra)
        .env("CODESAGE_WATCH", "0")
        .current_dir(root)
        .output()
        .unwrap()
}

fn cli_bundle(root: &Path, target: &str) -> Value {
    let output = export(root, target, &[]);
    assert!(
        output.status.success(),
        "{target}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Server {
    child: Child,
    lines: Receiver<String>,
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
            .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            lines,
            id: 0,
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "export-target-test", "version": "1"}
            }),
        );
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            })
        )
        .unwrap();
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc": "2.0", "id": self.id, "method": method, "params": params
            })
        )
        .unwrap();
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(30)).unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn export(&mut self, root: &Path, target: &str) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": "export_context", "arguments": {
                    "project": root, "target": target,
                    "include_callers": true, "include_callees": true
                }
            }),
        )
    }
}

fn failure(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|block| block["error"].is_object())
        .unwrap_or_else(|| panic!("no structured error: {result}"))
}

#[test]
fn cli_and_mcp_resolve_symbols_and_expand_callers_and_callees_without_a_model() {
    let root = project();
    let mut server = Server::start();
    for target in [
        "unique_anchor",
        "src/anchor.rs:2",
        "sym:src/anchor.rs#unique_anchor",
        "Worker::run",
    ] {
        let cli = cli_bundle(root.path(), target);
        let result = server.export(root.path(), target);
        assert_ne!(result["isError"], true, "{target}: {result}");
        let mcp = &result["structuredContent"];
        for key in [
            "found",
            "primary",
            "related",
            "symbol_definitions",
            "target_description",
        ] {
            assert_eq!(cli[key], mcp[key], "{target} {key}: {result}");
        }
        assert_eq!(cli["found"], true);
        assert_eq!(cli["symbol_definitions"].as_array().unwrap().len(), 1);
        assert_eq!(cli["primary"][0]["file_path"], "src/anchor.rs");
        if target != "Worker::run" {
            let paths: Vec<_> = cli["related"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file_path"].as_str().unwrap())
                .collect();
            assert!(paths.contains(&"src/helper.rs"), "callee: {cli}");
            assert!(paths.contains(&"src/lib.rs"), "caller: {cli}");
        }
    }
}

#[test]
fn cli_and_mcp_export_file_directory_chunk_and_feature_targets_without_a_model() {
    let root = project();
    let mut server = Server::start();
    for target in [
        "src/anchor.rs",
        "./src/anchor.rs",
        "file:src/anchor.rs",
        "dir:src",
        "chunk:src/anchor.rs:2-2",
        "feat_1111111111111111",
        "route:GET /anchor",
        "cmd:anchor",
    ] {
        let cli = cli_bundle(root.path(), target);
        let result = server.export(root.path(), target);
        assert_ne!(result["isError"], true, "{target}: {result}");
        let mcp = &result["structuredContent"];
        assert_eq!(cli["primary"], mcp["primary"], "{target}: {result}");
        assert_eq!(
            cli["symbol_definitions"], mcp["symbol_definitions"],
            "{target}: {result}"
        );
        assert_eq!(cli["found"], true);
        assert!(
            !cli["primary"].as_array().unwrap().is_empty(),
            "{target}: {cli}"
        );
        assert!(
            !cli["symbol_definitions"].as_array().unwrap().is_empty(),
            "{target}: {cli}"
        );
        if target.starts_with("chunk:") {
            assert_eq!(cli["symbol_definitions"].as_array().unwrap().len(), 1);
            assert_eq!(cli["symbol_definitions"][0]["name"], "unique_anchor");
        }
    }
}

#[test]
fn directory_exports_keep_literal_case_and_component_boundaries_in_both_adapters() {
    let root = project();
    let sources = [
        ("module/a.rs", "pub fn inside_lower() {}\n"),
        ("module/nested/b.rs", "pub fn inside_nested() {}\n"),
        ("MODULE/c.rs", "pub fn outside_upper() {}\n"),
        ("module_extra/d.rs", "pub fn outside_component() {}\n"),
    ];
    let db =
        Database::open_for_existing_model(&root.path().join(".codesage/index.db"), MODEL).unwrap();
    for (path, source) in sources {
        std::fs::create_dir_all(root.path().join(path).parent().unwrap()).unwrap();
        std::fs::write(root.path().join(path), source).unwrap();
    }
    full_index(root.path(), &db, &[], false).unwrap();
    for (path, source) in sources {
        db.insert_chunks(path, "rust", &[(source, 1, 1, &[1.0, 0.0, 0.0, 0.0])])
            .unwrap();
    }
    drop(db);
    let mut server = Server::start();
    for (target, expected) in [
        ("dir:module", vec!["module/a.rs", "module/nested/b.rs"]),
        ("dir:module/", vec!["module/a.rs", "module/nested/b.rs"]),
        ("dir:MODULE", vec!["MODULE/c.rs"]),
    ] {
        let cli = cli_bundle(root.path(), target);
        let result = server.export(root.path(), target);
        assert_ne!(result["isError"], true, "{target}: {result}");
        let mcp = &result["structuredContent"];
        for key in ["primary", "symbol_definitions"] {
            assert_eq!(cli[key], mcp[key], "{target}: {result}");
            let paths: Vec<_> = cli[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file_path"].as_str().unwrap())
                .collect();
            assert_eq!(paths, expected, "{target} {key}: {cli}");
        }
    }
    let output = export(root.path(), "dir:module", &["--limit", "1"]);
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = server.request(
        "tools/call",
        json!({
            "name": "export_context", "arguments": {
                "project": root.path(), "target": "dir:module", "limit": 1
            }
        }),
    );
    assert_ne!(result["isError"], true, "{result}");
    for key in ["primary", "symbol_definitions"] {
        assert_eq!(cli[key], result["structuredContent"][key]);
        assert_eq!(cli[key].as_array().unwrap().len(), 1);
        assert_eq!(cli[key][0]["file_path"], "module/a.rs");
    }
}

#[test]
fn cli_and_mcp_refuse_ambiguity_and_entity_misses_with_handles() {
    let root = project();
    let mut server = Server::start();
    for (target, code) in [
        ("search", "E_AMBIGUOUS"),
        ("route:GET /duplicate", "E_AMBIGUOUS"),
        ("cmd:duplicate", "E_AMBIGUOUS"),
        ("Search", "E_NOT_FOUND"),
        ("sym:src/anchor.rs#gone", "E_NOT_FOUND"),
        ("file:gone.rs", "E_NOT_FOUND"),
        ("dir:gone", "E_NOT_FOUND"),
        ("chunk:gone.rs:1-2", "E_NOT_FOUND"),
        ("feat_9999999999999999", "E_NOT_FOUND"),
        ("route:GET /gone", "E_NOT_FOUND"),
        ("cmd:gone", "E_NOT_FOUND"),
    ] {
        let output = export(root.path(), target, &[]);
        assert!(!output.status.success(), "{target} unexpectedly succeeded");
        let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
        let result = server.export(root.path(), target);
        let block = failure(&result);
        assert_eq!(cli["error"]["code"], code, "{target}: {cli}");
        assert_eq!(block["error"]["code"], code, "{target}: {block}");
        assert_eq!(
            cli["error"]["candidates"].as_array().unwrap(),
            &block["error"]["candidates"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            "{target}: {block}"
        );
        if code == "E_AMBIGUOUS" {
            assert_eq!(cli["error"]["candidates"].as_array().unwrap().len(), 2);
            let retry = block["error"]["remedy"]["arguments"]["target"]
                .as_str()
                .unwrap();
            let result = server.export(root.path(), retry);
            assert_ne!(result["isError"], true, "{result}");
        }
    }
}

#[test]
fn the_legacy_symbol_hint_preserves_its_unresolved_name_miss() {
    let root = project();
    let target = "unindexed_symbol";
    let output = export(root.path(), target, &["--symbol"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli["found"], false);
    let mut server = Server::start();
    let result = server.request(
        "tools/call",
        json!({
            "name": "export_context", "arguments": {
                "project": root.path(), "target": target, "is_symbol": true
            }
        }),
    );
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(result["structuredContent"]["found"], false);
}

#[test]
fn structural_exports_keep_definitions_when_no_semantic_chunks_exist() {
    let root = project();
    let db =
        Database::open_for_existing_model(&root.path().join(".codesage/index.db"), MODEL).unwrap();
    for path in [
        "src/lib.rs",
        "src/anchor.rs",
        "src/helper.rs",
        "src/alternate.rs",
    ] {
        db.delete_chunks_for_file(path).unwrap();
    }
    drop(db);
    let mut server = Server::start();
    for target in [
        "unique_anchor",
        "file:src/anchor.rs",
        "dir:src",
        "chunk:src/anchor.rs:2-2",
        "route:GET /anchor",
        "cmd:anchor",
    ] {
        let cli = cli_bundle(root.path(), target);
        let result = server.export(root.path(), target);
        assert_ne!(result["isError"], true, "{target}: {result}");
        let mcp = &result["structuredContent"];
        assert_eq!(cli["found"], true);
        assert_eq!(mcp["found"], true);
        assert!(cli["primary"].as_array().unwrap().is_empty());
        assert!(mcp["primary"].as_array().unwrap().is_empty());
        assert!(
            !cli["symbol_definitions"].as_array().unwrap().is_empty(),
            "{target}: {cli}"
        );
        assert_eq!(
            cli["symbol_definitions"], mcp["symbol_definitions"],
            "{target}: {mcp}"
        );
        assert!(
            cli["symbol_definitions"][0]["handle"]
                .as_str()
                .unwrap()
                .starts_with("sym:")
        );
    }
}

#[test]
fn a_real_mapped_go_library_exports_entry_and_owned_definitions_without_chunks() {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("go.mod", "module example.com/anchor\n\ngo 1.22\n"),
        (
            "anchor.go",
            "package anchor\n\nfunc PublicAnchor() int { return OwnedAnchor() }\nfunc SecondAnchor() int { return 2 }\n",
        ),
        (
            "helper.go",
            "package anchor\n\nfunc OwnedAnchor() int { return 7 }\n",
        ),
        (
            "anchor_test.go",
            "package anchor\n\nfunc TestExcluded() {}\n",
        ),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    for args in [vec!["init"], vec!["index", "--no-semantic"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listed = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["features-list", "--json"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let feature = listed["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|feature| feature["entry_path"] == "anchor.go")
        .unwrap();
    assert_eq!(feature["kind"], "library");
    assert!(feature.get("entry_symbol").is_none(), "{feature}");
    assert!(
        feature["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "helper.go" && file["role"] == "owned"),
        "{feature}"
    );
    let target = feature["feature_id"].as_str().unwrap();
    let mut server = Server::start();
    let exact = cli_bundle(root.path(), "PublicAnchor");
    assert_eq!(
        exact["symbol_definitions"][0]["handle"],
        "sym:anchor.go#PublicAnchor"
    );
    let exact_mcp = server.export(root.path(), "PublicAnchor");
    assert_ne!(exact_mcp["isError"], true, "{exact_mcp}");
    assert_eq!(
        exact_mcp["structuredContent"]["symbol_definitions"],
        exact["symbol_definitions"]
    );

    let cli = cli_bundle(root.path(), target);
    let result = server.export(root.path(), target);
    assert_ne!(result["isError"], true, "{result}");
    let mcp = &result["structuredContent"];
    assert_eq!(cli["found"], true);
    assert!(cli["primary"].as_array().unwrap().is_empty());
    assert_eq!(cli["symbol_definitions"], mcp["symbol_definitions"]);
    let definitions = cli["symbol_definitions"]
        .as_array()
        .unwrap_or_else(|| panic!("mapped library lost structural definitions: {cli}"));
    let mut handles: Vec<_> = definitions
        .iter()
        .map(|row| row["handle"].as_str().unwrap())
        .collect();
    handles.sort();
    assert_eq!(
        handles,
        [
            "sym:anchor.go#PublicAnchor",
            "sym:anchor.go#SecondAnchor",
            "sym:helper.go#OwnedAnchor"
        ]
    );

    let limited = export(root.path(), target, &["--limit", "1"]);
    assert!(limited.status.success());
    let limited: Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(limited["symbol_definitions"].as_array().unwrap().len(), 1);
    assert_eq!(
        limited["symbol_definitions"][0]["handle"],
        "sym:anchor.go#PublicAnchor"
    );
    let limited_mcp = server.request("tools/call", json!({
        "name": "export_context", "arguments": {"project": root.path(), "target": target, "limit": 1}
    }));
    assert_ne!(limited_mcp["isError"], true, "{limited_mcp}");
    assert_eq!(
        limited_mcp["structuredContent"]["symbol_definitions"],
        limited["symbol_definitions"]
    );

    let standalone = server.request(
        "tools/call",
        json!({
            "name": "feature_bundle", "arguments": {"project": root.path(), "target": target}
        }),
    );
    assert_ne!(standalone["isError"], true, "{standalone}");
    assert!(
        standalone["structuredContent"]
            .get("symbol_definitions")
            .is_none()
    );
}

#[test]
fn ingest_output_labels_resolved_entities_and_retains_token_estimates() {
    let root = project();
    for (target, label) in [
        ("unique_anchor", "symbol=unique_anchor"),
        ("file:src/anchor.rs", "target=file:src/anchor.rs"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["export", target, "--format", "ingest"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(&format!("Target: {label}")), "{text}");
        assert!(text.contains("Approx tokens: ~"), "{text}");
        assert!(text.contains("unique_anchor"), "{text}");
    }
}

#[test]
fn free_text_still_reaches_the_semantic_model_in_both_adapters() {
    let root = project();
    let target = "explain how the anchor behaves";
    let output = export(root.path(), target, &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("allowlist"));
    let mut server = Server::start();
    let result = server.export(root.path(), target);
    assert_eq!(failure(&result)["error"]["code"], "E_MODEL");
}
