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
        Self::start_mode(false)
    }

    fn start_mode(legacy: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "0")
            .env("CODESAGE_DIAGNOSTICS", "1")
            .env_remove("CODESAGE_ENVELOPE")
            .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if legacy {
            command.env("CODESAGE_ENVELOPE", "legacy");
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
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "detail-test", "version": "1"}
            }),
        );
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
                .unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn raw_call(&mut self, root: &Path, tool: &str, mut args: Value) -> Value {
        args["project"] = json!(root);
        self.request("tools/call", json!({"name": tool, "arguments": args}))
    }

    fn call(&mut self, root: &Path, tool: &str, args: Value) -> Value {
        let result = self.raw_call(root, tool, args);
        assert_ne!(result["isError"], true, "{result}");
        let value = result["structuredContent"].clone();
        assert_eq!(value["cost"]["bytes"], value.to_string().len(), "{tool}");
        assert!(value["cost"]["ms"].is_u64(), "{tool}");
        value
    }
}

fn command(root: &Path, binary: &str, args: &[&str]) {
    let output = Command::new(binary)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let source = format!(
        "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
        "    leaf();\n".repeat(100)
    );
    std::fs::write(root.join("src/lib.rs"), &source).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='detail_fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    command(root, "git", &["init", "-q"]);
    command(root, "git", &["add", "src/lib.rs", "Cargo.toml"]);
    command(
        root,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    for revision in 1..=3 {
        std::fs::write(
            root.join("src/lib.rs"),
            format!("{source}// revision {revision}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("src/helper.rs"),
            format!("pub fn helper() -> u32 {{ {revision} }}\n"),
        )
        .unwrap();
        command(root, "git", &["add", "src/lib.rs", "src/helper.rs"]);
        command(
            root,
            "git",
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "update",
            ],
        );
    }
    command(root, env!("CARGO_BIN_EXE_codesage"), &["init"]);
    command(
        root,
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    command(
        root,
        env!("CARGO_BIN_EXE_codesage"),
        &["git-index", "--full"],
    );
}

#[test]
fn compact_hundred_reference_response_is_under_four_kib_and_recovers_all_rows() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let standard = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","detail":"standard"}),
    );
    assert_eq!(standard["results"].as_array().unwrap().len(), 100);
    let compact = server.raw_call(root.path(), "find_references", json!({"name":"leaf"}));
    assert_ne!(compact["isError"], true, "{compact}");
    let payload = &compact["structuredContent"];
    assert_eq!(payload["cost"]["detail"], "compact");
    assert!(
        compact.to_string().len() < 4096,
        "{} bytes: {compact}",
        compact.to_string().len()
    );
    assert_eq!(payload["cost"]["bytes"], payload.to_string().len());
    let rows = payload["results"].as_array().unwrap();
    assert!(rows.len() >= 3 && rows.len() < 100, "{payload}");
    assert_eq!(payload["completeness"]["kind"], "truncated");
    assert_eq!(
        payload["completeness"]["recover"]["trimmed"][0]["omitted"],
        100 - rows.len()
    );
    for row in rows {
        assert_eq!(row["from"], "sym:src/lib.rs#caller");
        assert_eq!(row["to"], "sym:src/lib.rs#leaf");
        assert_eq!(row["kind"], "call");
        assert!(row.get("content").is_none());
    }
    assert_eq!(
        &standard["results"].as_array().unwrap()[..rows.len()],
        rows.as_slice()
    );
    let recovery = &payload["completeness"]["recover"];
    let recovered = server.call(
        root.path(),
        recovery["tool"].as_str().unwrap(),
        recovery["arguments"].clone(),
    );
    assert_eq!(recovered["results"], standard["results"]);
}

