#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use codesage_protocol::{FileInfo, Language};
use codesage_storage::Database;
use serde_json::Value;

#[test]
fn mcp_shim_starts_daemon_and_lists_tools() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut child = ChildGuard {
        child: Command::new(bin)
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(runtime.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage mcp"),
    };

    let stdout = child.child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"codesage-test","version":"0.0.0"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
    }

    let init = recv_response(&rx, 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "codesage");

    {
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
        stdin.flush().unwrap();
    }

    let tools = recv_response(&rx, 2);
    let tool_names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"list_features"));
    assert!(tool_names.contains(&"search"));

    drop(child.child.stdin.take());
    let _ = child.child.kill();
    let _ = child.child.wait();
    let _ = reader.join();
}

#[test]
fn shim_exits_when_daemon_dies() {
    // Keep stdin open so only daemon EOF can end the shim.
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut child = ChildGuard {
        child: Command::new(bin)
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(&runtime_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage mcp"),
    };

    let stdout = child.child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    {
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"codesage-test","version":"0.0.0"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    let init = recv_response(&rx, 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "codesage");

    kill_daemon(&runtime_dir);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.child.kill();
                    panic!("shim did not exit within 10s of daemon kill");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    let _ = reader.join();
}

#[test]
fn concurrent_shims_share_one_daemon() {
    // Race startup to exercise the shared start lock.
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");

    let spawn_shim = || -> ChildGuard {
        ChildGuard {
            child: Command::new(bin)
                .arg("mcp")
                .arg("--runtime-dir")
                .arg(&runtime_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn codesage mcp"),
        }
    };
    let mut a = spawn_shim();
    let mut b = spawn_shim();

    let init = |child: &mut ChildGuard| -> Value {
        let stdout = child.child.stdout.take().expect("child stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"codesage-test","version":"0.0.0"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
        recv_response(&rx, 1)
    };
    let resp_a = init(&mut a);
    let resp_b = init(&mut b);

    assert_eq!(resp_a["result"]["serverInfo"]["name"], "codesage");
    assert_eq!(resp_b["result"]["serverInfo"]["name"], "codesage");

    let socks: Vec<_> = std::fs::read_dir(&runtime_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
        .collect();
    assert_eq!(
        socks.len(),
        1,
        "expected one shared socket, got {}",
        socks.len()
    );

    drop(a.child.stdin.take());
    drop(b.child.stdin.take());
    let _ = a.child.kill();
    let _ = b.child.kill();
    let _ = a.child.wait();
    let _ = b.child.wait();
}

#[test]
fn silent_client_that_never_initializes_is_dropped() {
    // Connection-idle timing starts after initialization; the handshake
    // timeout must release the active-client count for a silent peer.
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut daemon = ChildGuard {
        child: Command::new(bin)
            .arg("daemon")
            .arg("--runtime-dir")
            .arg(&runtime_dir)
            .env("CODESAGE_CLIENT_IDLE_MAX_SECS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage daemon"),
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    while Instant::now() < deadline && socket.is_none() {
        thread::sleep(Duration::from_millis(50));
        for entry in std::fs::read_dir(&runtime_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("sock") {
                socket = Some(p);
            }
        }
    }
    let socket = socket.expect("daemon should bind a socket");

    let mut stream = UnixStream::connect(&socket).expect("connect to daemon socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();

    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).expect("read should return, not hang");
    assert_eq!(
        n, 0,
        "expected the daemon to drop a client that never completed the handshake"
    );

    let _ = daemon.child.kill();
}

#[test]
fn daemon_cleans_runtime_files_on_sigterm() {
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut daemon = Command::new(bin)
        .arg("daemon")
        .arg("--runtime-dir")
        .arg(&runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codesage daemon");

    // Binding precedes the PID-file write; wait for both to avoid that race.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    while Instant::now() < deadline && (socket.is_none() || pid_file.is_none()) {
        thread::sleep(Duration::from_millis(50));
        for entry in std::fs::read_dir(&runtime_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            match p.extension().and_then(|e| e.to_str()) {
                Some("sock") => socket = Some(p.clone()),
                Some("pid") => pid_file = Some(p.clone()),
                _ => {}
            }
        }
    }
    let socket = socket.expect("daemon socket never appeared");
    let pid_file = pid_file.expect("daemon pid file never appeared");

    let pid = read_daemon_pid_file(&pid_file).expect("read pid file");
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("kill");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match daemon.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = daemon.kill();
                    panic!("daemon did not shut down within 5s of SIGTERM");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }

    assert!(!socket.exists(), "socket file should be removed on SIGTERM");
    assert!(!pid_file.exists(), "pid file should be removed on SIGTERM");
}

#[test]
fn daemon_sigterm_with_parked_client_exits_bounded_and_cleans_up() {
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let mut session = McpSession::start(&runtime_dir);
    session.initialize();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    while Instant::now() < deadline && (socket.is_none() || pid_file.is_none()) {
        thread::sleep(Duration::from_millis(50));
        for entry in std::fs::read_dir(&runtime_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            match p.extension().and_then(|e| e.to_str()) {
                Some("sock") => socket = Some(p.clone()),
                Some("pid") => pid_file = Some(p.clone()),
                _ => {}
            }
        }
    }
    let socket = socket.expect("daemon socket never appeared");
    let pid_file = pid_file.expect("daemon pid file never appeared");

    kill_daemon(&runtime_dir);

    // Allow the 5s connection-drain bound plus scheduling margin.
    let deadline = Instant::now() + Duration::from_secs(9);
    while (socket.exists() || pid_file.exists()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !socket.exists(),
        "socket should be removed within the shutdown drain bound"
    );
    assert!(
        !pid_file.exists(),
        "pid file should be removed within the shutdown drain bound"
    );
}

#[test]
fn daemon_self_exits_after_idle_timeout() {
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut daemon = Command::new(bin)
        .arg("daemon")
        .arg("--runtime-dir")
        .arg(&runtime_dir)
        .env("CODESAGE_DAEMON_IDLE_TIMEOUT_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codesage daemon");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    while Instant::now() < deadline && (socket.is_none() || pid_file.is_none()) {
        thread::sleep(Duration::from_millis(50));
        for entry in std::fs::read_dir(&runtime_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            match p.extension().and_then(|e| e.to_str()) {
                Some("sock") => socket = Some(p.clone()),
                Some("pid") => pid_file = Some(p.clone()),
                _ => {}
            }
        }
    }
    let socket = socket.expect("daemon socket never appeared");
    let pid_file = pid_file.expect("daemon pid file never appeared");

    // The idle timeout is polled at 1s granularity.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match daemon.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = daemon.kill();
                    panic!("daemon did not self-exit within 10s despite a 1s idle timeout");
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }

    assert!(
        !socket.exists(),
        "socket file should be removed on idle exit"
    );
    assert!(
        !pid_file.exists(),
        "pid file should be removed on idle exit"
    );
}

#[test]
fn active_client_survives_past_client_idle_max() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut child = ChildGuard {
        child: Command::new(bin)
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(runtime.path())
            .env("CODESAGE_CLIENT_IDLE_MAX_SECS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage mcp"),
    };

    let stdout = child.child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"codesage-test","version":"0.0.0"}}}}}}"#
        )
        .unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    let init = recv_response(&rx, 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "codesage");

    // Requests stay within the 2s idle limit while the session exceeds it.
    for id in 2u64..8 {
        thread::sleep(Duration::from_millis(800));
        {
            let stdin = child.child.stdin.as_mut().expect("child stdin");
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/list"}}"#
            )
            .unwrap();
            stdin.flush().unwrap();
        }
        let resp = recv_response(&rx, id);
        assert!(
            resp["result"]["tools"].is_array(),
            "tools/list #{id} should still answer past the idle ceiling"
        );
    }

    assert!(
        child.child.try_wait().unwrap().is_none(),
        "active client was dropped despite continuous use within the idle window"
    );

    drop(child.child.stdin.take());
    let _ = child.child.kill();
    let _ = child.child.wait();
    let _ = reader.join();
}

