//! One target grammar, one argument name. Every tool that names an entity
//! takes `target` (or `targets`), resolves a handle to the entity it names,
//! and refuses a spelling that names nothing with the resolver's leads. The
//! calls are made exactly as an agent would, through `tools/call` on the
//! stdio server.

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
                "clientInfo": {"name": "mcp-target-alias-test", "version": "1"}
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

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
        assert_ne!(result["isError"], json!(true), "{tool} failed: {result}");
        result["structuredContent"].clone()
    }
}

/// The failed result's contract block.
fn failure(result: &Value, tool: &str) -> Value {
    assert_eq!(result["isError"], true, "{tool} must fail: {result}");
    let block = result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| value.get("status").is_some())
        .unwrap_or_else(|| panic!("{tool} must carry a contract block: {result}"));
    assert_eq!(block["tool"], tool, "{block}");
    block
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

/// Two `search` definitions in different modules, plus two files that share
/// the basename `mod.rs` so a bare suffix names several files.
fn onboard(root: &Path) {
    std::fs::create_dir_all(root.join("src/index")).unwrap();
    std::fs::create_dir_all(root.join("src/store")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod search;\npub mod index;\npub mod store;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/search.rs"),
        "use crate::index;\n\npub fn search(q: &str) -> usize {\n    index::search(q)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/index/mod.rs"),
        "pub fn search(q: &str) -> usize {\n    q.len()\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/store/mod.rs"),
        "pub fn put(v: usize) -> usize {\n    v\n}\n",
    )
    .unwrap();
    run(root, &["init"]);
    run(root, &["index", "--no-semantic", "--no-features"]);
}

#[test]
fn target_names_the_entity_across_symbol_and_file_tools() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let mut server = Server::start();
    let handle = "sym:src/search.rs#search";

    // A symbol handle on `target` names one definition, where the bare name
    // names two.
    let found = server.ok(
        "find_symbol",
        json!({"project": project.path(), "target": handle}),
    );
    let rows = found["results"].as_array().expect("results array");
    assert_eq!(rows.len(), 1, "one handle, one definition: {found}");
    assert_eq!(rows[0]["handle"], handle, "{found}");
    assert_eq!(rows[0]["file_path"], "src/search.rs", "{found}");
    assert_eq!(found["target"]["ambiguous"], false, "{found}");

    let by_name = server.ok(
        "find_symbol",
        json!({"project": project.path(), "name": "search"}),
    );
    assert_eq!(
        by_name["results"].as_array().unwrap().len(),
        2,
        "the bare name is the union the handle splits: {by_name}"
    );

    // A file-grained tool takes the same symbol handle and answers for the
    // file the definition lives in.
    let risk = server.ok(
        "assess_risk",
        json!({"project": project.path(), "target": handle}),
    );
    assert_eq!(risk["file"], "src/search.rs", "{risk}");
    assert_eq!(risk["handle"], "file:src/search.rs", "{risk}");

    // `file:` handle, `path:line`, and a `./`-prefixed path are the same file.
    for target in [
        json!("file:src/search.rs"),
        json!("src/search.rs:3"),
        json!("./src/search.rs"),
    ] {
        let deps = server.ok(
            "list_dependencies",
            json!({"project": project.path(), "target": target}),
        );
        assert_eq!(deps["file_path"], "src/search.rs", "{target}: {deps}");
        assert_eq!(deps["found"], true, "{target}: {deps}");
    }

    // The legacy spelling still works and means the same thing.
    let legacy = server.ok(
        "assess_risk",
        json!({"project": project.path(), "file_path": "src/search.rs"}),
    );
    assert_eq!(legacy["file"], risk["file"], "{legacy}");

    // A file set takes `targets`, resolving each entry the same way.
    let tests = server.ok(
        "recommend_tests",
        json!({"project": project.path(), "targets": [handle, "file:src/index/mod.rs"]}),
    );
    assert!(
        tests
            .get("unindexed_files")
            .is_none_or(|v| v.as_array().is_none_or(|rows| rows.is_empty())),
        "handles resolve to indexed paths: {tests}"
    );

    // A path the index does not hold still reaches the tool's own
    // disclosure rather than failing the whole set.
    let tests = server.ok(
        "recommend_tests",
        json!({"project": project.path(), "targets": ["src/brand_new.rs"]}),
    );
    assert_eq!(
        tests["unindexed_files"],
        json!(["src/brand_new.rs"]),
        "a new file is named, not refused: {tests}"
    );
}

/// A file the caller just wrote is the one thing the nearest-candidate scan
/// must not swallow: its path suffix-matches indexed files it has nothing to
/// do with, and naming those would answer about the wrong file.
#[test]
fn a_file_the_working_tree_holds_is_answered_for_rather_than_refused() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    // Unindexed, and `lib.rs` is the suffix of the indexed `src/lib.rs`.
    std::fs::write(project.path().join("lib.rs"), "pub fn fresh() {}\n").unwrap();
    let mut server = Server::start();

    let risk = server.ok(
        "assess_risk",
        json!({"project": project.path(), "target": "lib.rs"}),
    );
    assert_eq!(risk["file"], "lib.rs", "the file asked about: {risk}");
    assert_eq!(
        risk["unscored"], true,
        "a new file is unscored, not a miss: {risk}"
    );
    assert_eq!(risk["found"], false, "{risk}");

    // The same tool still refuses a spelling nothing on disk carries, with
    // the indexed files it might have meant.
    let result = server.call(
        "assess_risk",
        json!({"project": project.path(), "target": "mod.rs"}),
    );
    let block = failure(&result, "assess_risk");
    assert_eq!(block["error"]["code"], "E_NOT_FOUND", "{block}");
    let candidates = block["error"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("candidates block: {block}"));
    assert!(
        candidates.contains(&json!("file:src/index/mod.rs"))
            && candidates.contains(&json!("file:src/store/mod.rs")),
        "the misspelling still gets told where the files are: {candidates:?}"
    );
}

