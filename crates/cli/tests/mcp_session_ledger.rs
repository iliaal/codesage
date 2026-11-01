#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use codesage_storage::Database;
use serde_json::{Value, json};

const SECRET_QUERY: &str = "zebra quokka passphrase gamma";

#[test]
fn session_ledger_records_handles_only_and_ending_the_session_correlates_edits() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    onboard(root);
    seed_chunk(root, "src/c.rs");

    let runtime = tempfile::tempdir().unwrap();
    let _cleanup = DaemonCleanup(runtime.path().to_path_buf());
    let mut session = McpSession::start(
        runtime.path(),
        &[("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")],
    );
    session.initialize();

    call(
        &mut session,
        2,
        "session",
        json!({"project": root, "action": "start", "session_id": "ledger"}),
    );
    call(
        &mut session,
        3,
        "describe",
        json!({"project": root, "target": "src/a.rs"}),
    );
    call(
        &mut session,
        4,
        "find_symbol",
        json!({"project": root, "target": "gamma"}),
    );
    let search = call(
        &mut session,
        5,
        "search",
        json!({"project": root, "query": SECRET_QUERY, "limit": 3}),
    );
    assert!(
        search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file_path"] == "src/c.rs"),
        "{search}"
    );

    std::fs::write(
        root.join("src/a.rs"),
        "pub fn alpha() -> u32 {\n    10\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn beta() -> u32 {\n    20\n}\n").unwrap();

    call(
        &mut session,
        6,
        "review_rehearsal",
        json!({"project": root, "targets": ["src/a.rs", "src/b.rs"]}),
    );
    call(
        &mut session,
        7,
        "assess_risk",
        json!({"project": root, "targets": ["src/a.rs", "src/b.rs"], "aggregate": true}),
    );
    let end = call(
        &mut session,
        8,
        "session",
        json!({"project": root, "action": "end", "session_id": "ledger"}),
    );

    let raw = std::fs::read_to_string(root.join(".codesage/sessions/ledger.calls.jsonl")).unwrap();
    assert!(!raw.contains(SECRET_QUERY), "{raw}");
    assert!(!raw.contains("quokka"), "{raw}");
    let lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let tools: Vec<&str> = lines
        .iter()
        .map(|line| line["tool"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "describe",
            "find_symbol",
            "search",
            "review_rehearsal",
            "assess_risk:check"
        ],
        "{raw}"
    );
    for line in &lines {
        let mut keys: Vec<&str> = line
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["bytes", "detail", "target_handles", "tool", "ts"],
            "{line}"
        );
        assert!(line["ts"].as_u64().is_some() && line["bytes"].as_u64().unwrap() > 0);
        assert!(
            line["target_handles"]
                .as_array()
                .unwrap()
                .iter()
                .all(|handle| {
                    let handle = handle.as_str().unwrap();
                    handle.starts_with("file:")
                        || handle.starts_with("sym:")
                        || handle.starts_with("chunk:")
                }),
            "{line}"
        );
    }
    assert!(
        lines[2]["target_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| handle.as_str().unwrap().contains("src/c.rs")),
        "{raw}"
    );

    let acted = &end["acted_on"];
    assert_eq!(acted["ledger"], "recorded", "{end}");
    assert_eq!(acted["edited_source"], "worktree", "{end}");
    assert_eq!(acted["fetched_then_edited"], 1, "{end}");
    assert_eq!(
        acted["fetched_then_edited_files"],
        json!(["src/a.rs"]),
        "{end}"
    );
    assert_eq!(acted["edited_unfetched"], 1, "{end}");
    assert_eq!(
        acted["edited_unfetched_files"],
        json!(["src/b.rs"]),
        "{end}"
    );
    assert_eq!(acted["fetched_unedited"], 1, "{end}");
    assert_eq!(acted["calls"], 5, "{end}");
    assert_eq!(acted["checked_edited"], 2, "rehearsal after edits: {end}");
    assert_eq!(acted["checked"], 2, "{end}");
    assert_eq!(acted["per_tool"]["assess_risk:check"]["calls"], 1, "{end}");
    assert_eq!(
        acted["per_tool"]["describe"],
        json!({"calls": 1, "fetched": 1, "fetched_then_edited": 1}),
        "{end}"
    );
    assert_eq!(
        acted["per_tool"]["search"],
        json!({"calls": 1, "fetched": 1, "fetched_then_edited": 0}),
        "{end}"
    );
}