#[test]
fn idle_client_dropped_after_client_idle_max() {
    // Keep stdin open so the daemon-side idle timeout causes the exit.
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let bin = env!("CARGO_BIN_EXE_codesage");
    let mut child = ChildGuard {
        child: Command::new(bin)
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(runtime.path())
            .env("CODESAGE_CLIENT_IDLE_MAX_SECS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage mcp"),
    };

    let stdout = child.child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    {
        let stdin = child.child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"codesage-test","version":"0.0.0"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    let init = recv_response(&rx, 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "codesage");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.child.kill();
                    panic!("idle client was not dropped within 15s despite a 2s ceiling");
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    let _ = reader.join();
}

#[test]
fn status_finds_daemon_in_env_runtime_dir() {
    // Isolate the live child from the operator's daemon; unit tests cover fallback paths.
    let scratch = tempfile::tempdir().unwrap();
    let runtime = scratch.path().join("runtime");
    let xdg = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_codesage");

    let mut daemon = ChildGuard {
        child: Command::new(bin)
            .arg("daemon")
            .env("CODESAGE_DAEMON_RUNTIME_DIR", &runtime)
            .env("UID", "424242")
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codesage daemon"),
    };

    // Status needs the PID file, which is written after the socket binds.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    while Instant::now() < deadline && (socket.is_none() || pid_file.is_none()) {
        thread::sleep(Duration::from_millis(50));
        for entry in std::fs::read_dir(&runtime).into_iter().flatten().flatten() {
            let p = entry.path();
            match p.extension().and_then(|e| e.to_str()) {
                Some("sock") => socket = Some(p.clone()),
                Some("pid") => pid_file = Some(p.clone()),
                _ => {}
            }
        }
    }
    assert!(
        socket.is_some(),
        "daemon never bound a socket under the explicit runtime dir"
    );
    assert!(
        pid_file.is_some(),
        "daemon never wrote a pid file under the explicit runtime dir"
    );

    let status = Command::new(bin)
        .arg("daemon")
        .arg("status")
        .env("CODESAGE_DAEMON_RUNTIME_DIR", &runtime)
        .env("XDG_RUNTIME_DIR", xdg.path())
        .env("USER", "codesage-test-user")
        .env_remove("UID")
        .output()
        .expect("run codesage daemon status");

    let _ = daemon.child.kill();
    let _ = daemon.child.wait();

    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success() && stdout.contains("running"),
        "status should find the runtime-dir daemon; exit={:?} stdout={stdout:?}",
        status.status.code()
    );
}

#[test]
fn tools_call_unknown_tool_returns_jsonrpc_error() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
    );
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("unknown tool must be a JSON-RPC error, got: {resp}"));
    // rmcp reports unknown tools as invalid_params (-32602).
    assert_eq!(err["code"], -32602, "unexpected error shape: {err}");
    let msg = err["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("tool not found"),
        "error should say the tool was not found, got: {msg:?}"
    );
}

#[test]
fn tools_call_find_coupling_rejects_unparseable_limit_with_named_value() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find_coupling","arguments":{"project":"/nonexistent","file_path":"a.rs","limit":"not-a-number"}}}"#,
    );
    // rmcp returns parameter-deserialization failures as tool results.
    assert!(
        resp.get("error").is_none(),
        "param failures come back as tool results, got protocol error: {resp}"
    );
    assert_eq!(
        resp["result"]["isError"],
        Value::Bool(true),
        "unparseable limit must fail the call: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    // Preserve the offending value without pinning the surrounding prose.
    assert!(
        text.contains("not-a-number"),
        "error must quote the offending value, got: {text:?}"
    );
}

#[test]
fn tools_call_find_coupling_coerces_stringy_limit() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"find_coupling","arguments":{{"project":"{}","file_path":"src/lib.rs","limit":"5"}}}}}}"#,
            project.path().display()
        ),
    );
    assert!(
        resp.get("error").is_none(),
        "stringy limit must be coerced, not rejected: {resp}"
    );
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "tool run should succeed on an onboarded project: {resp}"
    );
    assert!(
        resp["result"]["structuredContent"].is_object(),
        "successful call should carry structured content: {resp}"
    );
}

#[test]
fn tools_call_file_list_tools_reject_empty_lists() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    for (id, tool) in [
        (2, "assess_risk_diff"),
        (3, "assess_risk_batch"),
        (4, "recommend_tests"),
    ] {
        let resp = session.request(
            id,
            &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{{"project":"{}","file_paths":[]}}}}}}"#,
                project.path().display()
            ),
        );
        assert!(
            resp.get("error").is_none(),
            "empty list validation should be a tool error, not JSON-RPC error: {resp}"
        );
        assert_eq!(
            resp["result"]["isError"],
            Value::Bool(true),
            "{tool} must reject empty file_paths: {resp}"
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains("at least one file path"),
            "error should tell the agent how to fix the request, got: {text:?}"
        );
    }
}

#[test]
fn tools_call_session_start_returns_summary_not_full_snapshot() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"session_start","arguments":{{"project":"{}","session_id":"wire"}}}}}}"#,
            project.path().display()
        ),
    );

    assert!(resp.get("error").is_none(), "expected tool result: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "session_start should succeed: {resp}"
    );
    let structured = &resp["result"]["structuredContent"];
    assert!(
        structured.get("files").is_none(),
        "MCP response should be compact, not the full SessionSnapshot: {structured}"
    );
    assert_eq!(structured["session_id"], "wire");
    let snapshot_path = structured["snapshot_path"]
        .as_str()
        .unwrap_or_else(|| panic!("snapshot_path missing: {structured}"));
    assert!(
        std::path::Path::new(snapshot_path).exists(),
        "full snapshot should still be persisted at {snapshot_path}"
    );
    let disk: Value =
        serde_json::from_str(&std::fs::read_to_string(snapshot_path).unwrap()).unwrap();
    assert!(
        disk["files"].is_array(),
        "disk snapshot should keep the full file list: {disk}"
    );
}

#[test]
fn tools_call_find_symbol_round_trips_against_structural_index() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"find_symbol","arguments":{{"project":"{}","name":"hello_symbol"}}}}}}"#,
            project.path().display()
        ),
    );
    assert!(resp.get("error").is_none(), "expected a result: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "find_symbol should succeed: {resp}"
    );
    let results = resp["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array: {resp}"));
    assert!(
        results
            .iter()
            .any(|r| r["name"] == "hello_symbol" && r["file_path"] == "src/lib.rs"),
        "structural index should resolve the fixture symbol, got: {results:?}"
    );
}

#[test]
fn tools_call_search_returns_seeded_hits_without_model_download() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());
    seed_search_chunks(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start_with_env(
        runtime.path(),
        &[("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")],
    );
    session.initialize();

    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search","arguments":{{"project":"{}","query":"hello symbol","limit":3}}}}}}"#,
            project.path().display()
        ),
    );

    assert!(resp.get("error").is_none(), "expected a result: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "search should succeed against seeded chunks: {resp}"
    );
    let results = resp["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected search results array: {resp}"));
    assert!(
        results.iter().any(|r| r["file_path"] == "src/lib.rs"
            && r["content"]
                .as_str()
                .is_some_and(|c| c.contains("hello_symbol"))),
        "seeded search hit missing from results: {results:?}"
    );
    assert_eq!(
        resp["result"]["structuredContent"]["_meta"]["test_override"],
        Value::Bool(true),
        "debug-override search must be marked: {resp}"
    );
}

