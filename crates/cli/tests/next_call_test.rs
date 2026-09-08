use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

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
            .env("CODESAGE_BUNDLE_TOKEN_BUDGET", "100")
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
                "clientInfo": {"name": "next-call-test", "version": "1"}
            }),
        );
        assert_eq!(initialized["serverInfo"]["name"], "codesage");
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
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

fn run(root: &Path, executable: &str, args: &[&str]) {
    let output = Command::new(executable)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{executable} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::create_dir_all(root.join("cluster")).unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        std::fs::write(
            root.join(format!("cluster/{name}.rs")),
            format!("pub fn cluster_{name}() {{}}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("engine.py"),
        "def calculate_payload():\n    return 7\n",
    )
    .unwrap();
    std::fs::write(root.join("tests/test_behavior.py"), "from engine import calculate_payload\n\ndef test_behavior():\n    assert calculate_payload() == 7\n").unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"next_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod helper;\nuse crate::helper::inner;\npub fn outer() -> u32 { inner() }\npub fn twin_a(n: u32) -> u32 { let mut total = 0; for i in 0..n { total += i * 2; } total }\npub fn twin_b(m: u32) -> u32 { let mut sum = 0; for j in 0..m { sum += j * 2; } sum }\n").unwrap();
    std::fs::write(root.join("src/helper.rs"), "pub fn inner() -> u32 { 7 }\n").unwrap();
    std::fs::write(
        root.join("tests/helper_test.rs"),
        "#[test]\nfn checks_inner() { assert_eq!(next_fixture::helper::inner(), 7); }\n",
    )
    .unwrap();
    let symbols: String = (0..512)
        .map(|i| format!("pub mod module_{i} {{ pub fn budget_symbol() {{}} }}\n"))
        .collect();
    std::fs::write(root.join("src/budget.rs"), symbols).unwrap();
    run(root, "git", &["init", "-q"]);
    run(root, "git", &["add", "."]);
    for revision in 0..3 {
        std::fs::write(
            root.join("src/helper.rs"),
            format!("pub fn inner() -> u32 {{ 7 }}\n// revision {revision}\n"),
        )
        .unwrap();
        std::fs::write(root.join("tests/helper_test.rs"), format!("#[test]\nfn checks_inner() {{ assert_eq!(next_fixture::helper::inner(), 7); }}\n// revision {revision}\n")).unwrap();
        run(
            root,
            "git",
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qam",
                "fixture",
            ],
        );
    }
    for args in [
        &["init"][..],
        &["index", "--no-semantic"],
        &["git-index", "--full"],
    ] {
        run(root, env!("CARGO_BIN_EXE_codesage"), args);
    }
    let db = Database::open_for_model(
        &root.join(".codesage/index.db"),
        "jinaai/jina-embeddings-v2-base-code",
        4,
    )
    .unwrap();
    for path in [
        "src/lib.rs",
        "src/helper.rs",
        "src/budget.rs",
        "tests/helper_test.rs",
    ] {
        let body = std::fs::read_to_string(root.join(path)).unwrap();
        db.insert_chunks(
            path,
            "rust",
            &[(&body, 1, body.lines().count() as u32, &[0.1, 0.2, 0.3, 0.4])],
        )
        .unwrap();
    }
}

fn assert_followups(server: &mut Server, tools: &BTreeMap<String, Value>, first: &Value) -> usize {
    let mut result = first.clone();
    let mut seen = BTreeSet::new();
    let mut count = 0;
    loop {
        assert_ne!(result["isError"], true, "{result}");
        let payload = &result["structuredContent"];
        let next = payload
            .get("next")
            .expect("every successful tool response declares next");
        if next.is_null() {
            return count;
        }
        assert!(count < 5, "next chain must terminate: {seen:?}");
        assert!(seen.insert(next.to_string()), "repeated next: {next}");
        let tool = next["tool"].as_str().unwrap();
        let advertised = tools.get(tool).expect("next names an advertised tool");
        let arguments = next["arguments"].as_object().unwrap();
        let input = &advertised["inputSchema"];
        for required in input["required"].as_array().unwrap() {
            assert!(
                arguments.contains_key(required.as_str().unwrap()),
                "{next} lacks {required}"
            );
        }
        for (key, value) in arguments {
            assert!(
                input["properties"].get(key).is_some(),
                "unadvertised next argument {key}"
            );
            assert!(
                value.is_string(),
                "next arguments are evidence strings: {next}"
            );
            assert_eq!(
                input["properties"][key]["type"], "string",
                "next argument schema drift: {key}"
            );
            if key == "project" {
                assert!(Path::new(value.as_str().unwrap()).is_absolute());
            } else {
                let mut evidence = payload.clone();
                evidence.as_object_mut().unwrap().remove("next");
                assert!(
                    contains_string(&evidence, value.as_str().unwrap()),
                    "next invented evidence: {next}"
                );
            }
        }
        result = server.request("tools/call", json!({"name": tool, "arguments": arguments}));
        count += 1;
    }
}

fn contains_string(value: &Value, wanted: &str) -> bool {
    match value {
        Value::String(s) => s == wanted,
        Value::Array(values) => values.iter().any(|v| contains_string(v, wanted)),
        Value::Object(values) => values.values().any(|v| contains_string(v, wanted)),
        _ => false,
    }
}

#[test]
fn reachable_only_recommendation_emits_an_executable_next() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
        .collect();
    let result = server.call(
        project.path(),
        "recommend_tests",
        json!({"file_paths": ["engine.py"]}),
    );
    let payload = &result["structuredContent"];
    assert_eq!(payload["primary"], json!([]));
    assert_eq!(payload["coupled"], json!([]));
    assert_eq!(payload["reach_walk_capped"], false);
    assert_eq!(payload["reachable"][0]["path"], "tests/test_behavior.py");
    assert_eq!(
        payload["next"]["arguments"]["file_path"],
        "tests/test_behavior.py"
    );
    assert!(assert_followups(&mut server, &tools, &result) > 0);
}

