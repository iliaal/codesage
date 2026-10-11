use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;

struct Session {
    child: Child,
    replies: Receiver<String>,
}

impl Session {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self { child, replies }
    }

    fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
    }

    fn response(&self, id: u64) -> Value {
        loop {
            let line = self.replies.recv_timeout(Duration::from_secs(15)).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    fn initialize(&mut self) {
        self.send(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"framing-test","version":"1"}
            }}),
        );
        assert_eq!(self.response(1)["result"]["serverInfo"]["name"], "codesage");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    }

    fn exercise_valid_frames(&mut self, project: &std::path::Path, default_project: bool) {
        let ping = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let mut boundary = ping.as_bytes().to_vec();
        boundary.resize(FRAME_LIMIT, b' ');
        boundary.push(b'\n');
        for chunk in boundary.chunks(7919) {
            self.child.stdin.as_mut().unwrap().write_all(chunk).unwrap();
        }
        assert_eq!(self.response(2)["result"], json!({}));

        self.child.stdin.as_mut().unwrap().write_all(
            b"not json\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\r\n{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n",
        ).unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let line = self.replies.recv_timeout(Duration::from_secs(15)).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["result"], json!({}));
            ids.push(value["id"].as_u64().unwrap());
        }
        ids.sort();
        assert_eq!(ids, vec![3, 4]);

        let mut arguments = json!({"tool":"find_symbol"});
        if !default_project {
            arguments["project"] = json!(project);
        }
        self.send(
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"help", "arguments":arguments
            }}),
        );
        let help = self.response(5);
        assert_ne!(help["result"]["isError"], true, "{help}");
        assert!(help["result"]["content"].is_array(), "{help}");
    }

    fn assert_rejected_while_stdin_open(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "oversized input kept the MCP process alive with stdin open"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success(), "oversized input must report failure");
        let mut stderr = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(
            stderr.contains("MCP frame exceeds 16777216 bytes"),
            "{stderr}"
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn direct_rejects_unterminated_oversized_input_while_stdin_stays_open() {
    for project_bound in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let mut args = vec!["mcp", "--direct"];
        if project_bound {
            args.extend(["--project", project.path().to_str().unwrap()]);
        }
        let mut session = Session::start(&args);
        session.initialize();
        session.exercise_valid_frames(project.path(), project_bound);
        session
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&vec![b'x'; FRAME_LIMIT + 1])
            .unwrap();
        session.assert_rejected_while_stdin_open();
    }
}

#[cfg(unix)]
struct DaemonCleanup(tempfile::TempDir);

#[cfg(unix)]
impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["daemon", "stop", "--runtime-dir"])
            .arg(self.0.path())
            .output();
    }
}

#[cfg(unix)]
#[test]
fn shim_and_daemon_socket_enforce_the_frame_limit_and_keep_other_clients_working() {
    use std::{io::Read, os::unix::net::UnixStream};

    let runtime = DaemonCleanup(tempfile::tempdir().unwrap());
    let project = tempfile::tempdir().unwrap();
    for project_bound in [false, true] {
        let mut args = vec!["mcp", "--runtime-dir", runtime.0.path().to_str().unwrap()];
        if project_bound {
            args.extend(["--project", project.path().to_str().unwrap()]);
        }
        let mut session = Session::start(&args);
        session.initialize();
        session.exercise_valid_frames(project.path(), project_bound);
        session
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&vec![b'x'; FRAME_LIMIT + 1])
            .unwrap();
        session.assert_rejected_while_stdin_open();
    }

    let socket = std::fs::read_dir(runtime.0.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .unwrap();
    for initialized in [false, true] {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        if initialized {
            writeln!(
                stream,
                "{}",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-11-25","capabilities":{},
                    "clientInfo":{"name":"raw-framing-test","version":"1"}
                }})
            )
            .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["result"]["serverInfo"]["name"],
                "codesage"
            );
            writeln!(
                stream,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
        stream.write_all(&vec![b'x'; FRAME_LIMIT + 1]).unwrap();
        match stream.read(&mut [0; 1]) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("daemon did not close the oversized connection: {other:?}"),
        }
    }
    let log = std::fs::read_dir(runtime.0.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "log"))
        .unwrap();
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("MCP frame exceeds 16777216 bytes")
    );
    let mut session = Session::start(&["mcp", "--runtime-dir", runtime.0.path().to_str().unwrap()]);
    session.initialize();
    session.send(json!({"jsonrpc":"2.0","id":2,"method":"ping"}));
    assert_eq!(session.response(2)["result"], json!({}));
}
