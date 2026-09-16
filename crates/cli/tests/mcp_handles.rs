//! Every advertised MCP tool that emits symbol, reference, or chunk rows
//! carries a legible handle on each row: `handle` on symbol and chunk rows,
//! `from` / `to` on reference rows. The rows are read exactly as an agent
//! would, through `tools/call` on the stdio server.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use codesage_protocol::Handle;
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
            .env("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")
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
        let initialized = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "handles-test", "version": "1"}
            }),
        );
        assert_eq!(initialized["serverInfo"]["name"], "codesage");
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        )
        .unwrap();
        loop {
            let line = self
                .responses
                .recv_timeout(Duration::from_secs(30))
                .expect("MCP response within 30 seconds");
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn call(&mut self, project: &Path, tool: &str, mut arguments: Value) -> Value {
        arguments["project"] = json!(project);
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert_ne!(result["isError"], true, "{tool}: {result}");
        result["structuredContent"].clone()
    }
}

fn run(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "codesage {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::create_dir_all(root.join("cluster")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"handles_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod helper;\nuse crate::helper::inner;\npub fn outer() -> u32 { inner() }\n",
    )
    .unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        std::fs::write(
            root.join(format!("cluster/{name}.rs")),
            format!("pub fn cluster_{name}() {{}}\n"),
        )
        .unwrap();
    }
    git(root, &["init", "-q"]);
    for revision in 0..3 {
        std::fs::write(
            root.join("src/helper.rs"),
            format!("pub fn inner() -> u32 {{ 7 }}\n// revision {revision}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("tests/helper_test.rs"),
            format!(
                "#[test]\nfn checks_inner() {{ assert_eq!(handles_fixture::helper::inner(), 7); }}\n// revision {revision}\n"
            ),
        )
        .unwrap();
        // A Python pair whose test is not a sibling by convention, so it can
        // only be recommended through co-change history and reachability.
        std::fs::write(
            root.join("engine.py"),
            format!("def calculate_payload():\n    return {revision}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("tests/test_behavior.py"),
            format!(
                "from engine import calculate_payload\n\ndef test_behavior():\n    assert calculate_payload() == {revision}\n"
            ),
        )
        .unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
    }
    run(root, &["init"]);
    run(root, &["index", "--no-semantic"]);
    run(root, &["git-index", "--full"]);
    let db = Database::open_for_model(
        &root.join(".codesage/index.db"),
        "jinaai/jina-embeddings-v2-base-code",
        4,
    )
    .unwrap();
    for path in ["src/lib.rs", "src/helper.rs"] {
        let body = std::fs::read_to_string(root.join(path)).unwrap();
        db.insert_chunks(
            path,
            "rust",
            &[(&body, 1, body.lines().count() as u32, &[0.1, 0.2, 0.3, 0.4])],
        )
        .unwrap();
    }
}

fn rows<'a>(payload: &'a Value, key: &str, what: &str) -> &'a Vec<Value> {
    let rows = payload[key]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `{key}` array missing: {payload}"));
    assert!(!rows.is_empty(), "{what}: `{key}` is empty: {payload}");
    rows
}

fn assert_handle(row: &Value, key: &str, prefix: &str, what: &str) -> String {
    let text = row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: row lacks string `{key}`: {row}"));
    assert!(text.starts_with(prefix), "{what}: `{key}` = {text}");
    assert!(
        Handle::parse(text).is_some(),
        "{what}: unparseable `{key}` {text}"
    );
    text.to_string()
}

