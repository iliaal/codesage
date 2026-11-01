//! The consolidated MCP surface: one noun per tool. Tool names and argument
//! spellings retired by the consolidation keep dispatching for one release,
//! unadvertised, and answer exactly as the tool that replaced them. Every
//! call goes through `tools/call` on the stdio server, as an agent's would.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

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
        let initialized = server.raw(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "mcp-retired-tools-test", "version": "1"}
            }),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "codesage");
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        server
    }

    /// The whole JSON-RPC response, so protocol errors are observable.
    fn raw(&mut self, method: &str, params: Value) -> Value {
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
                return response;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self.raw("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "tool failures must be tool results, not protocol errors: {response}"
        );
        response["result"].clone()
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
        assert_ne!(result["isError"], json!(true), "{tool} failed: {result}");
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

/// A Rust library crate, so feature mapping yields a library feature, with
/// a cross-file call so dependencies, impact, and bundles carry rows.
fn onboard(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"retired_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod helper;\nuse crate::helper::inner;\npub fn outer() -> u32 { inner() }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/helper.rs"),
        "pub fn inner() -> u32 {\n    7\n}\n\npub fn twin_a(x: u32) -> u32 {\n    if x > 1 { x + 1 } else { x }\n}\n\npub fn twin_b(y: u32) -> u32 {\n    if y > 1 { y + 1 } else { y }\n}\n",
    )
    .unwrap();
    run(root, &["init"]);
    run(root, &["index", "--no-semantic"]);
}

/// The payload minus what legitimately differs between two identical
/// questions asked at different instants: the measured cost and, on session
/// starts, the wall-clock creation stamp.
fn comparable(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.remove("cost");
        object.remove("created_at");
    }
    payload
}