#[test]
fn tools_call_search_round_trips_tool_error_without_protocol_failure() {
    let project = tempfile::tempdir().unwrap();
    onboard_fixture_project(project.path());
    std::fs::write(
        project.path().join(".codesage").join("config.toml"),
        "[project]\nname = \"fixture\"\n\n[embedding]\nmodel = \"not-on/allowlist\"\ndevice = \"cpu\"\n\n[index]\nexclude_patterns = []\n",
    )
    .unwrap();

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search","arguments":{{"project":"{}","query":"hello symbol","limit":3}}}}}}"#,
            project.path().display()
        ),
    );

    assert_eq!(
        resp["id"], 2,
        "error must retain the failed search request ID"
    );
    assert!(
        resp.get("error").is_none(),
        "search tool errors must be rendered as tool results, not JSON-RPC errors: {resp}"
    );
    assert_eq!(
        resp["result"]["isError"],
        Value::Bool(true),
        "unallowlisted model should produce a tool-level error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("not on CodeSage's validated-model allowlist"),
        "expected allowlist error text, got: {text:?}"
    );
    assert!(
        text.contains("resolving model files for \"not-on/allowlist\""),
        "outer model-resolution context must survive alongside the cause: {text:?}"
    );

    let healthy = session.request(
        3,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "find_symbol", "arguments": {
                "project": project.path(), "name": "hello_symbol"
            }}
        })
        .to_string(),
    );
    assert_ne!(healthy["result"]["isError"], true, "{healthy}");
    assert!(
        healthy["result"]["structuredContent"]["results"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "hello_symbol")),
        "a subsequent structural query must still return a real symbol: {healthy}"
    );
}

#[test]
fn malformed_protocol_request_returns_error_with_logged_frame_and_cause() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start_with_env(runtime.path(), &[("RUST_LOG", "debug")]);
    session.initialize();
    let token = format!("malformed-{}", runtime.path().display());
    let frame = serde_json::json!({
        "jsonrpc": "2.0", "id": 72, "method": 47, "params": {"query": token}
    })
    .to_string();
    session.send(&frame);
    let line = session
        .rx
        .recv_timeout(Duration::from_secs(5))
        .expect("malformed frame must receive a protocol error")
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["error"]["code"], -32600, "{response}");
    assert!(response.get("result").is_none(), "{response}");
    let log_path = std::fs::read_dir(runtime.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "log"))
        .expect("daemon log must exist");
    let log = std::fs::read_to_string(log_path).unwrap();
    let diagnostic = log
        .lines()
        .find(|line| line.contains("Failed to parse message receive:") && line.contains(&frame))
        .unwrap_or_else(|| panic!("failed frame context was lost: {log}"));
    assert!(
        diagnostic.contains("data did not match any variant"),
        "transport parsing cause was lost: {diagnostic}"
    );
    let healthy = session.request(3, r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
    assert!(
        healthy["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "search")),
        "the same connection must recover after an invalid frame: {healthy}"
    );
}

#[test]
fn daemon_initialize_write_failure_logs_nested_transport_cause() {
    use std::{net::Shutdown, os::unix::net::UnixStream};

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut healthy = McpSession::start_with_env(runtime.path(), &[("RUST_LOG", "debug")]);
    healthy.initialize();
    let socket = std::fs::read_dir(runtime.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .expect("initialized daemon must have a socket");
    let log_path = socket.with_extension("log");
    let mut failed = UnixStream::connect(&socket).unwrap();
    failed.shutdown(Shutdown::Read).unwrap();
    writeln!(
        failed,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 71, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "diagnostic-failure", "version": "0"}}
        })
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap();
        if let Some(diagnostic) = log
            .lines()
            .find(|line| line.contains("sending initialize response"))
        {
            assert!(
                diagnostic.contains("MCP daemon server error:"),
                "{diagnostic}"
            );
            assert!(diagnostic.contains("Broken pipe"), "{diagnostic}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "transport error was not logged: {log}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let response = healthy.request(2, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert!(
        response["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "search")),
        "a failed peer must leave the healthy connection usable: {response}"
    );
}

/// Require nonempty tool-specific results; shape-only checks accept empty branches.
/// Claude Code displays `structuredContent` when a tool advertises an output schema.
#[test]
fn every_schema_bearing_tool_returns_populated_structured_content() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    seed_fixture_chunks(project.path());
    let root = project.path().display().to_string();

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start_with_env(
        runtime.path(),
        // Match the seeded 4D chunks without downloading a model.
        &[("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")],
    );
    session.initialize();

    let listed = session.request(
        2,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    );
    let advertised: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list must return an array: {listed}"))
        .iter()
        .filter(|t| t.get("outputSchema").is_some())
        .map(|t| t["name"].as_str().expect("tool name").to_string())
        .collect();
    assert!(
        !advertised.is_empty(),
        "no tool advertises an outputSchema: {listed}"
    );

    // A fabricated feature ID would only exercise the empty-bundle branch.
    let features = session.request(
        3,
        &format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"list_features","arguments":{{"project":"{root}"}}}}}}"#
        ),
    );
    let feature_id = features["result"]["structuredContent"]["results"][0]["feature_id"]
        .as_str()
        .unwrap_or_else(|| panic!("fixture Cargo.toml should map a library feature: {features}"))
        .to_string();

    // Preserve order: session_end requires the preceding session_start.
    let calls: Vec<(&str, Value, &[&str])> = vec![
        (
            "edit_check",
            serde_json::json!({"file_path":"src/util.rs", "symbol_name":"shared_value", "replacement":"pub fn shared_value() -> u32 { 8 }"}),
            &[
                "head",
                "before",
                "after",
                "overloads_before",
                "overloads_after",
            ],
        ),
        (
            "project_overview",
            serde_json::json!({}),
            // Nonempty risk rows also verify the fixture's git-history pass.
            &["languages", "file_count", "top_risk_files", "entrypoints"],
        ),
        (
            "review_rehearsal",
            serde_json::json!({"file_paths": ["src/helper.rs"]}),
            &["files", "objections", "summary_notes"],
        ),
        (
            "find_symbol",
            serde_json::json!({"name": "outer_step"}),
            &["results"],
        ),
        (
            "find_references",
            serde_json::json!({"name": "inner_step"}),
            &["results"],
        ),
        (
            "find_similar",
            serde_json::json!({"name": "twin_a"}),
            &["results"],
        ),
        (
            "list_dependencies",
            // Python supplies resolved import edges in both directions.
            serde_json::json!({"file_path": "py/app.py"}),
            &["imports", "imported_by"],
        ),
        (
            "search",
            serde_json::json!({"query": "shared value helper", "limit": 3}),
            &["results"],
        ),
        (
            "trace_call_path",
            serde_json::json!({"from": "outer_step", "to": "inner_step"}),
            &["found", "steps", "length"],
        ),
        (
            "from_trace",
            serde_json::json!({
                "trace": format!(
                    "thread 'main' panicked at src/helper.rs:4:5:\nboom\nstack backtrace:\n   0: fixture::helper::inner_step\n             at {root}/src/helper.rs:4:5\n   1: fixture::outer_step\n             at {root}/src/lib.rs:7:5\n   2: std::rt::lang_start\n             at /rustc/abc/library/std/src/rt.rs:100:5\n"
                )
            }),
            &["frames", "format", "parsed", "resolved"],
        ),
        (
            "impact_analysis",
            serde_json::json!({"target": "inner_step"}),
            &["results"],
        ),
        (
            "export_context",
            serde_json::json!({"target": "outer_step", "is_symbol": true}),
            &["symbol_definitions", "primary"],
        ),
        (
            "find_coupling",
            serde_json::json!({"file_path": "src/helper.rs"}),
            &["found", "coupled", "file_commits"],
        ),
        (
            "assess_risk",
            // top_coupled requires verbose output.
            serde_json::json!({"file_path": "src/helper.rs", "verbose": true}),
            &["found", "score", "notes", "top_coupled", "top_symbols"],
        ),
        (
            "assess_risk_diff",
            serde_json::json!({"file_paths": ["src/helper.rs", "src/util.rs"]}),
            &["files", "max_score", "max_risk_file", "summary_notes"],
        ),
        (
            "assess_risk_batch",
            serde_json::json!({"file_paths": ["src/helper.rs", "src/util.rs"]}),
            &["files"],
        ),
        (
            "recommend_tests",
            serde_json::json!({"file_paths": ["src/helper.rs"]}),
            &["primary", "notes"],
        ),
        ("list_features", serde_json::json!({}), &["results"]),
        (
            "find_feature",
            serde_json::json!({"file_path": "src/helper.rs"}),
            &["results"],
        ),
        (
            "feature_bundle",
            serde_json::json!({"feature_id": feature_id}),
            &["found", "target_description", "primary"],
        ),
        (
            "session_start",
            serde_json::json!({"session_id": "inv"}),
            &["session_id", "file_count", "symbol_count", "snapshot_path"],
        ),
        (
            "session_end",
            serde_json::json!({"session_id": "inv"}),
            &["session_id", "new_files", "summary_notes"],
        ),
    ];

    let covered: Vec<String> = calls.iter().map(|(name, _, _)| name.to_string()).collect();
    let mut missing: Vec<&String> = advertised
        .iter()
        .filter(|name| !covered.contains(name))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "these tools advertise an outputSchema but have no representative call here: {missing:?}"
    );

    for (index, (tool, args, required)) in calls.iter().enumerate() {
        if *tool == "session_end" {
            // An unchanged tree would exercise only an empty session diff.
            std::fs::write(
                project.path().join("src/added_mid_session.rs"),
                "pub fn added_mid_session() -> u32 {\n    3\n}\n",
            )
            .unwrap();
            append_line(
                &project.path().join("src/lib.rs"),
                "pub mod added_mid_session;",
            );
            run_codesage(project.path(), &["index", "--no-semantic"]);
        }

        let id = index as u64 + 10;
        let mut arguments = args.as_object().expect("args object").clone();
        arguments.insert("project".to_string(), Value::String(root.clone()));
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let resp = session.request(id, &request.to_string());

        assert!(
            resp.get("error").is_none(),
            "{tool} must answer with a tool result, got JSON-RPC error: {resp}"
        );
        assert_ne!(
            resp["result"]["isError"],
            Value::Bool(true),
            "{tool} representative call failed: {resp}"
        );

        let structured = resp["result"]["structuredContent"]
            .as_object()
            .unwrap_or_else(|| panic!("{tool} must ship structuredContent as an object: {resp}"));
        let payload_keys: Vec<&String> = structured.keys().filter(|k| *k != "_meta").collect();
        assert!(
            !payload_keys.is_empty(),
            "{tool} shipped empty structuredContent, which Claude Code renders as `{{}}`: {resp}"
        );
        for key in *required {
            let value = structured.get(*key).unwrap_or_else(|| {
                panic!("{tool}: structuredContent has no `{key}`: {structured:?}")
            });
            assert!(
                carries_data(value),
                "{tool}: `{key}` carries no data ({value}) — the fixture is not exercising this \
                 tool's data-bearing branch, so the assertion is vacuous"
            );
        }

        // _meta is added after text rendering and has a separate banner block.
        let content = resp["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool} must ship a content array: {resp}"));
        let text = content
            .last()
            .and_then(|c| c["text"].as_str())
            .unwrap_or_else(|| panic!("{tool} last content block must be text: {resp}"));
        let parsed: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("{tool} text block must be JSON ({e}): {text}"));
        let from_text = parsed
            .as_object()
            .unwrap_or_else(|| panic!("{tool} text block must be a JSON object: {text}"));
        for (key, value) in structured {
            if key == "_meta" {
                continue;
            }
            assert_eq!(
                from_text.get(key),
                Some(value),
                "{tool}: text block disagrees with structuredContent on `{key}`"
            );
        }
        for key in from_text.keys() {
            if key == "_meta" {
                continue;
            }
            assert!(
                structured.contains_key(key),
                "{tool}: `{key}` is in the text block but missing from structuredContent"
            );
        }
    }
}

