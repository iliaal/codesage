use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use codesage_storage::Database;
use serde_json::{Value, json};

const MODEL: &str = "codesage-test/fingerprint";

fn command(cache: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    command
        .env("HF_HOME", cache)
        .env("XDG_CONFIG_HOME", cache.join("config"))
        .env("CODESAGE_DAEMON_RUNTIME_DIR", cache.join("runtime"))
        .env("HF_HUB_OFFLINE", "1")
        .env("CODESAGE_ALLOW_ANY_MODEL", "1")
        .env("CODESAGE_WATCH", "0")
        .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING");
    command
}

fn provision_model(cache: &Path) {
    let repo = cache.join("hub/models--codesage-test--fingerprint");
    let snapshot = repo.join("snapshots/fixture");
    std::fs::create_dir_all(snapshot.join("onnx")).unwrap();
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    std::fs::write(repo.join("refs/main"), "fixture").unwrap();
    let hex = include_str!("fixtures/fingerprint-model.hex").trim();
    let model: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    std::fs::write(snapshot.join("onnx/model.onnx"), model).unwrap();
    std::fs::write(snapshot.join("onnx/model.onnx_data"), []).unwrap();
    std::fs::write(
        snapshot.join("tokenizer.json"),
        json!({
            "version": "1.0", "truncation": null, "padding": null,
            "added_tokens": [], "normalizer": null,
            "pre_tokenizer": {"type": "Whitespace"},
            "post_processor": null, "decoder": null,
            "model": {"type": "WordLevel", "vocab": {"[UNK]": 1, "needle": 2}, "unk_token": "[UNK]"}
        })
        .to_string(),
    )
    .unwrap();
}

fn index_project(root: &Path, cache: &Path) {
    std::fs::create_dir_all(root.join(".codesage")).unwrap();
    std::fs::create_dir_all(cache.join("config/codesage")).unwrap();
    std::fs::write(
        cache.join("config/codesage/allowed-models"),
        root.canonicalize().unwrap().to_str().unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("needle.rs"), "pub fn needle() -> u32 { 42 }\n").unwrap();
    std::fs::write(
        root.join(".codesage/config.toml"),
        format!("[project]\nname = \"fingerprint-test\"\n[embedding]\nmodel = \"{MODEL}\"\ndevice = \"cpu\"\n"),
    ).unwrap();
    let result = command(cache)
        .args(["index", "--full"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

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
    fn start(cache: &Path, project: &Path) -> Self {
        let mut child = command(cache)
            .args(["mcp", "--direct"])
            .current_dir(project)
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
        let response = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "fingerprint-test", "version": "1"}
            }),
        );
        assert_eq!(response["serverInfo"]["name"], "codesage");
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

    fn search(&mut self, project: &Path) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": "search", "arguments": {"project": project, "query": "needle"}
            }),
        )
    }
}

#[test]
fn mcp_search_rejects_vectors_from_an_incompatible_fingerprint() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let project = temp.path().join("project");
    provision_model(&cache);
    index_project(&project, &cache);
    let db = Database::open_for_model(&project.join(".codesage/index.db"), MODEL, 2).unwrap();
    let original = db
        .semantic_fingerprint()
        .unwrap()
        .expect("index must attest its vectors");
    let mut server = Server::start(&cache, &project);
    let positive = server.search(&project);
    assert_ne!(positive["isError"], true, "{positive}");
    assert_eq!(
        positive["structuredContent"]["results"][0]["file_path"], "needle.rs",
        "{positive}"
    );
    assert!(
        positive["structuredContent"]["_meta"]
            .get("test_override")
            .is_none(),
        "{positive}"
    );

    assert!(original.contains(";pooling=mean;"), "{original}");
    let incompatible = original.replace(";pooling=mean;", ";pooling=cls;");
    db.record_semantic_fingerprint(&incompatible).unwrap();
    let rejected = server.search(&project);
    assert_eq!(
        rejected["isError"], true,
        "incompatible vectors must not reach search: {rejected}"
    );
    let error = rejected["content"].to_string();
    assert!(error.contains("different setup"), "{rejected}");
    assert!(error.contains("codesage index --full"), "{rejected}");
    assert!(rejected.get("structuredContent").is_none(), "{rejected}");

    db.record_semantic_fingerprint(&original).unwrap();
    let restored = server.search(&project);
    assert_ne!(restored["isError"], true, "{restored}");
    assert_eq!(
        restored["structuredContent"]["results"][0]["file_path"], "needle.rs",
        "{restored}"
    );
}