/// The resolver's exact match drops only a leading `./`; a `.` segment or a
/// doubled separator inside the path must still name the indexed file
/// rather than pass the working-tree check and read as unindexed.
#[test]
fn a_path_spelled_with_dot_segments_or_doubled_separators_names_the_indexed_file() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let mut server = Server::start();

    for target in ["src/./search.rs", "src//search.rs", "./src/.//search.rs"] {
        let risk = server.ok(
            "assess_risk",
            json!({"project": project.path(), "target": target}),
        );
        assert_eq!(risk["file"], "src/search.rs", "{target}: {risk}");
        assert_eq!(risk["handle"], "file:src/search.rs", "{target}: {risk}");
        assert_eq!(
            risk["found"], true,
            "{target} names an indexed file: {risk}"
        );

        let tests = server.ok(
            "recommend_tests",
            json!({"project": project.path(), "targets": [target]}),
        );
        assert!(
            tests
                .get("unindexed_files")
                .is_none_or(|v| v.as_array().is_none_or(|rows| rows.is_empty())),
            "{target} is the indexed src/search.rs, not a new file: {tests}"
        );
    }

    // A `..` segment is never folded into an indexed spelling: the input
    // stands as written and reads as unindexed.
    let risk = server.ok(
        "assess_risk",
        json!({"project": project.path(), "target": "src/../src/search.rs"}),
    );
    assert_eq!(risk["file"], "src/../src/search.rs", "{risk}");
    assert_eq!(risk["found"], false, "{risk}");
}

#[test]
fn a_target_that_names_no_indexed_file_is_refused_with_its_candidates() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let mut server = Server::start();

    // `mod.rs` suffix-matches two indexed files. A guess is not an answer:
    // the refusal names both so the caller can pick one.
    let result = server.call(
        "list_dependencies",
        json!({"project": project.path(), "target": "mod.rs"}),
    );
    let block = failure(&result, "list_dependencies");
    assert_eq!(block["error"]["code"], "E_NOT_FOUND", "{block}");
    let candidates = block["error"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("candidates block: {block}"));
    assert!(
        candidates.contains(&json!("file:src/index/mod.rs"))
            && candidates.contains(&json!("file:src/store/mod.rs")),
        "both suffix matches are addressable: {candidates:?}"
    );

    // Each candidate the refusal offered resolves on retry.
    let deps = server.ok(
        "list_dependencies",
        json!({"project": project.path(), "target": "file:src/index/mod.rs"}),
    );
    assert_eq!(deps["file_path"], "src/index/mod.rs", "{deps}");

    // A handle for a definition that does not exist is a miss, not a file.
    let result = server.call(
        "assess_risk",
        json!({"project": project.path(), "target": "sym:src/search.rs#gone"}),
    );
    assert_eq!(
        failure(&result, "assess_risk")["error"]["code"],
        "E_NOT_FOUND"
    );

    // A handle of a kind no file-grained tool can answer for is a parameter
    // error naming what it takes.
    let result = server.call(
        "find_coupling",
        json!({"project": project.path(), "target": "dir:src"}),
    );
    let block = failure(&result, "find_coupling");
    assert_eq!(block["error"]["code"], "E_PARAM", "{block}");
    assert!(
        block["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("`file:`") && m.contains("`sym:`")),
        "{block}"
    );
}

#[test]
fn a_legacy_argument_and_target_that_disagree_are_a_parameter_error() {
    let project = tempfile::tempdir().unwrap();
    onboard(project.path());
    let mut server = Server::start();

    let result = server.call(
        "assess_risk",
        json!({
            "project": project.path(),
            "file_path": "src/search.rs",
            "target": "src/index/mod.rs"
        }),
    );
    let block = failure(&result, "assess_risk");
    assert_eq!(block["error"]["code"], "E_PARAM", "{block}");
    let message = block["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("src/search.rs") && message.contains("src/index/mod.rs"),
        "the refusal quotes both spellings: {message}"
    );

    // The same value in both is the caller repeating itself, not a conflict.
    let risk = server.ok(
        "assess_risk",
        json!({
            "project": project.path(),
            "file_path": "src/search.rs",
            "target": "src/search.rs"
        }),
    );
    assert_eq!(risk["file"], "src/search.rs", "{risk}");

    // Neither is a parameter error naming the preferred spelling.
    let result = server.call("assess_risk", json!({"project": project.path()}));
    let block = failure(&result, "assess_risk");
    assert_eq!(block["error"]["code"], "E_PARAM", "{block}");
    assert!(
        block["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("`target`")),
        "{block}"
    );
}