#[test]
fn warm_overview_reuses_the_cold_ranking_execution() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let cold = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert!(
        !cold["top_risk_files"].as_array().unwrap().is_empty(),
        "fixture must exercise ranking work: {cold}"
    );
    let after_cold = daemon_stats(&mut session, 3, 256);
    assert_eq!(overview_ranking_executions(&after_cold), 1, "{after_cold}");

    let warm = call_mcp_tool(
        &mut session,
        4,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(
        warm, cold,
        "cache reuse must preserve every stable overview field and envelope annotation"
    );
    let after_warm = daemon_stats(&mut session, 5, 256);
    assert_eq!(
        overview_ranking_executions(&after_warm),
        1,
        "a stable warm request must not execute the ranking again: {after_warm}"
    );
    assert_eq!(after_warm["tools"]["project_overview"]["requests"], 2);
    assert_eq!(after_warm["counters"]["request_reuse"]["miss"], 1);
    assert_eq!(after_warm["counters"]["request_reuse"]["hit"], 1);
}

#[test]
fn semantic_search_preserves_cached_ranking_until_semantic_index_mutation() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    seed_fixture_chunks(project.path());
    let db_path = project.path().join(".codesage/index.db");
    let db = Database::open_for_model_existing(&db_path, "jinaai/jina-embeddings-v2-base-code", 4)
        .unwrap();
    let registration = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(
        registration
            .execute(
                "UPDATE semantic_models SET indexed_at = 1 WHERE model = ?1 AND dim = 4",
                ["jinaai/jina-embeddings-v2-base-code"],
            )
            .unwrap(),
        1
    );
    drop(registration);

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start_with_env(
        runtime.path(),
        &[("CODESAGE_MCP_TEST_QUERY_EMBEDDING", "0.1,0.2,0.3,0.4")],
    );
    session.initialize();
    let cold = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert!(!cold["top_risk_files"].as_array().unwrap().is_empty());
    assert_eq!(cold["freshness"]["semantic_indexed_files"], 0);
    let after_cold = daemon_stats(&mut session, 3, 256);
    assert_eq!(overview_ranking_executions(&after_cold), 1, "{after_cold}");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {"project": project.path(), "query": "inner step", "limit": 3},
        },
    });
    let response = session.request(4, &request.to_string());
    assert!(response.get("error").is_none(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    let search = &response["result"]["structuredContent"];
    assert_eq!(search["_meta"]["test_override"], true, "{search}");
    let mut text_payloads: Vec<Value> = response["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str(text).ok())
        .collect();
    assert_eq!(text_payloads.len(), 1, "{response}");
    let mut text_payload = text_payloads.pop().unwrap();
    let text_meta = text_payload
        .as_object_mut()
        .unwrap()
        .entry("_meta")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    assert!(!text_meta.contains_key("test_override"), "{response}");
    text_meta.insert("test_override".to_string(), Value::Bool(true));
    assert_eq!(
        &text_payload, search,
        "debug search payloads may differ only by the structured-only override marker"
    );
    assert!(
        search["results"].as_array().unwrap().iter().any(|row| {
            row["file_path"] == "src/helper.rs"
                && row["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("pub fn inner_step"))
        }),
        "seeded semantic search must return the indexed declaration: {search}"
    );
    let warm = call_mcp_tool(
        &mut session,
        5,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(warm, cold, "semantic reads must preserve overview output");
    let after_search = daemon_stats(&mut session, 6, 256);
    assert_eq!(
        overview_ranking_executions(&after_search),
        1,
        "semantic search must not invalidate the cached ranking: {after_search}"
    );
    assert_eq!(after_search["counters"]["request_reuse"]["miss"], 1);
    assert_eq!(after_search["counters"]["request_reuse"]["hit"], 1);

    let source = std::fs::read(project.path().join("src/lib.rs")).unwrap();
    db.upsert_semantic_file_hash(
        "src/lib.rs",
        &codesage_parser::discover::content_hash(&source),
    )
    .unwrap();
    assert_eq!(db.semantic_file_count().unwrap(), 1);
    let updated = call_mcp_tool(
        &mut session,
        7,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(updated["freshness"]["semantic_indexed_files"], 1);
    assert_eq!(updated["freshness"]["semantic_indexed"], true);
    assert_eq!(updated["top_risk_files"], cold["top_risk_files"]);
    let after_mutation = daemon_stats(&mut session, 8, 256);
    assert_eq!(
        overview_ranking_executions(&after_mutation),
        2,
        "a real semantic index mutation must invalidate the same cached ranking: {after_mutation}"
    );
    assert_eq!(after_mutation["counters"]["request_reuse"]["miss"], 2);
    assert_eq!(after_mutation["counters"]["request_reuse"]["hit"], 1);
}

#[test]
fn session_start_reuses_all_cached_rows_beyond_the_overview_cap() {
    let project = tempfile::tempdir().unwrap();
    for index in 0..55 {
        let path = project.path().join(format!("src/extra_{index}.rs"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("pub fn extra_{index}() -> u32 {{ {index} }}\n"),
        )
        .unwrap();
    }
    onboard_rich_fixture(project.path());
    let uncached = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args([
            "session-start",
            "--session-id",
            "uncached-reference",
            "--json",
        ])
        .current_dir(project.path())
        .output()
        .expect("run uncached session-start reference");
    assert!(
        uncached.status.success(),
        "uncached session-start failed: {}",
        String::from_utf8_lossy(&uncached.stderr)
    );
    let uncached: Value =
        serde_json::from_slice(&uncached.stdout).expect("uncached session-start JSON");
    let reference_rows = uncached["top_risk_files"]
        .as_array()
        .expect("uncached top_risk_files");
    assert_eq!(
        reference_rows.len(),
        50,
        "fixture must exceed the top-50 session baseline: {uncached}"
    );

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let overview = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    let overview_rows = overview["top_risk_files"].as_array().unwrap().len();
    assert_eq!(
        overview_rows, 10,
        "fixture must cross the overview cap: {overview}"
    );
    assert_eq!(
        overview["top_risk_files"].as_array().unwrap(),
        &reference_rows[..overview_rows],
        "overview must expose the exact first ten rows of the uncached ranking"
    );

    let session_start = call_mcp_tool(
        &mut session,
        3,
        "session_start",
        serde_json::json!({"project": project.path(), "session_id": "cached-top-50"}),
    );
    let session_rows = session_start["top_risk_file_count"]
        .as_u64()
        .expect("top_risk_file_count") as usize;
    assert_eq!(
        session_rows,
        reference_rows.len(),
        "session summary must report the complete top-50 ranking: {session_start}"
    );
    let snapshot_path = session_start["snapshot_path"]
        .as_str()
        .expect("snapshot_path");
    let cached_snapshot: Value = serde_json::from_slice(
        &std::fs::read(snapshot_path).expect("read cached daemon session snapshot"),
    )
    .expect("cached daemon session snapshot JSON");
    assert_eq!(
        cached_snapshot["top_risk_files"], uncached["top_risk_files"],
        "daemon session persistence must match uncached top-50 scores and order exactly"
    );
    let stats = daemon_stats(&mut session, 4, 256);
    assert_eq!(
        overview_ranking_executions(&stats),
        1,
        "session_start must reuse the cold overview's top-50 ranking: {stats}"
    );
    assert_eq!(stats["counters"]["request_reuse"]["miss"], 1);
    assert_eq!(stats["counters"]["request_reuse"]["hit"], 1);
}

#[test]
fn hidden_daemon_stats_and_cli_report_the_same_shared_state() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();
    call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );

    let listed = session.request(3, r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "daemon_stats"),
        "operator diagnostics must remain hidden from advertised MCP tools: {listed}"
    );
    let direct = daemon_stats(&mut session, 4, 256);
    assert_eq!(direct["tools"]["project_overview"]["requests"], 1);
    assert_eq!(overview_ranking_executions(&direct), 1);
    assert_eq!(direct["work"]["closed"], false, "{direct}");
    assert_eq!(direct["work"]["requests"], 0, "{direct}");
    assert_eq!(direct["work"]["queued"], serde_json::json!([0, 0, 0]));
    assert_eq!(direct["work"]["running"], serde_json::json!([0, 0, 0]));
    assert!(direct["work"]["limits"].is_object(), "{direct}");

    let cli = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args([
            "daemon",
            "stats",
            "--json",
            "--recent",
            "256",
            "--runtime-dir",
        ])
        .arg(runtime.path())
        .output()
        .expect("run codesage daemon stats");
    assert!(
        cli.status.success(),
        "codesage daemon stats failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_stats: Value = serde_json::from_slice(&cli.stdout).expect("daemon stats JSON");
    assert_eq!(
        cli_stats, direct,
        "the CLI must query the existing daemon rather than a fresh diagnostics state"
    );
}

#[test]
fn runtime_diagnostics_switch_preserves_overview_and_session_results() {
    assert_runtime_toggle_results(false, true);
}

#[test]
fn runtime_cache_switch_recomputes_complete_overview_and_session_results() {
    assert_runtime_toggle_results(true, false);
}

#[test]
fn runtime_combined_switches_preserve_overview_and_session_results() {
    assert_runtime_toggle_results(false, false);
}

fn assert_runtime_toggle_results(diagnostics_enabled: bool, cache_enabled: bool) {
    let project = tempfile::tempdir().unwrap();
    for index in 0..12 {
        let path = project.path().join(format!("src/toggle_{index}.rs"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("pub fn toggle_{index}() -> u32 {{ {index} }}\n"),
        )
        .unwrap();
    }
    onboard_rich_fixture(project.path());
    let baseline_runtime = tempfile::tempdir().unwrap();
    let _baseline_cleanup = DaemonCleanup {
        runtime_dir: baseline_runtime.path().to_path_buf(),
    };
    let mut baseline = McpSession::start_with_env(
        baseline_runtime.path(),
        &[
            ("CODESAGE_DIAGNOSTICS", "1"),
            ("CODESAGE_OVERVIEW_CACHE", "1"),
        ],
    );
    baseline.initialize();
    let reference_overview = call_mcp_tool(
        &mut baseline,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(
        reference_overview["top_risk_files"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    let mut reference_session = call_mcp_tool(
        &mut baseline,
        3,
        "session_start",
        serde_json::json!({"project": project.path(), "session_id": "runtime-toggles"}),
    );
    let snapshot_path = reference_session["snapshot_path"].as_str().unwrap();
    let mut reference_snapshot: Value =
        serde_json::from_slice(&std::fs::read(snapshot_path).unwrap()).unwrap();
    assert!(
        reference_snapshot["top_risk_files"]
            .as_array()
            .unwrap()
            .len()
            > 10
    );
    let reference_stats = daemon_stats(&mut baseline, 4, 256);
    assert_eq!(reference_stats["enabled"], true, "{reference_stats}");
    assert_eq!(reference_stats["overview_cache_enabled"], true);
    assert_eq!(overview_ranking_executions(&reference_stats), 1);
    assert_eq!(reference_stats["counters"]["request_reuse"]["miss"], 1);
    assert_eq!(reference_stats["counters"]["request_reuse"]["hit"], 1);

    let runtime = tempfile::tempdir().unwrap();
    let _cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start_with_env(
        runtime.path(),
        &[
            (
                "CODESAGE_DIAGNOSTICS",
                if diagnostics_enabled { "1" } else { "0" },
            ),
            (
                "CODESAGE_OVERVIEW_CACHE",
                if cache_enabled { "1" } else { "0" },
            ),
        ],
    );
    session.initialize();
    for id in [2, 3] {
        let overview = call_mcp_tool(
            &mut session,
            id,
            "project_overview",
            serde_json::json!({"project": project.path()}),
        );
        assert_eq!(
            overview, reference_overview,
            "runtime switches changed overview output"
        );
    }
    let target_snapshot_path = project
        .path()
        .join(".codesage/sessions/runtime-toggles-disabled.json");
    assert!(!target_snapshot_path.exists());
    let mut started = call_mcp_tool(
        &mut session,
        4,
        "session_start",
        serde_json::json!({"project": project.path(), "session_id": "runtime-toggles-disabled"}),
    );
    assert_eq!(started["session_id"], "runtime-toggles-disabled");
    assert_eq!(
        started["snapshot_path"],
        serde_json::json!(target_snapshot_path)
    );
    assert!(
        started["created_at"].as_i64().unwrap()
            >= reference_session["created_at"].as_i64().unwrap()
    );
    for field in ["session_id", "snapshot_path", "created_at"] {
        started.as_object_mut().unwrap().remove(field);
        reference_session.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(
        started, reference_session,
        "runtime switches changed session summary"
    );
    let mut snapshot: Value =
        serde_json::from_slice(&std::fs::read(&target_snapshot_path).unwrap()).unwrap();
    assert_eq!(snapshot["session_id"], "runtime-toggles-disabled");
    assert!(
        snapshot["created_at"].as_i64().unwrap()
            >= reference_snapshot["created_at"].as_i64().unwrap()
    );
    for field in ["session_id", "created_at"] {
        snapshot.as_object_mut().unwrap().remove(field);
        reference_snapshot.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(
        snapshot, reference_snapshot,
        "runtime switches changed persisted snapshot facts"
    );

    let stats = daemon_stats(&mut session, 5, 256);
    assert_eq!(stats["enabled"], diagnostics_enabled, "{stats}");
    assert_eq!(stats["overview_cache_enabled"], cache_enabled, "{stats}");
    assert_eq!(
        stats["work"], reference_stats["work"],
        "disabling diagnostics must retain the same drained work gauges and admission limits"
    );
    assert_eq!(stats["work"]["closed"], false);
    assert_eq!(stats["work"]["requests"], 0);
    assert_eq!(stats["work"]["queued"], serde_json::json!([0, 0, 0]));
    assert_eq!(stats["work"]["running"], serde_json::json!([0, 0, 0]));
    if diagnostics_enabled {
        assert!(!cache_enabled);
        assert_eq!(stats["tools"]["project_overview"]["requests"], 2);
        assert_eq!(stats["tools"]["project_overview"]["executions"], 4);
        assert_eq!(stats["tools"]["session_start"]["requests"], 1);
        assert_eq!(stats["tools"]["session_start"]["executions"], 2);
        let executions = stats["recent_executions"].as_array().unwrap();
        for (tool, expected) in [("project_overview", 2), ("session_start", 1)] {
            for class in ["interactive", "analysis"] {
                let rows: Vec<_> = executions
                    .iter()
                    .filter(|row| row["tool"] == tool && row["work_class"] == class)
                    .collect();
                assert_eq!(rows.len(), expected, "{tool} {class} work: {stats}");
                let ids: std::collections::HashSet<_> = rows
                    .iter()
                    .map(|row| {
                        assert_eq!(row["outcome"], "success", "{row}");
                        assert_eq!(row["phase"], "running", "{row}");
                        row["id"].as_u64().unwrap()
                    })
                    .collect();
                assert_eq!(ids.len(), expected, "executions must have distinct IDs");
            }
        }
        assert_eq!(overview_ranking_executions(&stats), 0);
        assert_eq!(stats["gauges"]["cache"], serde_json::json!({}));
        for reuse in ["hit", "miss", "shared"] {
            assert!(
                stats["counters"]["request_reuse"].get(reuse).is_none(),
                "cache bypass recorded {reuse}: {stats}"
            );
        }
    } else {
        for field in [
            "counters",
            "gauges",
            "tools",
            "active_requests",
            "active_executions",
            "recent_requests",
            "recent_executions",
            "retention",
        ] {
            assert!(
                stats.get(field).is_none(),
                "disabled diagnostics retained {field}: {stats}"
            );
        }
    }
}

#[test]
fn warm_overview_keeps_git_freshness_and_working_file_annotations_live() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let cold = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(cold["freshness"]["structural_kind"], "fresh", "{cold}");
    let indexed_head = cold["freshness"]["head_sha"]
        .as_str()
        .expect("fresh overview head_sha")
        .to_string();
    let changed = cold["top_risk_files"][0]["file"]
        .as_str()
        .expect("ranked fixture file")
        .to_string();
    append_line(&project.path().join(&changed), "");
    run_git(project.path(), &["add", "--", &changed]);
    run_git(project.path(), &["commit", "-qm", "advance head"]);

    let warm = call_mcp_tool(
        &mut session,
        3,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(warm["freshness"]["indexed_sha"], indexed_head, "{warm}");
    assert_ne!(warm["freshness"]["head_sha"], indexed_head, "{warm}");
    assert_eq!(
        warm["freshness"]["structural_kind"], "behind_head",
        "{warm}"
    );
    assert!(
        warm["_meta"]["stale_files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file.as_str() == Some(&changed))),
        "warm rendering must hash current files after serving cached ranking rows: {warm}"
    );
    let stats = daemon_stats(&mut session, 4, 256);
    assert_eq!(
        overview_ranking_executions(&stats),
        1,
        "Git and working-tree refresh must not invalidate database-derived ranking: {stats}"
    );
}

#[test]
fn structural_database_write_invalidates_the_cached_ranking() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let cold = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    let cold_files = cold["file_count"].as_u64().expect("file_count");
    let body = "pub fn inserted_after_cold_overview() {}\n";
    std::fs::write(project.path().join("src/inserted.rs"), body).unwrap();
    let db = Database::open(&project.path().join(".codesage/index.db")).unwrap();
    db.upsert_file(&FileInfo {
        path: "src/inserted.rs".to_string(),
        language: Language::Rust,
        content_hash: codesage_parser::discover::content_hash(body.as_bytes()),
    })
    .unwrap();
    drop(db);

    let updated = call_mcp_tool(
        &mut session,
        3,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    assert_eq!(updated["file_count"], cold_files + 1, "{updated}");
    let stats = daemon_stats(&mut session, 4, 256);
    assert_eq!(
        overview_ranking_executions(&stats),
        2,
        "a different PRAGMA data_version must force a new physical ranking: {stats}"
    );
    assert_eq!(stats["counters"]["request_reuse"]["miss"], 2);
}

#[test]
fn canonical_project_aliases_share_one_cached_ranking() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("project-alias");
    std::os::unix::fs::symlink(project.path(), &alias).unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    let original = call_mcp_tool(
        &mut session,
        2,
        "project_overview",
        serde_json::json!({"project": project.path()}),
    );
    let through_alias = call_mcp_tool(
        &mut session,
        3,
        "project_overview",
        serde_json::json!({"project": alias}),
    );
    assert_eq!(through_alias["project_root"], original["project_root"]);
    assert_eq!(through_alias["top_risk_files"], original["top_risk_files"]);
    let stats = daemon_stats(&mut session, 4, 256);
    assert_eq!(
        overview_ranking_executions(&stats),
        1,
        "canonical aliases must not allocate independent cache entries: {stats}"
    );
    assert_eq!(stats["counters"]["request_reuse"]["hit"], 1);
}

/// Reject empty branches even when their response shape is valid.
fn carries_data(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::Bool(b) => *b,
        Value::Null => false,
    }
}

/// Use real co-change history so trimming cannot pass on an empty result.
#[test]
fn assess_risk_default_response_omits_verbose_fields() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());
    let root = project.path().display().to_string();

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();

    const VERBOSE_ONLY: [&str; 11] = [
        "churn_score",
        "churn_percentile",
        "fix_ratio",
        "total_commits",
        "fix_count",
        "dependent_files",
        "coupled_files",
        "test_gap",
        "in_cycle",
        "cycle_size",
        "top_coupled",
    ];

    let call = |session: &mut McpSession,
                id: u64,
                tool: &str,
                mut args: serde_json::Map<String, Value>| {
        args.insert("project".to_string(), Value::String(root.clone()));
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        });
        let resp = session.request(id, &request.to_string());
        assert_ne!(
            resp["result"]["isError"],
            Value::Bool(true),
            "{tool} failed: {resp}"
        );
        resp["result"]["structuredContent"].clone()
    };
    let object = |v: Value| v.as_object().cloned().expect("object");

    let default = object(call(
        &mut session,
        2,
        "assess_risk",
        object(serde_json::json!({"file_path": "src/helper.rs"})),
    ));
    assert_eq!(default["found"], Value::Bool(true), "{default:?}");
    assert!(carries_data(&default["score"]), "{default:?}");
    assert!(carries_data(&default["notes"]), "{default:?}");
    for key in VERBOSE_ONLY {
        assert!(
            !default.contains_key(key),
            "assess_risk default response must omit `{key}`: {default:?}"
        );
    }
    let verbose = object(call(
        &mut session,
        3,
        "assess_risk",
        object(serde_json::json!({"file_path": "src/helper.rs", "verbose": true})),
    ));
    for key in VERBOSE_ONLY {
        assert!(
            verbose.contains_key(key),
            "assess_risk verbose response must carry `{key}`: {verbose:?}"
        );
    }
    assert!(
        carries_data(&verbose["top_coupled"]),
        "fixture has co-change history, verbose must surface it: {verbose:?}"
    );
    assert_eq!(
        default["score"], verbose["score"],
        "trim must not change the score"
    );

    let files = serde_json::json!(["src/helper.rs", "src/util.rs"]);
    for (id, tool) in [(4u64, "assess_risk_batch"), (5, "assess_risk_diff")] {
        let default = object(call(
            &mut session,
            id,
            tool,
            object(serde_json::json!({"file_paths": files})),
        ));
        let entries = default["files"].as_array().expect("files array");
        assert!(!entries.is_empty(), "{tool}: {default:?}");
        for entry in entries {
            let entry = entry.as_object().expect("entry object");
            for key in VERBOSE_ONLY {
                assert!(
                    !entry.contains_key(key),
                    "{tool} default entry must omit `{key}`: {entry:?}"
                );
            }
        }
        let verbose = object(call(
            &mut session,
            id + 10,
            tool,
            object(serde_json::json!({"file_paths": files, "verbose": true})),
        ));
        let entries = verbose["files"].as_array().expect("files array");
        assert!(
            entries
                .iter()
                .all(|e| e.get("churn_percentile").is_some() && e.get("test_gap").is_some()),
            "{tool} verbose entries must carry the decomposition: {verbose:?}"
        );
    }
}

/// Require call-site evidence before comparing surfaces; both could omit it.
#[test]
fn trace_call_path_mcp_and_cli_json_agree_on_step_fields() {
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());

    let cli = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["trace", "outer_step", "inner_step", "--json"])
        .current_dir(project.path())
        .output()
        .expect("run codesage trace");
    assert!(
        cli.status.success(),
        "codesage trace failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_report: Value = serde_json::from_slice(&cli.stdout).expect("trace --json output");

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();
    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"trace_call_path","arguments":{{"project":"{}","from":"outer_step","to":"inner_step"}}}}}}"#,
            project.path().display()
        ),
    );
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "trace_call_path failed: {resp}"
    );
    let mcp_report = resp["result"]["structuredContent"].clone();

    // Two empty step arrays would satisfy parity without exercising a hop.
    assert_eq!(cli_report["found"], Value::Bool(true), "CLI: {cli_report}");
    assert_eq!(mcp_report["found"], Value::Bool(true), "MCP: {mcp_report}");

    let cli_steps = cli_report["steps"].as_array().expect("CLI steps");
    let mcp_steps = mcp_report["steps"].as_array().expect("MCP steps");
    assert_eq!(
        cli_steps.len(),
        mcp_steps.len(),
        "step count differs: CLI {cli_report} vs MCP {mcp_report}"
    );
    assert!(
        cli_steps.len() >= 2,
        "need a hop to check call_line evidence: {cli_report}"
    );

    for (surface, steps) in [("CLI", cli_steps), ("MCP", mcp_steps)] {
        for (i, step) in steps.iter().enumerate() {
            for field in ["name", "qualified_name", "file_path", "line_start"] {
                assert!(
                    step.get(field).is_some(),
                    "{surface} step {i} lost `{field}`: {step}"
                );
            }
        }
        assert!(
            steps[0].get("call_line").is_none(),
            "{surface}: the origin step has no caller, so call_line must be omitted: {}",
            steps[0]
        );
        assert!(
            steps[1]["call_line"].is_u64(),
            "{surface}: hop 1 must name the call-site line: {}",
            steps[1]
        );
    }

    for (i, (c, m)) in cli_steps.iter().zip(mcp_steps).enumerate() {
        let ckeys: Vec<&String> = c.as_object().expect("CLI step object").keys().collect();
        let mkeys: Vec<&String> = m.as_object().expect("MCP step object").keys().collect();
        assert_eq!(ckeys, mkeys, "step {i} field sets diverge");
        assert_eq!(c, m, "step {i} values diverge");
    }
    let ckeys: Vec<&String> = cli_report.as_object().unwrap().keys().collect();
    let mkeys: Vec<&String> = mcp_report
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "_meta" && *k != "next")
        .collect();
    assert_eq!(ckeys, mkeys, "top-level field sets diverge");
    assert_eq!(
        mcp_report["next"],
        serde_json::json!({
            "tool": "list_dependencies",
            "arguments": {
                "project": project.path(),
                "file_path": mcp_steps[0]["file_path"]
            }
        })
    );
}

