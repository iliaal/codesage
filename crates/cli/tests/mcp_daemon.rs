#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

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
    // CR-002 regression: pre-fix proxy_stdio used try_join! which only
    // returns when BOTH copy directions finish. If the daemon crashes
    // but the MCP client keeps stdin open, the shim stayed alive with
    // no server behind it. Symptom for the agent: an MCP session that
    // appears stuck on initialize or a tool call.
    //
    // Cover: start shim → wait for daemon to bind → kill daemon →
    // assert shim exits within a few seconds.
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

    // Initialize so we know the shim is talking to the daemon.
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

    // Daemon is up. Kill it. The shim should detect the closed socket
    // and exit on its own (M6 + CR-002). Without the CR-002 fix, the
    // shim's stdin pump kept blocking even after socket EOF and the
    // process hung indefinitely.
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
    // L4: the daemon's point is that multiple shims share one process.
    // Race two shims at startup; whoever wins the StartLock spawns the
    // daemon, the loser waits for the socket and connects to the same
    // daemon. Both initialize successfully and observe the same
    // serverInfo (same process answering).
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

    // The runtime dir should contain exactly one socket — both shims
    // connected to the same daemon. Without M1+M2 fixes this still
    // holds if startup races resolve correctly, so the strong signal
    // here is "no startup error" + "both shims got a response".
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
fn daemon_cleans_runtime_files_on_sigterm() {
    // M6: SIGTERM should let the daemon remove its socket + pid files
    // instead of leaving stale artefacts. Without graceful shutdown the
    // next shim startup has to do remove_stale_socket and the pid file
    // lingers indefinitely.
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

    // Wait for the daemon to bind. The daemon writes the socket (via
    // UnixListener::bind) BEFORE writing the pid file, so polling on
    // the socket alone races: a read_dir scan that lands between the
    // two operations sees the socket but no pid, exits the loop, and
    // then `pid_file.expect(...)` panics spuriously. Wait for both.
    // fnd_84c7bd40.
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

    // Send SIGTERM by reading the pid.
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
fn daemon_self_exits_after_idle_timeout() {
    // Idle backstop: with no client ever connecting, the daemon must reap
    // itself once CODESAGE_DAEMON_IDLE_TIMEOUT_SECS elapses instead of pinning
    // the embedder/reranker pools in memory forever after every agent exits.
    // Set a 1s timeout and assert the process exits on its own (no signal sent)
    // and cleans up its runtime files the same way a SIGTERM would.
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

    // Wait for the daemon to bind (socket + pid both present) before timing
    // the idle exit, matching the SIGTERM test's two-file wait.
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

    // No signal sent: the 1s idle timeout (polled at 1s granularity) should
    // trip within a couple of ticks.
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
    // The per-connection ceiling must be measured from the client's last
    // request, not from connection start. A client that keeps sending requests
    // faster than CODESAGE_CLIENT_IDLE_MAX_SECS must never be dropped, however
    // old the connection gets. Pre-fix the ceiling was an absolute
    // timeout(CLIENT_SESSION_MAX, waiting()) that guillotined healthy sessions
    // at 1h regardless of activity.
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

    // Send a request every 800ms for ~5s — well past the 2s ceiling. Each
    // response must come back, proving the ceiling resets on every request.
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
    // The ceiling must still fire when a client goes silent (a hung tool call
    // or an agent that wandered off without disconnecting). With a 2s ceiling
    // and no requests after initialize, the daemon drops the connection; the
    // shim then sees its socket close and exits on its own. stdin is left open
    // so the only thing that can end the shim is the daemon-side idle drop.
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

    // Now go silent. Within a couple of 2s polls the daemon should drop the
    // idle connection, and the shim exits when its socket closes.
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
    // The fallback path is covered by daemon.rs unit tests; this integration
    // test uses an explicit runtime dir so it never collides with a developer's
    // real daemon under /tmp while still exercising status against a live child.
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

    // Wait for both socket and pid file — status reads the pid file and the
    // daemon binds the socket before writing it (fnd_017ca191).
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
    // rmcp's ToolRouter rejects unknown tools with invalid_params (-32602),
    // message "tool not found" — not the spec's -32601 method-not-found.
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
    // rmcp surfaces a parameter-deserialization failure as a tool RESULT with
    // isError=true (not a protocol-level -32602), with the serde message in
    // the content text.
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
    // The offending value must be quoted so an agent can self-correct; the
    // exact phrasing around it is free to change.
    assert!(
        text.contains("not-a-number"),
        "error must quote the offending value, got: {text:?}"
    );
}

#[test]
fn tools_call_find_coupling_coerces_stringy_limit() {
    // The documented dominant real-world agent error is `"limit": "5"` (a
    // JSON string instead of a number). The server coerces it, so the call
    // must get PAST parameter validation — no -32602 — and run the tool.
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
}

/// One MCP shim (stdin/stdout JSON-RPC) with a line-reader thread, so
/// tools/call tests don't re-inline the pump plumbing per test.
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
            // The daemon inherits the first shim's env; keep the
            // per-project watcher out of tool-call tests.
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

/// Onboard a throwaway project offline: `init` + structural-only index
/// (`--no-semantic` needs no model download, no network).
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
