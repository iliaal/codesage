use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Session {
    child: Child,
    responses: Receiver<Value>,
    id: u64,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx
                    .send(serde_json::from_str(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            responses,
            id: 0,
        };
        session.request("initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"edit-check-test", "version":"1"}}));
        writeln!(
            session.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .unwrap();
        session
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
            let response = self
                .responses
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }
}

fn run(root: &Path, executable: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(executable)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn mcp_reports_break_before_writing_without_touching_index_or_starting_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = "mod api { pub fn f(a: i32) {} }\nfn caller() { self::api::f(1); }\n";
    std::fs::write(root.join("lib.rs"), source).unwrap();
    run(root, "git", &["init", "-q"]);
    run(root, "git", &["add", "lib.rs"]);
    run(
        root,
        "git",
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    let head = run(root, "git", &["rev-parse", "HEAD"]);
    let git_index = std::fs::read(root.join(".git/index")).unwrap();
    let mut session = Session::start();
    let tools = session.request("tools/list", json!({}));
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "edit_check")
        .unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert!(tool["outputSchema"]["properties"]["incompatible_callers"].is_object());
    for indexed in [false, true] {
        if indexed {
            run(root, env!("CARGO_BIN_EXE_codesage"), &["init"]);
            run(
                root,
                env!("CARGO_BIN_EXE_codesage"),
                &["index", "--no-semantic"],
            );
            std::fs::write(
                root.join("lib.rs"),
                "fn dirty_unique_unindexed_symbol() {}\n",
            )
            .unwrap();
        }
        let before_source = std::fs::read(root.join("lib.rs")).unwrap();
        let before_index = std::fs::read(root.join(".codesage/index.db")).ok();
        let result = session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"f", "replacement":"fn f() {}"}}));
        assert_ne!(result["isError"], true, "{result}");
        let report = &result["structuredContent"];
        assert_eq!(
            report["incompatible_callers"].as_array().unwrap().len(),
            1,
            "{report}"
        );
        assert_eq!(report["arity_changed"], true);
        assert_eq!(report["visibility_changed"], true);
        assert_eq!(report["worktree_matches_head"], !indexed);
        assert_eq!(report["next"], Value::Null);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(std::fs::read(root.join("lib.rs")).unwrap(), before_source);
        assert_eq!(
            std::fs::read(root.join(".codesage/index.db")).ok(),
            before_index
        );
        if !indexed {
            assert!(!root.join(".codesage").exists());
        }
        assert!(!root.join(".codesage/watch.status").exists());
        assert_eq!(run(root, "git", &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), git_index);
    }
    let error = session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"../lib.rs", "symbol_name":"f", "replacement":"fn f() {}"}}));
    assert_eq!(error["isError"], true);
    assert!(error["content"].as_array().unwrap().iter().any(|part| {
        part["text"]
            .as_str()
            .is_some_and(|s| s.contains("\"next\":null"))
    }));
}