#[test]
fn from_trace_subdirectory_project_still_reads_the_workspace_manifest() {
    // Name-only Rust frames need the root Cargo manifest, even for a subdirectory project.
    let project = tempfile::tempdir().unwrap();
    onboard_rich_fixture(project.path());

    let runtime = tempfile::tempdir().unwrap();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime.path().to_path_buf(),
    };
    let mut session = McpSession::start(runtime.path());
    session.initialize();
    let resp = session.request(
        2,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"from_trace","arguments":{{"project":"{}","trace":"   0: fixture::helper::inner_step\n   1: fixture::outer_step\n"}}}}}}"#,
            project.path().join("src").display()
        ),
    );
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "from_trace failed: {resp}"
    );
    let report = &resp["result"]["structuredContent"];
    assert_eq!(report["format"], "rust", "{report}");
    assert_eq!(report["parsed"], 2, "{report}");
    let frames = report["frames"].as_array().expect("frames");
    assert_eq!(frames[0]["status"], "resolved", "{report}");
    assert_eq!(frames[0]["file"], "src/helper.rs", "{report}");
    assert_eq!(frames[0]["symbol"]["name"], "inner_step", "{report}");
    assert_eq!(frames[1]["status"], "resolved", "{report}");
    assert_eq!(frames[1]["file"], "src/lib.rs", "{report}");
    assert_eq!(report["resolved"], 2, "{report}");
}

