#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, getpgid, getsid, kill_process_group};
use serde_json::{Value, json};

struct Runtime(tempfile::TempDir);

impl Runtime {
    fn new() -> Self {
        Self(tempfile::tempdir_in("/tmp").unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn record(&self) -> (PathBuf, Pid) {
        let path = fs::read_dir(self.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "pid"))
            .expect("initialized daemon has a pid file");
        let record = fs::read_to_string(&path).unwrap();
        let pid = record
            .lines()
            .find_map(|line| line.strip_prefix("pid=").unwrap_or(line).parse().ok())
            .and_then(Pid::from_raw)
            .expect("daemon pid");
        (path, pid)
    }

    fn stop(&self) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["daemon", "stop", "--runtime-dir"])
            .arg(self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("daemon stop exceeded its deadline: {output:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Shim {
    child: Child,
    responses: Receiver<Value>,
}

impl Shim {
    fn start(runtime: &Runtime, idle_seconds: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .args(["mcp", "--runtime-dir"])
            .arg(runtime.path())
            .env("CODESAGE_DAEMON_IDLE_TIMEOUT_SECS", idle_seconds)
            .env("CODESAGE_CLIENT_IDLE_MAX_SECS", "0")
            .env("RUST_LOG", "codesage=info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and the closure does not allocate or lock.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        Self { child, responses }
    }

    fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "daemon-detachment-test", "version": "0.0.0"}
            }
        }));
        let response = self.response(1);
        assert_eq!(response["result"]["serverInfo"]["name"], "codesage");
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let value = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("MCP response within deadline");
            if value["id"] == id {
                return value;
            }
        }
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn shared_daemon_survives_spawning_shim_process_group_teardown() {
    let runtime = Runtime::new();
    let mut first = Shim::start(&runtime, "0");
    first.initialize();
    let shim_pid = Pid::from_raw(first.child.id().try_into().unwrap()).unwrap();
    assert_eq!(getpgid(Some(shim_pid)).unwrap(), shim_pid);
    assert_eq!(getsid(Some(shim_pid)).unwrap(), shim_pid);
    let (pid_file, daemon_pid) = runtime.record();
    let socket = pid_file.with_extension("sock");
    let socket_inode = fs::metadata(&socket).unwrap().ino();
    let daemon_group = getpgid(Some(daemon_pid)).unwrap();
    let daemon_session = getsid(Some(daemon_pid)).unwrap();

    kill_process_group(shim_pid, Signal::TERM).unwrap();
    match kill_process_group(shim_pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => panic!("killing spawning shim process group: {error}"),
    }
    assert!(!first.child.wait().unwrap().success());

    let mut second = Shim::start(&runtime, "0");
    second.initialize();
    assert_eq!(
        runtime.record().1,
        daemon_pid,
        "host process-group teardown replaced the shared daemon"
    );
    assert_eq!(fs::metadata(&socket).unwrap().ino(), socket_inode);
    assert_eq!(daemon_group, daemon_pid);
    assert_eq!(daemon_session, daemon_pid);
    assert_ne!(daemon_group, shim_pid);
    assert_ne!(daemon_session, shim_pid);
    second.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = second.response(2);
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "search")
    );
    drop(second);
    let stopped = runtime.stop();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(
        String::from_utf8_lossy(&stopped.stdout).contains("stopped daemon"),
        "{stopped:?}"
    );
    for extension in ["pid", "sock", "ctl"] {
        assert!(!pid_file.with_extension(extension).exists());
    }
}

#[test]
fn detached_daemon_exits_when_last_shim_leaves_and_idle_timeout_expires() {
    let runtime = Runtime::new();
    let mut shim = Shim::start(&runtime, "1");
    shim.initialize();
    let (pid_file, _) = runtime.record();
    drop(shim);
    let deadline = Instant::now() + Duration::from_secs(10);
    while ["pid", "sock", "ctl"]
        .iter()
        .any(|extension| pid_file.with_extension(extension).exists())
    {
        assert!(
            Instant::now() < deadline,
            "detached daemon did not clean its runtime files after idle exit"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let log = fs::read_to_string(pid_file.with_extension("log")).unwrap();
    assert!(log.contains("reason=\"idle\""), "{log}");
}