#[test]
fn detail_aliases_full_snippets_budgets_and_errors_work_over_stdio() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let compact = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"compact"}),
    );
    assert!(compact["results"][0].get("signature").is_none());
    let full = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"full"}),
    );
    assert!(
        full["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("pub fn leaf()")
    );
    assert_eq!(full["results"][0]["snippet_source"], "working_tree");
    for (alias, args, detail) in [
        (
            "assess_risk",
            json!({"target":"src/lib.rs","verbose":true}),
            "full",
        ),
        (
            "assess_risk",
            json!({"target":"src/lib.rs","detail":"full"}),
            "full",
        ),
        (
            "impact_analysis",
            json!({"target":"leaf","summary_only":true}),
            "compact",
        ),
        (
            "impact_analysis",
            json!({"target":"leaf","summary_only":false}),
            "standard",
        ),
    ] {
        let result = server.call(root.path(), alias, args);
        assert_eq!(result["cost"]["detail"], detail);
        if alias == "assess_risk" {
            assert!(result.get("churn_score").is_some(), "{result}");
            assert!(result.get("top_coupled").is_some(), "{result}");
        }
    }
    let budget = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","detail":"standard","budget_tokens":800}),
    );
    assert!(budget.to_string().len() <= 3200, "{budget}");
    assert!(budget["results"].as_array().unwrap().len() >= 3);
    assert_eq!(budget["completeness"]["kind"], "truncated");
    let capped = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","budget_tokens":999999}),
    );
    assert_eq!(capped["cost"]["budget_tokens"], 8000);
    for args in [
        json!({"detail":"invalid"}),
        json!({"budget_tokens":0}),
        json!({"budget_tokens":1.5}),
        json!({"verbose":true,"detail":"compact"}),
    ] {
        let result = server.raw_call(root.path(), "assess_risk", args);
        assert_eq!(result["isError"], true);
        let error = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|block| {
                let value = serde_json::from_str::<Value>(block["text"].as_str()?).ok()?;
                value.get("error").is_some().then_some(value)
            })
            .unwrap();
        assert_eq!(error["error"]["code"], "E_PARAM");
        assert!(error["cost"]["ms"].is_u64());
        assert_eq!(error["cost"]["bytes"], error.to_string().len());
    }
}

#[test]
fn fitting_describe_rows_survive_a_small_requested_budget() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let normal = server.call(
        root.path(),
        "describe",
        json!({
            "target":"src/lib.rs", "detail":"standard", "sections":["symbols"]
        }),
    );
    assert_eq!(
        normal["card"]["sections"]["symbols"]["data"]["top"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        normal.to_string().len() < 1024,
        "fixture must fit smallest budget: {normal}"
    );
    for budget in [256, 400] {
        let bounded = server.call(root.path(), "describe", json!({
            "target":"src/lib.rs", "detail":"standard", "sections":["symbols"], "budget_tokens":budget
        }));
        assert_eq!(bounded["card"], normal["card"]);
        assert_eq!(bounded["completeness"], normal["completeness"], "{bounded}");
        assert!(
            bounded["cost"].get("budget_exceeded").is_none(),
            "{bounded}"
        );
        assert!(bounded.to_string().len() <= budget * 4);
    }
}

#[test]
fn final_reference_budget_keeps_legacy_and_unified_counts_consistent() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(500)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    for budget in [400, 800] {
        let result = server.call(
            root.path(),
            "find_references",
            json!({
                "name":"leaf", "detail":"standard", "budget_tokens":budget
            }),
        );
        let retained = result["results"].as_array().unwrap().len();
        assert!(retained > 0 && retained < 500);
        assert_eq!(result["_meta"]["total_results"], 500);
        assert_eq!(result["_meta"]["returned"], retained, "{result}");
        assert_eq!(
            result["completeness"]["prior"]["recover"]["returned"], retained,
            "{result}"
        );
        assert_eq!(
            result["completeness"]["recover"]["trimmed"][0]["omitted"],
            500 - retained
        );
        assert_eq!(
            result["completeness"]["recover"]["trimmed"][0]["returned"],
            retained
        );
    }
}

