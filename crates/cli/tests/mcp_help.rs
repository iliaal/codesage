use std::io::{BufRead, BufReader, Write};
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
            .env("CODESAGE_DIAGNOSTICS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
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
                "clientInfo": {"name": "help-test", "version": "1"}
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
                .expect("stdio reply within deadline");
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

fn success(response: Value) -> Value {
    assert_ne!(response["isError"], true, "{response}");
    response["structuredContent"].clone()
}

fn code(response: &Value) -> String {
    assert_eq!(response["isError"], true, "{response}");
    response["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| serde_json::from_str::<Value>(block["text"].as_str()?).ok())
        .find_map(|block| block["error"]["code"].as_str().map(str::to_owned))
        .expect("typed failure code")
}

#[test]
fn stdio_help_covers_advertised_tools_fields_errors_recipes_and_measured_prices() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().to_str().unwrap();
    let mut server = Server::start();
    let listing = server.request("tools/list", json!({}));
    let tools = listing["tools"].as_array().unwrap();
    let help = tools
        .iter()
        .find(|tool| tool["name"] == "help")
        .expect("help is advertised");
    assert_eq!(help["annotations"]["readOnlyHint"], true);
    for field in ["project", "tool", "field", "code", "intent"] {
        assert!(help["inputSchema"]["properties"].get(field).is_some());
    }
    let catalog = success(server.call("help", json!({"project": root})));
    for tool in tools {
        assert!(
            catalog["catalog"]["tools"]
                .as_array()
                .unwrap()
                .contains(&tool["name"])
        );
        let response = success(server.call("help", json!({"project": root, "tool": tool["name"]})));
        assert_eq!(response["subject"]["name"], tool["name"]);
        assert_eq!(response["subject"]["description"], tool["description"]);
        assert_eq!(response["subject"]["input_schema"], tool["inputSchema"]);
    }
    for intent in [
        "review",
        "fix_bug",
        "rename",
        "add_feature",
        "before_commit",
        "debug_failure",
    ] {
        let response = success(server.call("help", json!({"project": root, "intent": intent})));
        assert_eq!(response["recipe"]["intent"], intent);
        assert_eq!(response["recipe"]["execution"]["mode"], "plan_only");
        assert!(response["recipe"]["steps"].as_array().unwrap().len() >= 3);
    }
    let field = success(server.call(
        "help",
        json!({"project": root, "tool": "search", "field": "output.results[].score"}),
    ));
    assert_eq!(field["field"]["schema"]["type"], "number");
    assert!(
        field["field"]["description"]
            .as_str()
            .unwrap()
            .contains("Final ranking score")
    );
    let trace = success(server.call(
        "help",
        json!({"project": root, "tool": "search", "field": "output.results[].trace[].stage"}),
    ));
    assert_eq!(trace["field"]["schema"]["type"], "string");
    let detail = success(server.call(
        "help",
        json!({"project": root, "tool": "describe", "field": "input.detail"}),
    ));
    assert_eq!(detail["field"]["schema"]["default"], "compact");
    let field = success(server.call(
        "help",
        json!({"project": root, "field": "index.files_behind_bounded"}),
    ));
    assert!(
        field["field"]["description"]
            .as_str()
            .unwrap()
            .contains("lower bound")
    );
    let failure_help =
        success(server.call("help", json!({"project": root, "code": "E_NOT_ONBOARDED"})));
    assert!(
        failure_help["error_help"]["recovery"]
            .as_str()
            .unwrap()
            .contains("codesage init")
    );
    let daemon = success(server.call("help", json!({"project": root, "tool": "daemon"})));
    let call = &daemon["subject"]["tools"]["daemon_stats"]["call"];
    let stats = success(server.call(call["tool"].as_str().unwrap(), call["arguments"].clone()));
    let price = success(server.call("help", json!({"project": root})));
    assert_eq!(price["price_list"]["tools"]["help"]["status"], "measured");
    assert_eq!(
        price["price_list"]["tools"]["help"]["request_wall_ms"],
        stats["tools"]["help"]["request_wall_ms"]
    );
    assert_eq!(price["price_list"]["tools"]["search"]["status"], "unknown");
    assert_eq!(
        std::fs::read_dir(project.path()).unwrap().count(),
        0,
        "help must not onboard or write into a bare project"
    );
}

#[test]
fn stdio_help_describes_budget_recovery_and_preserved_prior_evidence() {
    let project = tempfile::tempdir().unwrap();
    let mut server = Server::start();
    for (field, expected_type) in [
        ("completeness.prior", "object"),
        ("completeness.prior.recover.command", "string"),
        ("completeness.recover.detail", "string"),
        ("completeness.recover.trimmed", "array"),
        ("completeness.recover.trimmed[].field", "string"),
        ("completeness.recover.trimmed[].returned", "integer"),
        ("completeness.recover.trimmed[].omitted", "integer"),
        ("completeness.recover.trimmed[].omitted_handles", "array"),
        ("completeness.recover.trimmed[].omitted_handles[]", "string"),
        ("completeness.recover.shortened", "array"),
        ("completeness.recover.shortened[]", "string"),
        ("_meta.recover", "object"),
        ("_meta.recover.tool", "string"),
        ("_meta.recover.arguments", "object"),
        ("_meta.trimmed[].field", "string"),
        ("_meta.shortened[]", "string"),
    ] {
        for tool in [None, Some("search")] {
            let mut arguments = json!({"project": project.path(), "field": field});
            if let Some(tool) = tool {
                arguments["tool"] = json!(tool);
            }
            let response = success(server.call("help", arguments));
            assert_eq!(
                response["field"]["schema"]["type"], expected_type,
                "{field}: {response}"
            );
            assert!(
                response["field"]["description"]
                    .as_str()
                    .is_some_and(|description| !description.is_empty()),
                "{field}: {response}"
            );
        }
    }
    let invalid = server.call(
        "help",
        json!({"project": project.path(), "field": "completeness.recover.shortened[].field"}),
    );
    assert_eq!(
        code(&invalid),
        "E_PARAM",
        "shortened entries are JSON-pointer strings, not records"
    );
}

#[test]
fn stdio_help_rejects_bad_selectors_and_accepts_a_broken_index_without_reading_it() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().to_str().unwrap();
    let mut server = Server::start();
    for arguments in [
        json!({"project": root, "tool": "missing"}),
        json!({"project": root, "intent": "missing"}),
        json!({"project": root, "tool": "search", "code": "E_MODEL"}),
        json!({"project": root, "field": "missing"}),
        json!({"project": root, "unknown": true}),
    ] {
        assert_eq!(code(&server.call("help", arguments)), "E_PARAM");
    }
    assert_eq!(
        code(&server.call("help", json!({"project": "relative"}))),
        "E_PROJECT_PATH"
    );
    std::fs::create_dir(project.path().join(".codesage")).unwrap();
    let db = project.path().join(".codesage/index.db");
    std::fs::write(&db, "invalid database sentinel").unwrap();
    let help = success(server.call("help", json!({"project": root, "code": "E_SCHEMA_TOO_NEW"})));
    assert_eq!(help["error_help"]["code"], "E_SCHEMA_TOO_NEW");
    assert_eq!(
        std::fs::read_to_string(db).unwrap(),
        "invalid database sentinel"
    );
    assert_eq!(
        std::fs::read_dir(project.path().join(".codesage"))
            .unwrap()
            .count(),
        1
    );
}