fn error_code(result: &Value) -> String {
    assert_eq!(result["isError"], true, "must fail: {result}");
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find_map(|block| block["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("typed error block: {result}"))
}

fn error_message(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find_map(|block| block["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("error message: {result}"))
}

const RETIRED: [&str; 8] = [
    "assess_risk_batch",
    "assess_risk_diff",
    "list_features",
    "find_feature",
    "export_context",
    "feature_bundle",
    "session_start",
    "session_end",
];

#[test]
fn tools_list_advertises_one_noun_per_tool_and_no_retired_name() {
    let mut server = Server::start();
    let listing = server.raw("tools/list", json!({}));
    let mut names: Vec<String> = listing["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    // `batch` is a separate bead; it is not part of this surface yet.
    let mut expected = vec![
        "assess_risk",
        "bundle",
        "describe",
        "edit_check",
        "features",
        "find_coupling",
        "find_references",
        "find_similar",
        "find_symbol",
        "from_trace",
        "help",
        "impact_analysis",
        "list_dependencies",
        "project_overview",
        "recommend_tests",
        "review_rehearsal",
        "search",
        "session",
        "trace_call_path",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected);
    for retired in RETIRED {
        assert!(!names.iter().any(|name| name == retired), "{retired}");
    }
    let tool = |name: &str| {
        listing["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()
            .clone()
    };
    let impact = tool("impact_analysis");
    assert!(impact["inputSchema"]["properties"].get("is_file").is_none());
    let session = tool("session");
    assert_eq!(
        session["inputSchema"]["required"],
        json!(["project", "action"])
    );
    assert_eq!(session["annotations"]["readOnlyHint"], false);
    let risk = tool("assess_risk");
    assert!(
        risk["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("targets"))
    );
    assert!(risk["inputSchema"]["properties"].get("aggregate").is_some());
}

#[test]
fn every_retired_name_and_spelling_answers_as_its_successor() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let root = project.path();
    let mut server = Server::start();
    let same = |server: &mut Server, retired: (&str, Value), current: (&str, Value)| {
        let with_project = |mut arguments: Value| {
            arguments["project"] = json!(root);
            arguments
        };
        let old = server.ok(retired.0, with_project(retired.1.clone()));
        let new = server.ok(current.0, with_project(current.1.clone()));
        assert_eq!(
            new["tool"], current.0,
            "the envelope names the advertised tool: {new}"
        );
        assert_eq!(
            comparable(old),
            comparable(new),
            "{} {} must answer as {} {}",
            retired.0,
            retired.1,
            current.0,
            current.1
        );
    };

    let pair = json!(["src/helper.rs", "src/lib.rs"]);
    same(
        &mut server,
        ("assess_risk_batch", json!({"file_paths": pair})),
        ("assess_risk", json!({"targets": pair})),
    );
    same(
        &mut server,
        (
            "assess_risk_batch",
            json!({"targets": pair, "verbose": true}),
        ),
        ("assess_risk", json!({"targets": pair, "detail": "full"})),
    );
    same(
        &mut server,
        ("assess_risk_diff", json!({"file_paths": pair})),
        ("assess_risk", json!({"targets": pair, "aggregate": true})),
    );
    for spelling in [
        json!({"file_path": "src/helper.rs"}),
        json!({"target": "src/helper.rs"}),
        json!({"file_paths": ["src/helper.rs"]}),
    ] {
        same(
            &mut server,
            ("assess_risk", spelling),
            ("assess_risk", json!({"targets": ["src/helper.rs"]})),
        );
    }

    same(
        &mut server,
        ("list_features", json!({})),
        ("features", json!({})),
    );
    same(
        &mut server,
        ("list_features", json!({"kind": "library", "limit": 5})),
        ("features", json!({"kind": "library", "limit": 5})),
    );
    for spelling in [
        json!({"file_path": "src/helper.rs"}),
        json!({"target": "src/helper.rs"}),
    ] {
        same(
            &mut server,
            ("find_feature", spelling.clone()),
            ("features", json!({"target": "src/helper.rs"})),
        );
        same(
            &mut server,
            ("features", spelling),
            ("features", json!({"target": "src/helper.rs"})),
        );
    }
    let owned = server.ok(
        "features",
        json!({"project": root, "target": "src/helper.rs"}),
    );
    let feature_id = owned["results"][0]["feature_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the library feature owns src/helper.rs: {owned}"))
        .to_owned();

    same(
        &mut server,
        ("export_context", json!({"target": "inner", "limit": 3})),
        ("bundle", json!({"target": "inner", "limit": 3})),
    );
    same(
        &mut server,
        (
            "export_context",
            json!({"target": "sym:src/lib.rs#outer", "include_callees": true}),
        ),
        (
            "bundle",
            json!({"target": "sym:src/lib.rs#outer", "include_callees": true}),
        ),
    );
    for spelling in [
        json!({"feature_id": feature_id}),
        json!({"target": feature_id}),
    ] {
        same(
            &mut server,
            ("feature_bundle", spelling),
            ("bundle", json!({"target": feature_id})),
        );
    }
    let bundle = server.ok("bundle", json!({"project": root, "target": feature_id}));
    assert_eq!(bundle["found"], true, "{bundle}");
    // The fixture indexes without embeddings, so the feature's content is
    // its structural definitions; the comparison is not over an empty bundle.
    assert!(
        bundle["symbol_definitions"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "inner")),
        "the comparison exercises a populated feature bundle: {bundle}"
    );

    same(
        &mut server,
        (
            "impact_analysis",
            json!({"target": "src/helper.rs", "is_file": true}),
        ),
        ("impact_analysis", json!({"target": "src/helper.rs"})),
    );
    same(
        &mut server,
        ("find_similar", json!({"name": "twin_a"})),
        ("find_similar", json!({"target": "twin_a"})),
    );
    same(
        &mut server,
        ("list_dependencies", json!({"file_path": "src/lib.rs"})),
        ("list_dependencies", json!({"target": "src/lib.rs"})),
    );

    // Two baselines taken through each spelling describe one state; the end
    // diff against one baseline is then the same through either spelling.
    same(
        &mut server,
        ("session_start", json!({"session_id": "retired-a"})),
        (
            "session",
            json!({"action": "start", "session_id": "retired-a"}),
        ),
    );
    std::fs::write(root.join("src/added.rs"), "pub fn added() {}\n").unwrap();
    run(root, &["index", "--no-semantic"]);
    same(
        &mut server,
        ("session_end", json!({"session_id": "retired-a"})),
        (
            "session",
            json!({"action": "end", "session_id": "retired-a"}),
        ),
    );
    let diff = server.ok(
        "session",
        json!({"project": root, "action": "end", "session_id": "retired-a"}),
    );
    assert!(
        diff["new_files"]
            .as_array()
            .is_some_and(|files| files.contains(&json!("src/added.rs"))),
        "the comparison exercises a non-empty session diff: {diff}"
    );
}

#[test]
fn unknown_tools_and_contradictory_spellings_are_refused() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let root = project.path();
    let mut server = Server::start();

    // An unknown name is still the router's protocol-level refusal.
    let unknown = server.raw(
        "tools/call",
        json!({"name": "no_such_tool", "arguments": {"project": root}}),
    );
    assert_eq!(unknown["error"]["message"], "tool not found", "{unknown}");
    assert!(unknown.get("result").is_none(), "{unknown}");

    for (tool, arguments, mentions) in [
        (
            "assess_risk",
            json!({"targets": ["src/helper.rs"], "target": "src/lib.rs"}),
            vec!["`targets`", "`target`", "src/lib.rs"],
        ),
        (
            "assess_risk",
            json!({"targets": ["src/helper.rs"], "file_paths": ["src/lib.rs"]}),
            vec!["`targets`", "`file_paths`"],
        ),
        (
            "assess_risk_diff",
            json!({"targets": ["src/helper.rs"], "aggregate": false}),
            vec!["aggregate", "assess_risk"],
        ),
        (
            "assess_risk_batch",
            json!({"targets": ["src/helper.rs"], "aggregate": true}),
            vec!["aggregate", "assess_risk"],
        ),
        (
            "session_start",
            json!({"session_id": "x", "action": "end"}),
            vec!["action", "session"],
        ),
        (
            "features",
            json!({"target": "src/helper.rs", "file_path": "src/lib.rs"}),
            vec!["`file_path`", "`target`"],
        ),
        (
            "bundle",
            json!({"target": "feat_1", "feature_id": "feat_2"}),
            vec!["`feature_id`", "`target`"],
        ),
        (
            "feature_bundle",
            json!({"target": "feat_1", "feature_id": "feat_2"}),
            vec!["`feature_id`", "`target`"],
        ),
        (
            "list_features",
            json!({"target": "src/helper.rs"}),
            vec!["`target`", "features"],
        ),
        ("session", json!({"session_id": "x"}), vec!["action"]),
    ] {
        let mut with_project = arguments.clone();
        with_project["project"] = json!(root);
        let result = server.call(tool, with_project);
        assert_eq!(
            error_code(&result),
            "E_PARAM",
            "{tool} {arguments}: {result}"
        );
        let message = error_message(&result);
        for mention in mentions {
            assert!(
                message.contains(mention),
                "{tool} {arguments}: the refusal must name {mention}: {message}"
            );
        }
    }

    // `help` points a retired name at its successor instead of "unknown".
    let help = server.call("help", json!({"project": root, "tool": "assess_risk_diff"}));
    assert_eq!(error_code(&help), "E_PARAM", "{help}");
    assert!(
        error_message(&help).contains("retired alias of `assess_risk`"),
        "{help}"
    );
}