/// Supply calls, clones, imports, tests, co-change history, and a mapped slice
/// so each tool's populated branch can be exercised without model downloads.
fn onboard_rich_fixture(root: &std::path::Path) {
    let write = |rel: &str, body: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };

    write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        "src/lib.rs",
        "pub mod helper;\npub mod util;\n\n\
         use crate::helper::inner_step;\n\n\
         pub fn outer_step() -> u32 {\n    inner_step()\n}\n\n\
         pub fn twin_a(n: u32) -> u32 {\n    let mut total = 0;\n    \
         for i in 0..n {\n        total += i * 2;\n    }\n    total\n}\n\n\
         pub fn twin_b(m: u32) -> u32 {\n    let mut sum = 0;\n    \
         for j in 0..m {\n        sum += j * 2;\n    }\n    sum\n}\n",
    );
    write(
        "src/helper.rs",
        "use crate::util::shared_value;\n\npub fn inner_step() -> u32 {\n    shared_value()\n}\n",
    );
    write("src/util.rs", "pub fn shared_value() -> u32 {\n    7\n}\n");
    write(
        "tests/helper_test.rs",
        "use fixture::helper::inner_step;\n\n\
         #[test]\nfn inner_step_returns_seven() {\n    assert_eq!(inner_step(), 7);\n}\n",
    );
    write("py/util.py", "def shared_value():\n    return 7\n");
    write(
        "py/app.py",
        "from util import shared_value\n\n\ndef use_shared():\n    return shared_value()\n",
    );
    write(
        "py/main.py",
        "from app import use_shared\n\n\ndef entry():\n    return use_shared()\n",
    );

    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args([
                "-c",
                "user.email=fixture@codesage.test",
                "-c",
                "user.name=fixture",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "."]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "initial"]);
    // Clear the min-count-3 co-change threshold.
    for rev in 2..=4 {
        for rel in ["src/helper.rs", "src/util.rs"] {
            append_line(&root.join(rel), &format!("// rev {rev}"));
        }
        git(&["commit", "-qam", &format!("rev {rev}")]);
    }

    run_codesage(root, &["init"]);
    run_codesage(root, &["index", "--no-semantic"]);
    run_codesage(root, &["git-index", "--full"]);
}