#[test]
fn clustered_risk_evidence_emits_an_executable_next() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
        .collect();
    let paths: Vec<_> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|name| format!("cluster/{name}.rs"))
        .collect();
    let result = server.call(
        project.path(),
        "assess_risk_diff",
        json!({"file_paths": paths}),
    );
    let payload = &result["structuredContent"];
    assert_eq!(payload["files"], json!([]));
    assert_eq!(
        payload["clustered_directories"][0]["top_files"][0]["found"],
        true
    );
    let selected = &payload["clustered_directories"][0]["top_files"][0]["file"];
    assert!(selected.is_string());
    assert_eq!(&payload["next"]["arguments"]["file_path"], selected);
    assert!(assert_followups(&mut server, &tools, &result) > 0);
}

#[test]
fn every_advertised_tool_emits_executable_evidence_derived_terminating_next() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools: BTreeMap<String, Value> = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
        .collect();
    let features = server.call(project.path(), "list_features", json!({}));
    let feature_id = features["structuredContent"]["results"][0]["feature_id"].clone();
    assert!(feature_id.is_string());
    let calls = [
        ("project_overview", json!({}), true),
        ("find_symbol", json!({"name":"inner"}), true),
        ("find_references", json!({"name":"inner"}), true),
        ("find_similar", json!({"name":"twin_a"}), true),
        (
            "list_dependencies",
            json!({"file_path":"src/helper.rs"}),
            true,
        ),
        ("search", json!({"query":"inner", "limit": 1000}), true),
        (
            "trace_call_path",
            json!({"from":"outer", "to":"inner"}),
            true,
        ),
        (
            "from_trace",
            json!({"trace":"thread 'main' panicked at src/helper.rs:1:1:\nboom\nstack backtrace:\n   0: next_fixture::helper::inner\n             at src/helper.rs:1:1\n"}),
            true,
        ),
        ("impact_analysis", json!({"target":"inner"}), true),
        (
            "export_context",
            json!({"target":"outer", "is_symbol":true}),
            false,
        ),
        (
            "edit_check",
            json!({"file_path":"src/helper.rs", "symbol_name":"inner", "replacement":"pub fn inner() -> u32 { 8 }"}),
            false,
        ),
        ("find_coupling", json!({"file_path":"src/helper.rs"}), true),
        ("assess_risk", json!({"file_path":"src/helper.rs"}), true),
        (
            "assess_risk_batch",
            json!({"file_paths":["src/helper.rs"]}),
            true,
        ),
        (
            "assess_risk_diff",
            json!({"file_paths":["src/helper.rs"]}),
            true,
        ),
        (
            "recommend_tests",
            json!({"file_paths":["src/helper.rs"]}),
            true,
        ),
        (
            "review_rehearsal",
            json!({"file_paths":["src/lib.rs"]}),
            true,
        ),
        ("list_features", json!({}), true),
        ("find_feature", json!({"file_path":"src/helper.rs"}), true),
        ("feature_bundle", json!({"feature_id":feature_id}), false),
        ("session_start", json!({"session_id":"next-test"}), false),
        ("session_end", json!({"session_id":"next-test"}), true),
    ];
    let covered: BTreeSet<_> = calls.iter().map(|(name, _, _)| name.to_string()).collect();
    assert_eq!(
        covered,
        tools.keys().cloned().collect(),
        "new tools need a next contract test"
    );
    for (tool, arguments, expected_next) in calls {
        if tool == "session_end" {
            std::fs::write(
                project.path().join("src/new_file.rs"),
                "pub fn added() {}\n",
            )
            .unwrap();
            run(
                project.path(),
                env!("CARGO_BIN_EXE_codesage"),
                &["index", "--no-semantic"],
            );
        }
        assert!(
            tools[tool]["outputSchema"]["properties"]
                .get("next")
                .is_some(),
            "{tool} lacks next schema"
        );
        let result = server.call(project.path(), tool, arguments);
        let count = assert_followups(&mut server, &tools, &result);
        assert_eq!(count > 0, expected_next, "{tool}: {result}");
        if tool == "search" {
            assert!(result["structuredContent"]["_meta"]["clamps"].is_array());
        }
    }
    let budgeted = server.call(
        project.path(),
        "find_symbol",
        json!({"name":"budget_symbol"}),
    );
    assert_eq!(budgeted["structuredContent"]["_meta"]["truncated"], true);
    assert!(assert_followups(&mut server, &tools, &budgeted) > 0);
    for (tool, arguments) in [
        ("find_symbol", json!({"name":"does_not_exist_anywhere"})),
        ("find_references", json!({"name":"does_not_exist_anywhere"})),
        ("list_dependencies", json!({"file_path":"absent.rs"})),
        ("find_feature", json!({"file_path":"absent.rs"})),
        ("feature_bundle", json!({"feature_id":"absent"})),
        ("search", json!({"query":"inner", "limit":0})),
        ("from_trace", json!({"trace":"at external.py:42"})),
    ] {
        let result = server.call(project.path(), tool, arguments);
        assert_eq!(
            assert_followups(&mut server, &tools, &result),
            0,
            "{tool}: {result}"
        );
    }
    let error = server.call(project.path(), "recommend_tests", json!({"file_paths":[]}));
    assert_eq!(error["isError"], true);
    assert!(error.get("structuredContent").is_none());
    assert!(
        error["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["text"] == "{\"next\":null}")
    );
}