#[test]
fn argument_derived_paths_never_enter_the_ledger() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    onboard(root);
    std::fs::write(root.join("src/new_unindexed.rs"), "pub fn n() {}\n").unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let _cleanup = DaemonCleanup(runtime.path().to_path_buf());
    let mut session = McpSession::start(runtime.path(), &[]);
    session.initialize();
    call(
        &mut session,
        2,
        "session_start",
        json!({"project": root, "session_id": "priv"}),
    );
    let trace = call(
        &mut session,
        3,
        "from_trace",
        json!({"project": root, "trace": "Traceback (most recent call last):\n  File \"app/secret_trace_path.py\", line 3, in handler\n    run()\nValueError: boom\n"}),
    );
    assert!(trace.to_string().contains("secret_trace_path"), "{trace}");
    let deps = call(
        &mut session,
        4,
        "list_dependencies",
        json!({"project": root, "file_path": "src/missing_dep.rs"}),
    );
    assert!(deps.to_string().contains("missing_dep"), "{deps}");
    let tests = call(
        &mut session,
        5,
        "recommend_tests",
        json!({"project": root, "targets": ["src/new_unindexed.rs"]}),
    );
    assert!(tests.to_string().contains("new_unindexed"), "{tests}");

    let raw = std::fs::read_to_string(root.join(".codesage/sessions/priv.calls.jsonl")).unwrap();
    assert_eq!(raw.lines().count(), 3, "{raw}");
    for leaked in ["secret_trace_path", "missing_dep", "new_unindexed"] {
        assert!(
            !raw.contains(leaked),
            "{leaked} leaked into the ledger: {raw}"
        );
    }
}

#[test]
fn fresh_connection_without_session_id_reports_zeros_and_ledger_failures_do_not_fail_calls() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    onboard(root);
    let runtime = tempfile::tempdir().unwrap();
    let _cleanup = DaemonCleanup(runtime.path().to_path_buf());

    let mut first = McpSession::start(runtime.path(), &[]);
    first.initialize();
    call(
        &mut first,
        2,
        "session",
        json!({"project": root, "action": "start"}),
    );
    let mut fresh = McpSession::start(runtime.path(), &[]);
    fresh.initialize();
    let end = call(
        &mut fresh,
        2,
        "session",
        json!({"project": root, "action": "end"}),
    );
    let acted = &end["acted_on"];
    assert_eq!(acted["ledger"], "absent", "{end}");
    for key in [
        "calls",
        "fetched_then_edited",
        "edited_unfetched",
        "fetched_unedited",
        "recommended_tests_edited",
    ] {
        assert_eq!(acted[key], 0, "{key}: {end}");
    }

    call(
        &mut first,
        3,
        "session",
        json!({"project": root, "action": "start", "session_id": "ro"}),
    );
    let sessions = root.join(".codesage/sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = std::fs::write(sessions.join("probe"), b"").is_ok();
    let describe = call(
        &mut first,
        4,
        "describe",
        json!({"project": root, "target": "src/a.rs"}),
    );
    let end = call(
        &mut first,
        5,
        "session",
        json!({"project": root, "action": "end", "session_id": "ro"}),
    );
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !writable,
        "the sessions directory must be unwritable for this check (running as root?)"
    );
    assert_eq!(describe["card"]["handle"], "file:src/a.rs", "{describe}");
    assert!(!sessions.join("ro.calls.jsonl").exists());
    assert_eq!(end["acted_on"]["ledger"], "absent", "{end}");
}

