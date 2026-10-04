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
        Self::start_with_envelope(None)
    }

    fn start_with_envelope(envelope: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .args(["mcp", "--direct"])
            .env_remove("CODESAGE_ENVELOPE")
            .env("CODESAGE_WATCH", "0")
            .env("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")
            .env("CODESAGE_BUNDLE_TOKEN_BUDGET", "100")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(envelope) = envelope {
            command.env("CODESAGE_ENVELOPE", envelope);
        }
        let mut child = command.spawn().unwrap();
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
    let callers: String = (0..12)
        .map(|i| format!("pub fn caller_{i}() -> u32 {{ inner() }}\n"))
        .collect();
    std::fs::write(root.join("src/lib.rs"), "pub mod helper;\nuse crate::helper::inner;\npub fn outer() -> u32 { inner() }\npub fn twin_a(n: u32) -> u32 { let mut total = 0; for i in 0..n { total += i * 2; } total }\npub fn twin_b(m: u32) -> u32 { let mut sum = 0; for j in 0..m { sum += j * 2; } sum }\n".to_owned() + &callers).unwrap();
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
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
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
    let mut pending = vec![(first.clone(), BTreeSet::new())];
    let mut count = 0;
    while let Some((result, chain)) = pending.pop() {
        assert_ne!(result["isError"], true, "{result}");
        let payload = &result["structuredContent"];
        let next_entries = payload
            .get("next")
            .and_then(Value::as_array)
            .expect("every successful tool response declares next[]");
        assert!(next_entries.len() <= 3, "{next_entries:?}");
        assert!(serde_json::to_vec(next_entries).unwrap().len() <= 2048);
        for next in next_entries {
            let mut seen = chain.clone();
            assert!(seen.len() < 8, "next chain must terminate: {seen:?}");
            assert!(
                seen.insert(next.to_string()),
                "repeated next in one chain: {next}"
            );
            let tool = next["tool"].as_str().unwrap();
            assert_ne!(tool, "list_dependencies", "no dependency-list default");
            assert!(
                next["why"]
                    .as_str()
                    .is_some_and(|why| !why.is_empty() && why.split_whitespace().count() < 10)
            );
            let advertised = tools.get(tool).expect("next names an advertised tool");
            let arguments = next["arguments"].as_object().unwrap();
            let input = &advertised["inputSchema"];
            assert_schema(input, input, &next["arguments"]);
            for (key, value) in arguments {
                assert!(
                    input["properties"].get(key).is_some(),
                    "unadvertised next argument {key}"
                );
                if key == "project" {
                    assert!(Path::new(value.as_str().unwrap()).is_absolute());
                } else if key == "session_id" && tool == "session_start" {
                    let id = value.as_str().unwrap();
                    assert!(id.starts_with("next-") && id.len() < 128);
                    assert!(
                        id.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    );
                    assert!(
                        !Path::new(arguments["project"].as_str().unwrap())
                            .join(".codesage/sessions")
                            .join(format!("{id}.json"))
                            .exists(),
                        "suggestion must not replace a saved baseline"
                    );
                } else {
                    let mut evidence = payload.clone();
                    evidence.as_object_mut().unwrap().remove("next");
                    let values = value
                        .as_array()
                        .cloned()
                        .unwrap_or_else(|| vec![value.clone()]);
                    for value in values {
                        let value = value.as_str().expect("identity evidence is a string");
                        let path = codesage_protocol::Handle::parse(value)
                            .and_then(|handle| handle.path().map(str::to_owned));
                        assert!(
                            contains_string(&evidence, value)
                                || path
                                    .as_deref()
                                    .is_some_and(|path| contains_string(&evidence, path)),
                            "next invented evidence: {next}"
                        );
                    }
                }
            }
            let result =
                server.request("tools/call", json!({"name": tool, "arguments": arguments}));
            pending.push((result, seen));
            count += 1;
        }
    }
    count
}

