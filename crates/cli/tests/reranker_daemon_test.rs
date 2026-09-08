#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn command(project: &Path, runtime: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codesage"));
    cmd.current_dir(project)
        .env("CODESAGE_DAEMON_RUNTIME_DIR", runtime)
        .env("CODESAGE_WATCH", "0")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null());
    cmd
}

fn checked(mut cmd: Command) -> Output {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires cached pinned Jina and MiniLM ONNX models and an installed ONNX Runtime"]
fn cli_search_and_export_share_one_daemon_reranker_and_match_private_results() {
    let scratch = tempfile::tempdir().unwrap();
    let project = scratch.path().join("project");
    let runtime = scratch.path().join("runtime");
    fs::create_dir_all(project.join(".codesage")).unwrap();
    fs::create_dir(&runtime).unwrap();
    fs::write(project.join(".codesage/config.toml"), "[project]\nname = \"reranker-live-test\"\n[embedding]\nmodel = \"jinaai/jina-embeddings-v2-base-code\"\nreranker = \"cross-encoder/ms-marco-MiniLM-L6-v2\"\ndevice = \"cpu\"\n[index]\nwatch = false\n").unwrap();
    for (path, source) in [
        (
            "auth.rs",
            "pub fn validate_access_token(token: &str) -> bool { token.starts_with(\"Bearer \") && token.len() > 20 }\n",
        ),
        (
            "cache.rs",
            "pub fn read_cached_value(values: &[String], key: usize) -> Option<&str> { values.get(key).map(String::as_str) }\n",
        ),
        (
            "retry.rs",
            "pub fn retry_delay_seconds(attempt: u32) -> u64 { 2u64.saturating_pow(attempt).min(60) }\n",
        ),
        (
            "sum.rs",
            "pub fn sum_numbers(values: &[i64]) -> i64 { values.iter().sum() }\n",
        ),
    ] {
        fs::write(project.join(path), source).unwrap();
    }
    let mut index = command(&project, &runtime);
    index.args(["index", "--no-features"]);
    checked(index);

    let query = "validate an access token";
    let oversized_query = "validate an access token ".repeat(3_000);
    let cases = [
        ("search", query),
        ("export", query),
        ("search", oversized_query.as_str()),
    ];
    let mut private_results = Vec::new();
    for (operation, query) in cases {
        let mut cmd = command(&project, &runtime);
        cmd.args([operation, query, "--json"]);
        let output = checked(cmd);
        let log = String::from_utf8(output.stderr).unwrap();
        assert!(log.contains("reranking privately"), "{log}");
        assert!(log.contains("reranker loaded"), "{log}");
        private_results.push(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap());
    }
    assert!(!private_results[0]["results"].as_array().unwrap().is_empty());
    assert!(!private_results[1]["primary"].as_array().unwrap().is_empty());

    let daemon_log_path = scratch.path().join("daemon.log");
    let log = fs::File::create(&daemon_log_path).unwrap();
    let mut cmd = command(&project, &runtime);
    cmd.arg("daemon")
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    let mut daemon = Daemon(cmd.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            fs::read_to_string(&daemon_log_path).unwrap()
        );
        let mut status = command(&project, &runtime);
        status.args(["daemon", "status"]);
        if status.output().unwrap().status.success() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(25));
    }
    for _ in 0..2 {
        for (index, (operation, query)) in cases.iter().enumerate() {
            let mut cmd = command(&project, &runtime);
            cmd.args([operation, query, "--json"]);
            let output = checked(cmd);
            let log = String::from_utf8(output.stderr).unwrap();
            assert!(
                log.contains("reranking through the running daemon"),
                "{log}"
            );
            if index == 2 {
                assert!(
                    log.contains("reranker input exceeds daemon byte caps; reranking privately"),
                    "{log}"
                );
                assert!(log.contains("reranker loaded"), "{log}");
            } else {
                assert!(
                    !log.contains("reranker loaded") && !log.contains("reranking privately"),
                    "{log}"
                );
            }
            assert!(
                !log.contains("rerank failed") && !log.contains("reranking failed"),
                "{log}"
            );
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                result, private_results[index],
                "{operation} daemon and private outputs differ"
            );
        }
    }
    let daemon_log = fs::read_to_string(&daemon_log_path).unwrap();
    assert_eq!(
        daemon_log.matches("reranker loaded").count(),
        1,
        "{daemon_log}"
    );
    let mut stop = command(&project, &runtime);
    stop.args(["daemon", "stop"]);
    let stop = stop
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon failed to exit after stop"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = stop.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
