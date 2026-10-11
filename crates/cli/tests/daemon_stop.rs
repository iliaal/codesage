#![cfg(unix)]

use std::{
    fs,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(runtime: &Path, action: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["daemon", action, "--runtime-dir"])
        .arg(runtime)
        .output()
        .unwrap()
}

fn daemon(runtime: &Path) -> (Process, PathBuf) {
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["daemon", "--runtime-dir"])
            .arg(runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "daemon exited at startup"
        );
        for entry in fs::read_dir(runtime).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "pid")
                && fs::read_to_string(&path).unwrap().lines().any(|line| {
                    line == format!("pid={}", process.0.id()) || line == process.0.id().to_string()
                })
                && command(runtime, "status").status.success()
            {
                return (process, path);
            }
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
}

fn sentinel() -> Process {
    Process(Command::new("sleep").arg("60").spawn().unwrap())
}

#[test]
fn daemon_stop_stops_the_real_daemon_and_cleans_runtime_files() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut process, pid_file) = daemon(runtime.path());
    use std::os::unix::fs::PermissionsExt;
    for path in [&pid_file, &pid_file.with_extension("ctl")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let output = command(runtime.path(), "stop");
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("stopped daemon"),
        "{output:?}"
    );
    assert!(process.0.wait().unwrap().success());
    for extension in ["pid", "sock", "ctl"] {
        assert!(
            !pid_file.with_extension(extension).exists(),
            "left runtime file {extension}"
        );
    }
    assert!(!command(runtime.path(), "status").status.success());
}

#[test]
fn daemon_start_does_not_signal_or_unlink_an_unverified_live_sibling() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let sibling_pid = runtime.path().join("mcp-0.0.1-fake.pid");
    let sibling_socket = sibling_pid.with_extension("sock");
    let _listener = UnixListener::bind(&sibling_socket).unwrap();
    let mut sentinel = sentinel();
    fs::write(
        &sibling_pid,
        format!(
            "pid={}\ncontrol_nonce={}\nstart_time_ticks=1\n",
            sentinel.0.id(),
            "22".repeat(32)
        ),
    )
    .unwrap();
    let (_daemon, _) = daemon(runtime.path());
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "startup signalled an unverified sibling PID"
    );
    assert!(
        sibling_pid.exists(),
        "startup removed live sibling PID state"
    );
    assert!(
        sibling_socket.exists(),
        "startup unlinked a live sibling socket"
    );
    assert!(command(runtime.path(), "stop").status.success());
}

#[test]
fn daemon_stop_rejects_replaced_pid_without_signalling_either_process() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    let mut sentinel = sentinel();
    let original = fs::read_to_string(&pid_file).unwrap();
    let replacement = original.replace(
        &format!("pid={}", daemon.0.id()),
        &format!("pid={}", sentinel.0.id()),
    );
    // Legacy platforms wrote only the numeric PID.
    let replacement = if replacement == original && original.trim().parse::<u32>().is_ok() {
        format!("{}\n", sentinel.0.id())
    } else {
        replacement
    };
    assert_ne!(replacement, original);
    fs::write(&pid_file, replacement).unwrap();
    assert!(!command(runtime.path(), "status").status.success());
    let output = command(runtime.path(), "stop");
    assert!(output.status.success(), "{output:?}");
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "replaced PID sentinel was signalled"
    );
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "mismatched record stopped the daemon"
    );
    fs::write(&pid_file, original).unwrap();
    assert!(command(runtime.path(), "stop").status.success());
}

#[test]
fn daemon_stop_rejects_a_stale_nonce_even_when_the_pid_is_live() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    let original = fs::read_to_string(&pid_file).unwrap();
    let mut lines: Vec<_> = original
        .lines()
        .filter(|line| !line.starts_with("control_nonce="))
        .map(str::to_owned)
        .collect();
    lines.push(format!("control_nonce={}", "00".repeat(32)));
    fs::write(&pid_file, lines.join("\n")).unwrap();
    let output = command(runtime.path(), "stop");
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "stale nonce authorized daemon shutdown"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!command(runtime.path(), "status").status.success());
    fs::write(&pid_file, original).unwrap();
    assert!(command(runtime.path(), "stop").status.success());
}

#[test]
fn daemon_stop_does_not_trust_an_unrelated_listening_socket() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    for extension in ["sock", "ctl"] {
        let _ = fs::remove_file(pid_file.with_extension(extension));
    }
    let _listener = UnixListener::bind(pid_file.with_extension("sock")).unwrap();
    let mut sentinel = sentinel();
    let record = format!(
        "pid={}\ncontrol_nonce={}\n",
        sentinel.0.id(),
        "11".repeat(32)
    );
    #[cfg(target_os = "linux")]
    let record = {
        let stat = fs::read_to_string(format!("/proc/{}/stat", sentinel.0.id())).unwrap();
        let start = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        format!("{record}start_time_ticks={start}\n")
    };
    fs::write(&pid_file, record).unwrap();
    let output = command(runtime.path(), "stop");
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "unrelated socket authorized signalling the sentinel"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!command(runtime.path(), "status").status.success());
}

#[test]
#[cfg(target_os = "linux")]
fn daemon_stop_preserves_linux_legacy_start_time_validation() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    let original = fs::read_to_string(&pid_file).unwrap();
    let start = original
        .lines()
        .find_map(|line| line.strip_prefix("start_time_ticks="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    fs::write(
        &pid_file,
        format!("pid={}\nstart_time_ticks={}\n", daemon.0.id(), start + 1),
    )
    .unwrap();
    assert!(!command(runtime.path(), "status").status.success());
    assert!(command(runtime.path(), "stop").status.success());
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "mismatched start time authorized shutdown"
    );
    fs::write(
        &pid_file,
        format!("pid={}\nstart_time_ticks={start}\n", daemon.0.id()),
    )
    .unwrap();
    let waiter = thread::spawn(move || daemon.0.wait().unwrap());
    let output = command(runtime.path(), "stop");
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("stopped daemon"),
        "{output:?}"
    );
    assert!(waiter.join().unwrap().success());
}
