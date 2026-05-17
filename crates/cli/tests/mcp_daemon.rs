#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

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

    // Wait for the daemon to bind. Loop on the pid file appearing.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    while Instant::now() < deadline && socket.is_none() {
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
    let pid = std::fs::read_to_string(&pid_file).expect("read pid file");
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.trim())
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
        let Ok(pid) = std::fs::read_to_string(&path) else {
            continue;
        };
        let _ = Command::new("kill").arg("-TERM").arg(pid.trim()).status();
    }
}
