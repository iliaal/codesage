#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use codesage_storage::Database;
use serde_json::{Value, json};

struct Session {
    child: Child,
    responses: Receiver<Value>,
    log: PathBuf,
}

impl Session {
    fn new(root: &Path) -> Self {
        let log = root.join("mcp.log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .current_dir(root)
            .env("CODESAGE_WATCH", "1")
            .env("REINDEX_DEBOUNCE", "1000")
            .env("CODESAGE_WATCH_IDLE_SECS", "0")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = serde_json::from_str(&line.unwrap()).unwrap();
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            responses,
            log,
        };
        session.send(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"watch-config-test","version":"1"}
            }}),
        );
        session.receive(1);
        session.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        session
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn receive(&self, id: u64) {
        let response = self
            .responses
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        assert_eq!(response["id"], id, "{response}");
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
    }

    fn query(&mut self, root: &Path, id: u64) {
        self.send(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                "name":"find_symbol","arguments":{"project":root,"name":"initial"}
            }}),
        );
    }

    fn wait(&self, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "watcher did not converge; enabled control and host backpressure must be checked:\n{}",
                fs::read_to_string(&self.log).unwrap()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn starts(&self) -> usize {
        fs::read_to_string(&self.log)
            .unwrap()
            .matches("live watcher started")
            .count()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn configure(root: &Path, enabled: Option<bool>, exclude: &[&str]) {
    thread::sleep(Duration::from_millis(20));
    let watch = enabled
        .map(|enabled| format!("watch = {enabled}\n"))
        .unwrap_or_default();
    fs::write(root.join(".codesage/config.toml"), format!(
        "[embedding]\nmodel = \"\"\ndevice = \"cpu\"\n[index]\n{watch}exclude_patterns = {exclude:?}\n"
    )).unwrap();
}

#[test]
fn live_config_disables_reenables_and_reconciles_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".codesage")).unwrap();
    fs::write(root.join("live.rs"), "fn initial() {}\n").unwrap();
    fs::write(root.join("excluded.rs"), "fn initially_excluded() {}\n").unwrap();
    configure(root, Some(true), &["**/excluded.rs"]);
    let db = Database::open(&root.join(".codesage/index.db")).unwrap();
    let mut session = Session::new(root);
    session.query(root, 2);
    session.receive(2);
    session.wait(|| db.symbol_exists("initial").unwrap());
    fs::write(root.join("live.rs"), "fn enabled_control() {}\n").unwrap();
    session.wait(|| db.symbol_exists("enabled_control").unwrap());
    assert!(!db.symbol_exists("initially_excluded").unwrap());
    assert_eq!(session.starts(), 1);

    configure(root, Some(false), &["**/excluded.rs"]);
    session.query(root, 3);
    session.receive(3);
    fs::write(root.join("live.rs"), "fn forbidden_after_disable() {}\n").unwrap();
    thread::sleep(Duration::from_secs(4));
    assert!(!db.symbol_exists("forbidden_after_disable").unwrap());
    assert!(db.symbol_exists("enabled_control").unwrap());

    configure(root, Some(true), &["**/excluded.rs"]);
    for id in 10..26 {
        session.query(root, id);
    }
    let mut ids = std::collections::HashSet::new();
    for _ in 10..26 {
        let response = session
            .responses
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        ids.insert(response["id"].as_u64().unwrap());
    }
    assert_eq!(ids, (10..26).collect());
    session.wait(|| db.symbol_exists("forbidden_after_disable").unwrap());
    assert_eq!(session.starts(), 2);

    configure(root, None, &["**/live.rs"]);
    session.query(root, 30);
    session.receive(30);
    session.wait(|| {
        db.symbol_exists("initially_excluded").unwrap()
            && !db.symbol_exists("forbidden_after_disable").unwrap()
    });
    fs::write(root.join("live.rs"), "fn forbidden_excluded_edit() {}\n").unwrap();
    fs::write(root.join("excluded.rs"), "fn newly_included_edit() {}\n").unwrap();
    session.wait(|| db.symbol_exists("newly_included_edit").unwrap());
    assert!(!db.symbol_exists("forbidden_excluded_edit").unwrap());
    assert_eq!(session.starts(), 3);
}
