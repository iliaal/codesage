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
    // Regression: proxy_stdio previously used try_join!, which only
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
    // and exit on its own. Previously the shim's stdin pump kept
    // blocking even after socket EOF and the process hung indefinitely.
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
fn silent_client_that_never_initializes_is_dropped() {
    // A peer that connects and never sends `initialize` parks the connection
    // task inside the handshake await. The per-connection idle ceiling only
    // starts *after* that await, so before the handshake was bounded such a
    // peer held the daemon's active-client count above zero permanently and
    // the whole-daemon idle backstop could never fire again.
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

    // Say nothing. The daemon must close the connection on its own; a read
    // returning 0 bytes is that EOF.
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
fn daemon_sigterm_with_parked_client_exits_bounded_and_cleans_up() {
    // Graceful shutdown drains in-flight connections for a bounded window.
    // A parked client (connected but idle) must not stall SIGTERM shutdown
    // forever: after the drain bound the daemon aborts the connection,
    // removes its runtime files, and exits.
    let runtime = tempfile::tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    let _daemon_cleanup = DaemonCleanup {
        runtime_dir: runtime_dir.clone(),
    };
    let mut session = McpSession::start(&runtime_dir);
    session.initialize();

    // Locate the daemon's runtime files while the shim stays connected.
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

    // 5s drain bound + margin. Failing here means shutdown hangs on the
    // parked connection instead of bounding the wait.
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
    // daemon binds the socket before writing it.
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

/// Every tool that advertises an `outputSchema` must answer a representative
/// successful call with a **populated, data-bearing** `structuredContent`.
///
/// Claude Code treats `structuredContent` as THE result once a tool declares
/// an output schema, so a tool that ships `{}` there renders as an empty
/// object no matter how good its text block is.
///
/// Two things make this test non-vacuous, and both matter:
///
/// 1. The fixture (see [`onboard_rich_fixture`]) carries real data for every
///    tool — git history with a co-change pair, a sibling test file, a Python
///    import chain with edges in both directions, a near-clone pair, seeded
///    semantic chunks. Against a bare `src/lib.rs`, half this surface answers
///    `{"results": []}` or `{"found": false}`, which is a populated object and
///    would pass a shape-only assertion without ever running the tool's
///    data-bearing branch.
/// 2. Each tool names the specific keys that must be non-empty, not just "some
///    non-`_meta` key exists".
///
/// Tools with a real precondition are driven through it rather than exempted:
/// `feature_bundle` gets an id resolved from `list_features`, and `session_end`
/// runs after `session_start` plus an actual tree change and reindex.
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
        // Lets `search` run without a model download; the seeded chunk table
        // is 4-dimensional to match.
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

    // Feature ids are content hashes, so `feature_bundle` has to be handed a
    // real one — a made-up id lands on the `found: false` branch, which is a
    // populated object too and would never exercise the loaded-bundle path.
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

    // (tool, arguments, keys that must carry data). `session_end` must follow
    // `session_start` on the same id, so the table is ordered, not a map.
    let calls: Vec<(&str, Value, &[&str])> = vec![
        (
            "project_overview",
            serde_json::json!({}),
            // `top_risk_files` is empty until git history is indexed, so it
            // doubles as proof the fixture's git-index pass took effect.
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
            // The one fixture file with edges in BOTH directions: it imports
            // `py/util.py` and is imported by `py/main.py`. Rust `use crate::`
            // paths do not resolve back to files, so a Rust-only fixture
            // leaves `imported_by` empty on every file.
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
            // `top_coupled` is verbose-only; the default trim is pinned by
            // `assess_risk_default_response_omits_verbose_fields`.
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
            // Non-empty only because the loop adds a file and reindexes right
            // before this call; on an unchanged tree every array here is [].
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
            // Drive the real precondition instead of exempting the tool: a
            // session diff over an unchanged tree is all-empty by definition.
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

        // The text block must not disagree with the structured payload: an
        // agent reading either one has to see the same facts. `_meta` is
        // exempt — coverage and staleness annotations are merged into the
        // structured value after the text is rendered, and their human-facing
        // half is prepended as its own banner block.
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

/// Whether a response field actually carries a result rather than the shape of
/// one. `[]`, `""`, `{}`, `0`, `false` and `null` all mean "the tool ran but
/// found nothing", which is exactly the vacuous pass this guards against.
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

/// The risk tools hide the per-signal decomposition and `top_coupled` unless
/// the caller passes `verbose: true` (`cycle_files` is exempt — the staleness
/// scan needs it). Pinned over the daemon wire, on the same fixture the
/// populated-content test uses, so the data-bearing branch (a file with
/// co-change history) is what gets trimmed.
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

    // assess_risk: default trims, verbose restores.
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

    // assess_risk_batch and assess_risk_diff apply the same gate per entry.
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

/// `trace_call_path` (MCP) and `codesage trace --json` (CLI) must carry the
/// same per-step evidence. `call_line` is the whole point of the tool — it
/// names the line in the caller's body where the next hop is invoked — and it
/// is `skip_serializing_if = "Option::is_none"`, exactly the kind of field
/// that can vanish from one surface unnoticed. Both shapes are pinned
/// explicitly, then checked against each other.
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

    // The fixture has a real cross-file edge, so both surfaces must find it.
    // Without this the comparisons below would pass vacuously over two empty
    // `steps` arrays.
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

    // Pin each shape absolutely, not just relative to the other — two
    // surfaces that lose the same field together would still agree.
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

    // Field-set parity key by key, so a field added or dropped on one surface
    // only is caught even if both still satisfy the pins above.
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
        .filter(|k| *k != "_meta")
        .collect();
    assert_eq!(ckeys, mkeys, "top-level field sets diverge");
}

/// A fixture with real data for every MCP tool, so an assertion that a tool
/// returned something is not satisfied by an empty result. It carries:
///
/// - a cross-file call edge `outer_step` → `inner_step` (`trace_call_path`,
///   `impact_analysis`, `find_references`)
/// - a structurally identical `twin_a` / `twin_b` pair (`find_similar`)
/// - a Python import chain where `py/app.py` both imports and is imported, the
///   only shape that gives `list_dependencies` non-empty edges in both
///   directions (Rust `use crate::` paths do not resolve back to files)
/// - a sibling test file (`recommend_tests`, and the feature-test-gap
///   objection in `review_rehearsal`)
/// - four commits, each touching `src/helper.rs` and `src/util.rs` together,
///   which clears the min-count-3 co-change threshold (`find_coupling`) and
///   gives churn a percentile to report (`assess_risk`, `project_overview`'s
///   `top_risk_files`)
/// - a Cargo manifest, so the feature mapper produces a slice
///   (`list_features`, `find_feature`, `feature_bundle`)
///
/// Indexed structurally only: no model download, no network.
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
    // Three more commits touching the same pair, clearing the min-count-3
    // co-change threshold so `find_coupling` has a row to return.
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

/// Seed semantic chunks for the fixture's Rust files so `search`,
/// `export_context` and `feature_bundle` have content to return without a
/// model download. Line spans cover each file whole, so a symbol lookup
/// anywhere in them resolves to an overlapping chunk.
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