fn append_line(path: &std::path::Path, line: &str) {
    let mut body = std::fs::read_to_string(path).unwrap();
    body.push_str(line);
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

fn run_codesage(root: &std::path::Path, args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run codesage");
    assert!(
        out.status.success(),
        "codesage {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=fixture@codesage.test",
            "-c",
            "user.name=fixture",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn call_mcp_tool(session: &mut McpSession, id: u64, tool: &str, arguments: Value) -> Value {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    });
    let response = session.request(id, &request.to_string());
    assert!(
        response.get("error").is_none(),
        "{tool} returned a JSON-RPC error: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        Value::Bool(true),
        "{tool} failed: {response}"
    );
    let structured = response["result"]["structuredContent"].clone();
    let content = response["result"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("{tool} result must carry text content: {response}"));
    let mut json_blocks: Vec<Value> = content
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str(text).ok())
        .collect();
    assert_eq!(
        json_blocks.len(),
        1,
        "{tool} must carry exactly one JSON payload text block alongside any banners: {response}"
    );
    let text_payload = json_blocks.pop().unwrap();
    assert_eq!(
        text_payload, structured,
        "{tool} JSON text payload must match structuredContent exactly"
    );
    if structured["_meta"]["stale_files"].is_array() {
        assert!(
            content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .any(|text| serde_json::from_str::<Value>(text).is_err()
                    && text.contains("changed on disk")),
            "{tool} stale-file metadata must retain its user-visible warning banner: {response}"
        );
    }
    structured
}

