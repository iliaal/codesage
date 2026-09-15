//! Every MCP failure returns one contract block: `tool`, `error.code`,
//! `error.remedy` (tool call, command, or null), plus the legacy status fields.

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
        let initialized = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "mcp-errors-test", "version": "1"}
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
                assert!(
                    response.get("error").is_none(),
                    "tool failures must be tool results, not protocol errors: {response}"
                );
                return response["result"].clone();
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

/// The failed result's contract block and its first (human-readable) text.
struct Failure {
    text: String,
    block: Value,
}

fn failure(result: &Value, tool: &str) -> Failure {
    assert_eq!(result["isError"], true, "{tool} must fail: {result}");
    assert!(
        result.get("structuredContent").is_none(),
        "errors carry no structured content: {result}"
    );
    let content = result["content"].as_array().expect("content array");
    let text = content[0]["text"]
        .as_str()
        .expect("first block is text")
        .to_owned();
    let blocks: Vec<Value> = content
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .filter(|value| value.get("status").is_some())
        .collect();
    assert_eq!(
        blocks.len(),
        1,
        "exactly one contract block per failure: {result}"
    );
    let block = blocks.into_iter().next().unwrap();
    assert_eq!(block["tool"], tool, "{block}");
    assert_eq!(block["complete"], false, "{block}");
    assert_eq!(block["next"], Value::Null, "{block}");
    for key in [
        "status",
        "phase",
        "work_continuing",
        "request_id",
        "persistence_committed",
    ] {
        assert!(
            block.get(key).is_some(),
            "legacy field {key} missing: {block}"
        );
    }
    assert_eq!(
        block["error"]["message"], text,
        "message mirrors the readable text: {result}"
    );
    assert!(block["error"]["code"].is_string(), "{block}");
    Failure { text, block }
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

/// Two distinct definitions named `dup` make an unqualified impact target ambiguous.
fn onboard(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod widget;\n\npub fn dup() -> u32 {\n    1\n}\n\npub fn only_once() -> u32 {\n    dup()\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/widget.rs"),
        "pub struct Widget;\n\nimpl Widget {\n    pub fn dup(&self) -> u32 {\n        2\n    }\n}\n",
    )
    .unwrap();
    run(root, &["init"]);
    run(root, &["index", "--no-semantic"]);
}

#[test]
fn parameter_and_routing_failures_name_the_tool_and_code() {
    let mut server = Server::start();

    let result = server.call(
        "find_coupling",
        json!({"project": "/nonexistent", "file_path": "a.rs", "limit": "not-a-number"}),
    );
    let param = failure(&result, "find_coupling");
    assert_eq!(param.block["error"]["code"], "E_PARAM");
    assert_eq!(param.block["error"]["remedy"], Value::Null);
    assert_eq!(param.block["status"], "error");
    assert!(
        param.text.starts_with("failed to deserialize parameters"),
        "{}",
        param.text
    );
    assert!(param.text.contains("not-a-number"), "{}", param.text);
    assert!(param.block["request_id"].is_u64(), "{}", param.block);

    let result = server.call(
        "find_symbol",
        json!({"project": "relative/project", "name": "dup"}),
    );
    let relative = failure(&result, "find_symbol");
    assert_eq!(relative.block["error"]["code"], "E_PROJECT_PATH");
    assert_eq!(relative.block["error"]["remedy"], Value::Null);
    assert!(relative.text.contains("absolute"), "{}", relative.text);

    let result = server.call(
        "find_symbol",
        json!({"project": "/nonexistent/codesage-errors-test", "name": "dup"}),
    );
    let missing = failure(&result, "find_symbol");
    assert_eq!(missing.block["error"]["code"], "E_PROJECT_PATH");
    assert_eq!(missing.block["error"]["remedy"], Value::Null);
    assert!(missing.text.contains("does not exist"), "{}", missing.text);

    let plain = tempfile::tempdir().unwrap();
    let result = server.call(
        "list_dependencies",
        json!({"project": plain.path(), "file_path": "src/lib.rs"}),
    );
    let onboarding = failure(&result, "list_dependencies");
    assert_eq!(onboarding.block["error"]["code"], "E_NOT_ONBOARDED");
    let remedy = &onboarding.block["error"]["remedy"];
    assert_eq!(remedy["command"], "codesage init && codesage index");
    assert_eq!(
        remedy["cwd"],
        json!(plain.path().canonicalize().unwrap()),
        "the directory travels as a field, never inside the command: {remedy}"
    );
    assert_eq!(remedy.as_object().unwrap().len(), 2, "{remedy}");

    // A directory name that is shell syntax stays inert data.
    let hostile = plain.path().join("a'; echo INJECTED; :'b");
    std::fs::create_dir(&hostile).unwrap();
    let result = server.call(
        "list_dependencies",
        json!({"project": hostile, "file_path": "src/lib.rs"}),
    );
    let remedy = failure(&result, "list_dependencies").block["error"]["remedy"].clone();
    assert_eq!(remedy["command"], "codesage init && codesage index");
    assert_eq!(remedy["cwd"], json!(hostile.canonicalize().unwrap()));
    assert!(
        onboarding.text.contains("not onboarded"),
        "{}",
        onboarding.text
    );
}

#[test]
fn daemon_stats_bad_recent_is_a_param_error_without_a_request_ticket() {
    let mut server = Server::start();
    let result = server.call("daemon_stats", json!({"recent": 999}));
    let stats = failure(&result, "daemon_stats");
    assert_eq!(stats.block["error"]["code"], "E_PARAM");
    assert_eq!(stats.block["error"]["remedy"], Value::Null);
    assert_eq!(stats.block["request_id"], Value::Null);
    assert_eq!(stats.block["work_continuing"], false);
    assert!(stats.text.contains("0 to 256"), "{}", stats.text);
}

#[test]
fn input_failures_on_an_onboarded_project_carry_machine_remedies() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let mut server = Server::start();

    let result = server.call(
        "impact_analysis",
        json!({"project": project.path(), "target": "dup"}),
    );
    let ambiguous = failure(&result, "impact_analysis");
    assert_eq!(ambiguous.block["error"]["code"], "E_AMBIGUOUS");
    let remedy = &ambiguous.block["error"]["remedy"];
    assert_eq!(remedy["tool"], "impact_analysis", "{remedy}");
    assert_eq!(remedy["arguments"]["project"], json!(project.path()));
    assert_eq!(remedy["arguments"]["is_file"], false);
    let candidate = remedy["arguments"]["target"].as_str().unwrap();
    assert_ne!(candidate, "dup", "remedy must qualify the target: {remedy}");
    assert!(
        ambiguous.text.contains(candidate),
        "remedy names one of the listed candidates: {} / {candidate}",
        ambiguous.text
    );

    let result = server.call(
        "recommend_tests",
        json!({"project": project.path(), "file_paths": []}),
    );
    let empty = failure(&result, "recommend_tests");
    assert_eq!(empty.block["error"]["code"], "E_EMPTY_INPUT");
    assert_eq!(empty.block["error"]["remedy"], Value::Null);
    assert!(
        empty.text.contains("at least one file path"),
        "{}",
        empty.text
    );

    let paths: Vec<String> = (0..501).map(|i| format!("src/f{i}.rs")).collect();
    let result = server.call(
        "assess_risk_batch",
        json!({"project": project.path(), "file_paths": paths}),
    );
    let over = failure(&result, "assess_risk_batch");
    assert_eq!(over.block["error"]["code"], "E_OVER_CAP");
    let remedy = &over.block["error"]["remedy"];
    assert_eq!(remedy["tool"], "assess_risk_batch", "{remedy}");
    assert_eq!(remedy["arguments"]["project"], json!(project.path()));
    assert_eq!(
        remedy["arguments"]["file_paths"].as_array().unwrap().len(),
        500,
        "retry is cut to the cap: {remedy}"
    );
    assert!(over.text.contains("at most 500"), "{}", over.text);

    let result = server.call(
        "session_end",
        json!({"project": project.path(), "session_id": "never-started"}),
    );
    let snapshot = failure(&result, "session_end");
    assert_eq!(snapshot.block["error"]["code"], "E_NOT_FOUND");
    assert_eq!(
        snapshot.block["error"]["remedy"],
        json!({"tool": "session_start", "arguments": {
            "project": project.path(), "session_id": "never-started"
        }})
    );
    assert!(snapshot.text.contains("session_start"), "{}", snapshot.text);

    // An unknown feature is a negative answer, not a failure: the tool keeps
    // `found: false` so callers check the flag rather than an error code.
    let result = server.call(
        "feature_bundle",
        json!({"project": project.path(), "feature_id": "absent"}),
    );
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(result["structuredContent"]["found"], false, "{result}");

    // Failures do not poison the session; the next call answers normally.
    let result = server.call(
        "find_symbol",
        json!({"project": project.path(), "name": "only_once"}),
    );
    assert_ne!(result["isError"], true, "{result}");
    assert!(
        result["structuredContent"]["results"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "only_once")),
        "{result}"
    );
}