#[test]
fn disabled_ledger_writes_nothing_and_reports_disabled() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    onboard(root);
    let runtime = tempfile::tempdir().unwrap();
    let _cleanup = DaemonCleanup(runtime.path().to_path_buf());
    std::fs::write(
        root.join("src/c.rs"),
        "pub fn gamma() -> u32 {\n    30\n}\n",
    )
    .unwrap();
    let mut session = McpSession::start(runtime.path(), &[("CODESAGE_SESSION_LEDGER", "0")]);
    session.initialize();
    call(
        &mut session,
        2,
        "describe",
        json!({"project": root, "target": "src/a.rs"}),
    );
    call(
        &mut session,
        3,
        "session",
        json!({"project": root, "action": "start", "session_id": "off"}),
    );
    call(
        &mut session,
        4,
        "describe",
        json!({"project": root, "target": "src/a.rs"}),
    );
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn alpha() -> u32 {\n    10\n}\n",
    )
    .unwrap();
    let end = call(
        &mut session,
        5,
        "session",
        json!({"project": root, "action": "end", "session_id": "off"}),
    );
    assert_eq!(end["acted_on"]["ledger"], "disabled", "{end}");
    assert_eq!(end["acted_on"]["edited_source"], "disabled", "{end}");
    assert_eq!(end["acted_on"]["edited_unfetched"], 0, "{end}");
    let snapshot: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".codesage/sessions/off.json")).unwrap(),
    )
    .unwrap();
    assert!(snapshot.get("worktree").is_none(), "{snapshot}");
    let sessions = root.join(".codesage/sessions");
    assert!(!sessions.join("connections").exists());
    assert!(!sessions.join("off.calls.jsonl").exists());
}

fn onboard(root: &Path) {
    let write = |rel: &str, body: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    write("src/a.rs", "pub fn alpha() -> u32 {\n    1\n}\n");
    write("src/b.rs", "pub fn beta() -> u32 {\n    2\n}\n");
    write("src/c.rs", "pub fn gamma() -> u32 {\n    3\n}\n");
    git(root, &["init", "-q", "."]);
    git(root, &["add", "src"]);
    git(root, &["commit", "-qm", "initial"]);
    for args in [["init"].as_slice(), ["index", "--no-semantic"].as_slice()] {
        let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "codesage {args:?}: {out:?}");
    }
}

fn seed_chunk(root: &Path, rel: &str) {
    let db = Database::open_for_model(
        &root.join(".codesage/index.db"),
        "jinaai/jina-embeddings-v2-base-code",
        4,
    )
    .unwrap();
    let body = std::fs::read_to_string(root.join(rel)).unwrap();
    let lines = body.lines().count() as u32;
    db.insert_chunks(
        rel,
        "rust",
        &[(body.as_str(), 1, lines, [0.1_f32, 0.2, 0.3, 0.4].as_slice())],
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=fixture@codesage.test",
            "-c",
            "user.name=fixture",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn call(session: &mut McpSession, id: u64, tool: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    });
    let response = session.request(id, &request.to_string());
    assert!(response.get("error").is_none(), "{tool}: {response}");
    assert_ne!(
        response["result"]["isError"],
        Value::Bool(true),
        "{tool}: {response}"
    );
    response["result"]["structuredContent"].clone()
}

struct McpSession {
    child: Child,
    rx: Receiver<std::io::Result<String>>,
}

impl McpSession {
    fn start(runtime_dir: &Path, envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .env("CODESAGE_WATCH", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, rx }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, id: u64, line: &str) -> Value {
        self.send(line);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for MCP response");
            let line = self.rx.recv_timeout(remaining).unwrap().unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    fn initialize(&mut self) {
        let init = self.request(
            1,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"codesage-test","version":"0.0.0"}}}"#,
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "codesage");
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct DaemonCleanup(PathBuf);

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let pid = contents.trim().parse::<i32>().ok().or_else(|| {
                contents.lines().find_map(|line| {
                    let (key, value) = line.split_once('=')?;
                    (key.trim() == "pid")
                        .then(|| value.trim().parse::<i32>().ok())
                        .flatten()
                })
            });
            if let Some(pid) = pid.filter(|pid| *pid > 0) {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
        }
    }
}