#[test]
fn every_row_emitting_tool_carries_handles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let mut server = Server::start();

    let symbols = server.call(&root, "find_symbol", json!({"name": "inner"}));
    let inner = &rows(&symbols, "results", "find_symbol")[0];
    let inner_handle = assert_handle(inner, "handle", "sym:", "find_symbol");
    assert_eq!(inner_handle, "sym:src/helper.rs#inner");

    let references = server.call(&root, "find_references", json!({"name": "inner"}));
    let mut saw_caller = false;
    for row in rows(&references, "results", "find_references") {
        if row["from_symbol"].is_null() {
            assert!(row.get("from").is_none(), "omitted at file scope: {row}");
        } else {
            let from = assert_handle(row, "from", "sym:", "find_references");
            assert_eq!(from, "sym:src/lib.rs#outer");
            assert_eq!(row["to"], inner_handle, "{row}");
            saw_caller = true;
        }
    }
    assert!(saw_caller, "a call row from `outer`: {references}");

    let search = server.call(
        &root,
        "search",
        json!({"query": "inner helper", "limit": 5}),
    );
    for row in rows(&search, "results", "search") {
        let handle = assert_handle(row, "handle", "chunk:", "search");
        assert_eq!(
            handle,
            format!(
                "chunk:{}:{}-{}",
                row["file_path"].as_str().unwrap(),
                row["start_line"],
                row["end_line"]
            )
        );
    }

    let trace = server.call(
        &root,
        "trace_call_path",
        json!({"from": "outer", "to": "inner"}),
    );
    assert_eq!(trace["found"], true, "{trace}");
    let steps: Vec<String> = rows(&trace, "steps", "trace_call_path")
        .iter()
        .map(|step| assert_handle(step, "handle", "sym:", "trace_call_path"))
        .collect();
    assert_eq!(steps, ["sym:src/lib.rs#outer", "sym:src/helper.rs#inner"]);

    let impact = server.call(
        &root,
        "impact_analysis",
        json!({"target": "outer", "include_siblings": true}),
    );
    for sibling in rows(&impact, "sibling_symbols", "impact_analysis") {
        assert_handle(sibling, "handle", "sym:", "impact_analysis");
    }

    let bundle = server.call(
        &root,
        "export_context",
        json!({"target": "inner", "is_symbol": true}),
    );
    assert_eq!(bundle["found"], true, "{bundle}");
    for row in rows(&bundle, "primary", "export_context") {
        assert_handle(row, "handle", "chunk:", "export_context");
    }
    for definition in rows(&bundle, "symbol_definitions", "export_context") {
        assert_eq!(
            assert_handle(definition, "handle", "sym:", "export_context"),
            inner_handle
        );
    }

    let features = server.call(&root, "list_features", json!({}));
    let feature_id = rows(&features, "results", "list_features")[0]["feature_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        matches!(Handle::parse(&feature_id), Some(Handle::Feature { .. })),
        "{feature_id}"
    );
    let bundle = server.call(&root, "feature_bundle", json!({"feature_id": feature_id}));
    assert_eq!(bundle["found"], true, "{bundle}");
    for row in rows(&bundle, "primary", "feature_bundle") {
        assert_handle(row, "handle", "chunk:", "feature_bundle");
    }

    // File-shaped rows carry `file:` handles.
    let impact = server.call(&root, "impact_analysis", json!({"target": "inner"}));
    for row in rows(&impact, "results", "impact_analysis") {
        assert_eq!(
            assert_handle(row, "handle", "file:", "impact_analysis"),
            format!("file:{}", row["file_path"].as_str().unwrap())
        );
    }

    let deps = server.call(
        &root,
        "list_dependencies",
        json!({"file_path": "src/lib.rs"}),
    );
    assert_eq!(
        assert_handle(&deps, "handle", "file:", "list_dependencies"),
        "file:src/lib.rs"
    );

    let coupling = server.call(
        &root,
        "find_coupling",
        json!({"file_path": "src/helper.rs"}),
    );
    for row in rows(&coupling, "coupled", "find_coupling") {
        assert_eq!(
            assert_handle(row, "handle", "file:", "find_coupling"),
            format!("file:{}", row["file"].as_str().unwrap())
        );
    }

    let risk = server.call(
        &root,
        "assess_risk",
        json!({"file_path": "src/helper.rs", "verbose": true}),
    );
    assert_eq!(
        assert_handle(&risk, "handle", "file:", "assess_risk"),
        "file:src/helper.rs"
    );
    for row in rows(&risk, "top_coupled", "assess_risk") {
        assert_handle(row, "handle", "file:", "assess_risk top_coupled");
    }

    let tests = server.call(
        &root,
        "recommend_tests",
        json!({"file_paths": ["engine.py"]}),
    );
    for row in rows(&tests, "coupled", "recommend_tests") {
        assert_eq!(
            assert_handle(row, "handle", "file:", "recommend_tests coupled"),
            format!("file:{}", row["file"].as_str().unwrap())
        );
    }
    // The fixture's reachability walk does not reach the Python test, so the
    // bucket may be empty here; the row shape is pinned by the schema test.
    for row in tests["reachable"].as_array().into_iter().flatten() {
        assert_eq!(
            assert_handle(row, "handle", "file:", "recommend_tests reachable"),
            format!("file:{}", row["path"].as_str().unwrap())
        );
    }

    let trace = server.call(
        &root,
        "from_trace",
        json!({"trace": "thread 'main' panicked at src/helper.rs:1:5:\nboom\n   0: handles_fixture::helper::inner\n             at ./src/helper.rs:1:1\n   1: handles_fixture::outer\n             at ./src/lib.rs:3:1\n"}),
    );
    let mut resolved = 0;
    for frame in rows(&trace, "frames", "from_trace") {
        if frame["status"] == "resolved" {
            let indexed = frame["symbol"]["path"]
                .as_str()
                .or_else(|| frame["file"].as_str())
                .unwrap();
            assert_eq!(
                assert_handle(frame, "handle", "file:", "from_trace"),
                format!("file:{indexed}")
            );
            resolved += 1;
        } else {
            assert!(frame.get("handle").is_none(), "{frame}");
        }
    }
    assert!(resolved > 0, "{trace}");

    let cluster_files: Vec<String> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|n| format!("cluster/{n}.rs"))
        .collect();
    let diff = server.call(
        &root,
        "assess_risk_diff",
        json!({"file_paths": cluster_files}),
    );
    for cluster in rows(&diff, "clustered_directories", "assess_risk_diff") {
        assert_eq!(
            assert_handle(cluster, "handle", "dir:", "assess_risk_diff"),
            format!("dir:{}", cluster["directory"].as_str().unwrap())
        );
        for row in rows(cluster, "top_files", "assess_risk_diff cluster") {
            assert_handle(row, "handle", "file:", "assess_risk_diff top_files");
        }
    }
}