#[test]
fn full_snippets_shrink_before_rows_or_evidence_are_dropped() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{ let value = \"{}\"; }}\n",
            "é".repeat(40_000)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let standard = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"standard"}),
    );
    let result = server.call(
        root.path(),
        "find_symbol",
        json!({
            "name":"leaf", "detail":"full", "budget_tokens":800
        }),
    );
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["results"][0]["handle"],
        standard["results"][0]["handle"]
    );
    let snippet = result["results"][0]["snippet"].as_str().unwrap();
    assert!(snippet.starts_with("pub fn leaf() { let value = \"é"));
    assert!(snippet.ends_with("[truncated by MCP budget]"));
    assert!(
        result.to_string().len() <= 3200,
        "{} bytes",
        result.to_string().len()
    );
    assert!(result["cost"].get("budget_exceeded").is_none(), "{result}");
    assert_eq!(result["completeness"]["kind"], "truncated");
}

#[test]
fn irreducible_describe_budget_preserves_symbol_coordinate_tuple() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{\n    let value = 7;\n}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(100)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let result = server.call(
        root.path(),
        "describe",
        json!({
            "target":"sym:src/lib.rs#leaf", "detail":"standard", "budget_tokens":256
        }),
    );
    assert_eq!(result["card"]["handle"], "sym:src/lib.rs#leaf");
    assert_eq!(
        result["card"]["sections"]["identity"]["data"]["lines"],
        json!([1, 3])
    );
    assert_eq!(result["cost"]["budget_exceeded"], true);
}

#[test]
fn first_legacy_budget_cut_discloses_counts_and_retry_without_an_envelope() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start_mode(true);
    let result = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","budget_tokens":256}),
    );
    assert!(result.get("completeness").is_none(), "{result}");
    assert!(result.get("tool").is_none(), "{result}");
    assert!(result.get("index").is_none(), "{result}");
    let returned = result["results"].as_array().unwrap().len();
    assert!(returned > 0 && returned < 100);
    assert_eq!(result["_meta"]["truncated"], true);
    assert_eq!(result["_meta"]["total_results"], 100);
    assert_eq!(result["_meta"]["returned"], returned);
    assert_eq!(result["_meta"]["trimmed"][0]["omitted"], 100 - returned);
    let retry = &result["_meta"]["recover"];
    let recovered = server.call(
        root.path(),
        retry["tool"].as_str().unwrap(),
        retry["arguments"].clone(),
    );
    assert_eq!(recovered["results"].as_array().unwrap().len(), 100);
    assert!(recovered.get("completeness").is_none());
}

#[test]
fn request_diagnostics_include_final_rendering_in_default_and_legacy_modes() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(200)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        let response = server.call(
            root.path(),
            "find_references",
            json!({"name":"leaf","budget_tokens":256}),
        );
        let cost_ms = response["cost"]["ms"].as_u64().unwrap();
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":20}));
        let request = stats["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == "find_references")
            .unwrap();
        let wall_ms = request["wall_ms"].as_u64().unwrap();
        assert!(
            wall_ms + 1 >= cost_ms,
            "legacy={legacy}: request {wall_ms}ms excludes response work measured at {cost_ms}ms"
        );
        assert_eq!(request["outcome"], "success");
        let help = server.call(root.path(), "help", json!({"tool":"find_references"}));
        assert_eq!(
            help["price_list"]["tools"]["find_references"]["request_wall_ms"],
            stats["tools"]["find_references"]["request_wall_ms"]
        );
        let error = server.raw_call(
            root.path(),
            "find_references",
            json!({"name":"leaf","detail":"invalid"}),
        );
        assert_eq!(error["isError"], true);
        let contract = error["content"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|item| {
                let value = serde_json::from_str::<Value>(item["text"].as_str()?).ok()?;
                value.get("error").is_some().then_some(value)
            })
            .unwrap();
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":20}));
        let request = stats["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == "find_references")
            .unwrap();
        assert_eq!(request["outcome"], "error");
        assert!(
            request["wall_ms"].as_u64().unwrap() + 1 >= contract["cost"]["ms"].as_u64().unwrap()
        );
    }
}