fn daemon_stats(session: &mut McpSession, id: u64, recent: usize) -> Value {
    call_mcp_tool(
        session,
        id,
        "daemon_stats",
        serde_json::json!({"recent": recent}),
    )
}

fn overview_ranking_executions(stats: &Value) -> u64 {
    stats["tools"]["overview_ranking"]["executions"]
        .as_u64()
        .unwrap_or(0)
}

/// Whole-file spans ensure every symbol overlaps a seeded chunk.
fn seed_fixture_chunks(root: &std::path::Path) {
    let db_path = root.join(".codesage").join("index.db");
    let db = Database::open_for_model(&db_path, "jinaai/jina-embeddings-v2-base-code", 4).unwrap();
    let embedding = [0.1_f32, 0.2, 0.3, 0.4];
    for rel in ["src/lib.rs", "src/helper.rs", "src/util.rs"] {
        let body = std::fs::read_to_string(root.join(rel)).unwrap();
        let lines = body.lines().count().max(1) as u32;
        db.insert_chunks(
            rel,
            "rust",
            &[(body.as_str(), 1, lines, embedding.as_slice())],
        )
        .unwrap();
    }
}

struct McpSession {
    child: ChildGuard,
    rx: Receiver<std::io::Result<String>>,
}

impl McpSession {
    fn start(runtime_dir: &std::path::Path) -> Self {
        Self::start_with_env(runtime_dir, &[])
    }

    fn start_with_env(runtime_dir: &std::path::Path, envs: &[(&str, &str)]) -> Self {
        let bin = env!("CARGO_BIN_EXE_codesage");
        let mut command = Command::new(bin);
        command
            .arg("mcp")
            .arg("--runtime-dir")
            .arg(runtime_dir)
            // The daemon inherits the first shim's environment.
            .env("CODESAGE_WATCH", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = ChildGuard {
            child: command.spawn().expect("spawn codesage mcp"),
        };
        let stdout = child.child.stdout.take().expect("child stdout");
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
        let stdin = self.child.child.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, id: u64, line: &str) -> Value {
        self.send(line);
        recv_response(&self.rx, id)
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

fn onboard_fixture_project(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn hello_symbol() {}\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_codesage");
    for args in [vec!["init"], vec!["index", "--no-semantic"]] {
        let out = Command::new(bin)
            .args(&args)
            .current_dir(root)
            .output()
            .expect("run codesage");
        assert!(
            out.status.success(),
            "codesage {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn seed_search_chunks(root: &std::path::Path) {
    let db_path = root.join(".codesage").join("index.db");
    let db = Database::open_for_model(&db_path, "jinaai/jina-embeddings-v2-base-code", 4).unwrap();
    let embedding = [0.1_f32, 0.2, 0.3, 0.4];
    db.insert_chunks(
        "src/lib.rs",
        "rust",
        &[("pub fn hello_symbol() {}", 1, 1, embedding.as_slice())],
    )
    .unwrap();
}

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct DaemonCleanup {
    runtime_dir: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        kill_daemon(&self.runtime_dir);
    }
}

fn recv_response(rx: &Receiver<std::io::Result<String>>, id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for MCP response");
        let line = rx
            .recv_timeout(remaining)
            .expect("MCP stdout closed before response")
            .expect("read MCP stdout");
        let value: Value = serde_json::from_str(&line).expect("MCP response JSON");
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return value;
        }
    }
}

fn kill_daemon(runtime_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
            continue;
        }
        let Some(pid) = read_daemon_pid_file(&path) else {
            continue;
        };
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
}

fn read_daemon_pid_file(path: &std::path::Path) -> Option<i32> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if let Ok(pid) = trimmed.parse::<i32>() {
        return (pid > 0).then_some(pid);
    }
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != "pid" {
            return None;
        }
        value.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
    })
}