fn assert_schema(root: &Value, schema: &Value, value: &Value) {
    for key in schema.as_object().unwrap().keys() {
        assert!(
            [
                "$schema",
                "$ref",
                "$defs",
                "title",
                "description",
                "type",
                "required",
                "properties",
                "additionalProperties",
                "anyOf",
                "default",
                "enum",
                "const",
                "items",
                "minItems",
                "maxItems",
                "minimum",
                "maximum",
            ]
            .contains(&key.as_str()),
            "schema assertion needs support before emitting arguments constrained by {key}"
        );
    }
    if let Some(reference) = schema["$ref"].as_str() {
        assert_schema(
            root,
            root.pointer(reference.strip_prefix('#').unwrap()).unwrap(),
            value,
        );
        return;
    }
    if let Some(types) = schema.get("type") {
        let types = types
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![types.clone()]);
        assert!(
            types.iter().any(|kind| match kind.as_str().unwrap() {
                "null" => value.is_null(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                other => panic!("unsupported schema type {other}"),
            }),
            "{value} does not satisfy {schema}"
        );
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let variant = variants
            .iter()
            .find(|variant| variant["type"] != "null")
            .unwrap();
        assert_schema(root, variant, value);
    }
    if let Some(variants) = schema["enum"].as_array() {
        assert!(variants.contains(value), "{value} not in {variants:?}");
    }
    if let Some(expected) = schema.get("const") {
        assert_eq!(value, expected);
    }
    if let Some(minimum) = schema["minimum"].as_f64() {
        assert!(value.as_f64().unwrap() >= minimum);
    }
    if let Some(maximum) = schema["maximum"].as_f64() {
        assert!(value.as_f64().unwrap() <= maximum);
    }
    if let Some(object) = value.as_object() {
        for required in schema["required"].as_array().into_iter().flatten() {
            assert!(
                object.contains_key(required.as_str().unwrap()),
                "{value} lacks {required}"
            );
        }
        for (key, child) in object {
            let property = schema["properties"]
                .get(key)
                .expect("every emitted argument must be advertised");
            assert_schema(root, property, child);
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(min) = schema["minItems"].as_u64() {
            assert!(items.len() as u64 >= min);
        }
        if let Some(max) = schema["maxItems"].as_u64() {
            assert!(items.len() as u64 <= max);
        }
        for item in items {
            assert_schema(root, &schema["items"], item);
        }
    }
}

#[test]
fn rehearsal_discloses_unscored_history_through_mcp() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    std::fs::write(project.path().join("fresh.rs"), "pub fn fresh() {}\n").unwrap();
    run(
        project.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let listed = server.request("tools/list", json!({}));
    let tool = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "review_rehearsal")
        .unwrap();
    assert!(
        tool["outputSchema"]["properties"]
            .get("objections")
            .is_some()
    );
    for (paths, severity) in [
        (json!(["fresh.rs"]), Some("medium")),
        (json!(["fresh.rs", "src/lib.rs"]), Some("low")),
        (json!(["src/lib.rs"]), None),
    ] {
        let result = server.call(
            project.path(),
            "review_rehearsal",
            json!({"file_paths":paths}),
        );
        assert_ne!(result["isError"], true, "{result}");
        let objections: Vec<_> = result["structuredContent"]["objections"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["category"] == "unscored-risk")
            .collect();
        assert_eq!(
            objections.len(),
            usize::from(severity.is_some()),
            "{result}"
        );
        if let Some(severity) = severity {
            assert_eq!(objections[0]["severity"], severity);
            assert_eq!(objections[0]["files"], json!(["fresh.rs"]));
            assert!(
                objections[0]["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e.as_str().unwrap().contains("codesage git-index"))
            );
        }
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
    // The row carries a `file:` handle, so the follow-up names the file by
    // handle rather than re-spelling its path.
    assert_eq!(
        payload["reachable"][0]["handle"],
        "file:tests/test_behavior.py"
    );
    assert_eq!(
        payload["next"][0]["arguments"]["target"],
        "file:tests/test_behavior.py"
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
    assert_eq!(payload["next"][0]["tool"], "review_rehearsal");
    assert_eq!(
        payload["next"][0]["arguments"]["targets"][0],
        format!("file:{}", selected.as_str().unwrap())
    );
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
    let mut calls = vec![
        ("project_overview", json!({}), true),
        ("describe", json!({"target":"file:src/helper.rs"}), true),
        ("describe", json!({"target":"file:src/lib.rs"}), true),
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
    if tools.contains_key("help") {
        calls.push(("help", json!({}), false));
    }
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
    let candidates = budgeted["structuredContent"]["next"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    for (candidate, definition) in candidates
        .iter()
        .zip(budgeted["structuredContent"]["results"].as_array().unwrap())
    {
        assert_eq!(candidate["tool"], "describe");
        assert_eq!(candidate["arguments"]["target"], definition["handle"]);
    }
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
    let blocks: Vec<Value> = error["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .collect();
    assert_eq!(blocks.len(), 1, "one contract block per failure: {error}");
    assert!(blocks[0].get("status").is_some(), "{error}");
    assert_eq!(blocks[0]["next"], json!([]), "{error}");
}

#[test]
fn wide_reference_floors_and_high_cliff_search_choose_specific_questions() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    let db = Database::open_for_model(
        &project.path().join(".codesage/index.db"),
        "jinaai/jina-embeddings-v2-base-code",
        4,
    )
    .unwrap();
    for path in ["src/lib.rs", "src/budget.rs", "tests/helper_test.rs"] {
        let body = std::fs::read_to_string(project.path().join(path)).unwrap();
        db.delete_chunks_for_file(path).unwrap();
        db.insert_chunks(
            path,
            "rust",
            &[(
                &body,
                1,
                body.lines().count() as u32,
                &[-0.1, -0.2, -0.3, -0.4],
            )],
        )
        .unwrap();
    }
    drop(db);
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
        .collect();
    let references = server.call(
        project.path(),
        "find_references",
        json!({"target":"sym:src/helper.rs#inner","detail":"standard"}),
    );
    assert!(
        references["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len()
            >= 10
    );
    assert_eq!(
        references["structuredContent"]["next"][0]["tool"],
        "impact_analysis"
    );
    assert_eq!(
        references["structuredContent"]["next"][0]["arguments"]["target"],
        "sym:src/helper.rs#inner"
    );
    assert!(assert_followups(&mut server, &tools, &references) > 0);
    let compact = server.call(
        project.path(),
        "find_references",
        json!({"target":"sym:src/helper.rs#inner"}),
    );
    assert!(
        compact["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len()
            < 10,
        "compact fixture must trim below the wide-reference threshold: {compact}"
    );
    assert_eq!(
        compact["structuredContent"]["next"],
        references["structuredContent"]["next"]
    );
    assert!(assert_followups(&mut server, &tools, &compact) > 0);
    let search = server.call(
        project.path(),
        "search",
        json!({"query":"locate runtime invariant behavior","limit":4}),
    );
    assert_eq!(
        search["structuredContent"]["confidence"], "high",
        "{search}"
    );
    assert_eq!(search["structuredContent"]["next"][0]["tool"], "describe");
    assert_eq!(
        search["structuredContent"]["next"][0]["arguments"]["target"],
        "file:src/helper.rs"
    );
    assert!(assert_followups(&mut server, &tools, &search) > 0);
}

#[test]
fn high_risk_session_advice_preserves_existing_baselines() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    let db = Database::open(&project.path().join(".codesage/index.db")).unwrap();
    db.upsert_git_file("src/helper.rs", 1000.0, 10, 10, Some(1_790_000_000))
        .unwrap();
    drop(db);
    let mut server = Server::start();
    let baseline = server.call(project.path(), "session_start", json!({}));
    assert_eq!(baseline["structuredContent"]["next"], json!([]));
    let baseline_path = project.path().join(".codesage/sessions/default.json");
    let saved = std::fs::read(&baseline_path).unwrap();
    let risk = server.call(
        project.path(),
        "assess_risk",
        json!({"target":"file:src/helper.rs"}),
    );
    assert!(
        risk["structuredContent"]["score"].as_f64().unwrap() >= 0.5,
        "{risk}"
    );
    let start = risk["structuredContent"]["next"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["tool"] == "session_start")
        .unwrap();
    let fresh = server.request(
        "tools/call",
        json!({"name":"session_start","arguments":start["arguments"]}),
    );
    assert_ne!(fresh["isError"], true, "{fresh}");
    assert_eq!(fresh["structuredContent"]["next"], json!([]));
    assert_eq!(
        fresh["structuredContent"]["session_id"],
        start["arguments"]["session_id"]
    );
    assert_eq!(std::fs::read(&baseline_path).unwrap(), saved);
}

#[test]
fn stale_index_recovery_is_a_cli_command_and_legacy_keeps_array_contract() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    std::fs::write(
        project.path().join("src/helper.rs"),
        "pub fn inner() -> u32 { 8 }\n",
    )
    .unwrap();
    for mode in [None, Some("legacy")] {
        let mut server = Server::start_with_envelope(mode);
        let result = server.call(project.path(), "find_symbol", json!({"target":"inner"}));
        assert_ne!(result["isError"], true, "{result}");
        let payload = &result["structuredContent"];
        assert!(payload["next"].is_array());
        if mode.is_none() {
            assert_eq!(payload["index"]["structural"], "dirty");
            assert_eq!(payload["index"]["recover"]["command"], "codesage index");
            assert_eq!(payload["index"]["recover"]["cwd"], json!(project.path()));
        } else {
            assert!(payload.get("index").is_none());
            assert!(
                payload["_meta"]["stale_warning"]
                    .as_str()
                    .unwrap()
                    .contains("codesage index")
            );
        }
    }
}

#[test]
fn ambiguous_errors_preserve_remedies_and_offer_executable_candidates() {
    let project = tempfile::tempdir().unwrap();
    fixture(project.path());
    for index in 0..4 {
        std::fs::write(
            project.path().join(format!("candidate_{index}.c")),
            "int run(void) { return 1; }\n",
        )
        .unwrap();
    }
    run(
        project.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let listed = server.request("tools/list", json!({}));
    let tools: BTreeMap<_, _> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
        .collect();
    for (tool, arguments) in [
        ("impact_analysis", json!({"target":"run"})),
        ("export_context", json!({"target":"run", "is_symbol":true})),
        (
            "edit_check",
            json!({"file_path":"src/budget.rs", "symbol_name":"budget_symbol", "replacement":"pub fn budget_symbol() {}"}),
        ),
    ] {
        let result = server.call(project.path(), tool, arguments);
        assert_eq!(result["isError"], true, "{result}");
        let block: Value = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|part| part["text"].as_str())
            .filter_map(|text| serde_json::from_str::<Value>(text).ok())
            .find(|value| value.get("error").is_some())
            .unwrap();
        assert_eq!(block["error"]["code"], "E_AMBIGUOUS", "{block}");
        assert_eq!(block["error"]["remedy"]["tool"], tool);
        let next = block["next"].as_array().unwrap();
        assert_eq!(next.len(), 3, "{block}");
        assert!(serde_json::to_vec(next).unwrap().len() <= 2048);
        let candidates = block["error"]["candidates"].as_array().unwrap();
        for (call, candidate) in next.iter().zip(candidates) {
            let expected_tool = if tool == "edit_check" {
                "edit_check"
            } else {
                "describe"
            };
            assert_eq!(call["tool"], expected_tool);
            assert_eq!(&call["arguments"]["target"], candidate);
            assert!(call["why"].as_str().unwrap().split_whitespace().count() < 10);
            let schema = &tools[expected_tool]["inputSchema"];
            assert_schema(schema, schema, &call["arguments"]);
            let followed = server.request(
                "tools/call",
                json!({"name":expected_tool,"arguments":call["arguments"]}),
            );
            assert_ne!(followed["isError"], true, "{followed}");
            assert_followups(&mut server, &tools, &followed);
        }
    }
}
