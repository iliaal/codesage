#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use codesage_embed::model::{
    ModelAuthorization, allow_any_model_from_env, resolve_model_artifacts,
};
use serde_json::{Value, json};

const MODEL: &str = "authorization-test/unvalidated";
const CHILD: &str = "CODESAGE_AUTHORIZATION_TEST_CHILD";

struct Session {
    child: Child,
    responses: mpsc::Receiver<String>,
    next: u64,
}

impl Session {
    fn start(cwd: &Path, config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .current_dir(cwd)
            .env("CODESAGE_ALLOW_ANY_MODEL", "1")
            .env("XDG_CONFIG_HOME", config)
            .env("CODESAGE_WATCH", "0")
            .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
        let mut session = Self {
            child,
            responses,
            next: 1,
        };
        let init = session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "authorization-test", "version": "1"}
            }),
        );
        assert!(init.get("result").is_some(), "{init}");
        writeln!(
            session.child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0", "method":"notifications/initialized"
            })
        )
        .unwrap();
        session
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        stdin.flush().unwrap();
        loop {
            let line = self
                .responses
                .recv_timeout(Duration::from_secs(20))
                .expect("MCP response deadline");
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    fn probe(&mut self, project: &Path) -> Value {
        self.request(
            "tools/call",
            json!({"name":"rerank_pairs", "arguments":{
                "project":project, "model":MODEL, "device":"cpu", "query":"", "documents":[]
            }}),
        )
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn requested_project_authorization_ignores_startup_directory_and_revocation() {
    let scratch = tempfile::tempdir().unwrap();
    let listed = scratch.path().join("listed");
    let unlisted = scratch.path().join("unlisted");
    let config = scratch.path().join("config");
    std::fs::create_dir_all(config.join("codesage")).unwrap();
    let allowlist = config.join("codesage/allowed-models");
    for root in [&listed, &unlisted] {
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        codesage_storage::Database::open(&root.join(".codesage/index.db")).unwrap();
        std::fs::write(root.join(".codesage/config.toml"), format!(
            "[embedding]\nmodel = {MODEL:?}\nreranker = {MODEL:?}\ndevice = \"cpu\"\n[index]\nwatch = false\n"
        )).unwrap();
    }
    for startup in [&listed, &unlisted] {
        std::fs::write(
            &allowlist,
            format!("{}\n", listed.canonicalize().unwrap().display()),
        )
        .unwrap();
        let mut session = Session::start(startup, &config);
        for root in [startup, &listed, &unlisted] {
            let response = session.probe(root);
            assert_eq!(
                response["result"]["isError"] == true,
                root == &unlisted,
                "{response}"
            );
        }
        std::fs::write(&allowlist, "").unwrap();
        let response = session.probe(&listed);
        assert_eq!(
            response["result"]["isError"], true,
            "revoked project: {response}"
        );
        let denied = session.request(
            "tools/call",
            json!({"name":"embed_texts", "arguments":{
                "project":unlisted, "model":MODEL, "texts":[]
            }}),
        );
        assert_eq!(denied["result"]["isError"], true, "{denied}");
    }
}

#[test]
fn scoped_artifact_authorization_and_policy_identity() {
    if std::env::var_os(CHILD).is_some() {
        let listed = std::env::var_os("AUTH_LISTED").unwrap();
        let unlisted = std::env::var_os("AUTH_UNLISTED").unwrap();
        let allowlist = std::env::var_os("AUTH_ALLOWLIST").unwrap();
        let listed = Path::new(&listed);
        let unlisted = Path::new(&unlisted);
        let approved = ModelAuthorization::for_project(listed);
        let denied = ModelAuthorization::for_project(unlisted);
        approved.scope(|| {
            assert!(allow_any_model_from_env());
            denied.scope(|| {
                assert!(!allow_any_model_from_env());
                let error = resolve_model_artifacts(MODEL).unwrap_err().to_string();
                assert!(error.contains("validated-model allowlist"), "{error}");
            });
            assert!(allow_any_model_from_env());
        });
        let pinned = "sentence-transformers/all-MiniLM-L6-v2";
        assert_ne!(
            approved.pool_key(pinned).unwrap(),
            denied.pool_key(pinned).unwrap()
        );
        std::fs::write(allowlist, "").unwrap();
        let revoked = ModelAuthorization::for_project(listed);
        assert!(revoked.pool_key(MODEL).is_err());
        assert_ne!(
            approved.pool_key(pinned).unwrap(),
            revoked.pool_key(pinned).unwrap()
        );
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("config");
    let listed = scratch.path().join("listed");
    let unlisted = scratch.path().join("unlisted");
    for root in [&listed, &unlisted] {
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
    }
    std::fs::create_dir_all(config.join("codesage")).unwrap();
    let allowlist = config.join("codesage/allowed-models");
    for cwd in [&listed, &unlisted] {
        std::fs::write(
            &allowlist,
            format!("{}\n", listed.canonicalize().unwrap().display()),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scoped_artifact_authorization_and_policy_identity",
                "--nocapture",
            ])
            .current_dir(cwd)
            .env(CHILD, "1")
            .env("CODESAGE_ALLOW_ANY_MODEL", "1")
            .env("XDG_CONFIG_HOME", &config)
            .env("AUTH_LISTED", &listed)
            .env("AUTH_UNLISTED", &unlisted)
            .env("AUTH_ALLOWLIST", &allowlist)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
