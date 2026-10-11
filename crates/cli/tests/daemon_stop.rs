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
    let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["daemon", action, "--runtime-dir"])
        .arg(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("daemon {action} exceeded its process-test deadline: {output:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
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
    fs::remove_file(pid_file.with_extension("ctl")).unwrap();
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

#[test]
fn daemon_start_preserves_live_control_when_mcp_socket_is_missing() {
    use std::os::unix::fs::MetadataExt;

    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut first, pid_file) = daemon(runtime.path());
    let record = fs::read_to_string(&pid_file).unwrap();
    let control = pid_file.with_extension("ctl");
    let inode = fs::metadata(&control).unwrap().ino();
    fs::remove_file(pid_file.with_extension("sock")).unwrap();
    let output = command(runtime.path(), "run");
    assert!(
        !output.status.success(),
        "second daemon replaced live control endpoint: {output:?}"
    );
    assert_eq!(fs::metadata(&control).unwrap().ino(), inode);
    assert_eq!(fs::read_to_string(&pid_file).unwrap(), record);
    assert!(first.0.try_wait().unwrap().is_none());
    let output = command(runtime.path(), "stop");
    assert!(
        output.status.success(),
        "original daemon lost its shutdown endpoint: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("stopped daemon"));
    assert!(first.0.wait().unwrap().success());
}

#[test]
fn daemon_stop_rejects_nonce_stripping_from_a_current_daemon() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    let original = fs::read_to_string(&pid_file).unwrap();
    let stripped = original
        .lines()
        .filter(|line| !line.starts_with("control_nonce="))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&pid_file, stripped).unwrap();
    let output = command(runtime.path(), "stop");
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "removing the nonce downgraded current-daemon authorization"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!command(runtime.path(), "status").status.success());
    fs::write(&pid_file, original).unwrap();
    assert!(command(runtime.path(), "stop").status.success());
}

#[test]
#[cfg(target_os = "linux")]
fn daemon_stop_rejects_spoofed_daemon_argv_and_correct_start_time() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    for extension in ["sock", "ctl"] {
        fs::remove_file(pid_file.with_extension(extension)).unwrap();
    }
    let mut sentinel = Process(
        Command::new("python3")
            .args([
                "-c",
                "import time; time.sleep(60)",
                "daemon",
                "--runtime-dir",
            ])
            .arg(runtime.path())
            .spawn()
            .unwrap(),
    );
    let stat = fs::read_to_string(format!("/proc/{}/stat", sentinel.0.id())).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    fs::write(
        &pid_file,
        format!("pid={}\nstart_time_ticks={start}\n", sentinel.0.id()),
    )
    .unwrap();
    let output = command(runtime.path(), "stop");
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "spoofed daemon argv authorized a signal to Python"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!command(runtime.path(), "status").status.success());
}

#[test]
#[cfg(target_os = "linux")]
fn daemon_stop_requires_the_legacy_socket_to_belong_to_the_recorded_process() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    for extension in ["sock", "ctl"] {
        fs::remove_file(pid_file.with_extension(extension)).unwrap();
    }
    let _listener = UnixListener::bind(pid_file.with_extension("sock")).unwrap();
    let mut sentinel = Process(
        Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stat = fs::read_to_string(format!("/proc/{}/stat", sentinel.0.id())).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    fs::write(
        &pid_file,
        format!("pid={}\nstart_time_ticks={start}\n", sentinel.0.id()),
    )
    .unwrap();
    let output = command(runtime.path(), "stop");
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "a matching executable authorized signalling the wrong socket owner"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!command(runtime.path(), "status").status.success());
}

#[test]
#[cfg(not(target_os = "linux"))]
fn daemon_stop_reports_unverified_legacy_daemon_after_executable_key_changes() {
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let (mut daemon, pid_file) = daemon(runtime.path());
    let original = fs::read_to_string(&pid_file).unwrap();
    let legacy_pid = runtime.path().join("mcp-0.0.1-legacy.pid");
    fs::write(&legacy_pid, format!("{}\n", daemon.0.id())).unwrap();
    fs::remove_file(&pid_file).unwrap();
    fs::rename(
        pid_file.with_extension("sock"),
        legacy_pid.with_extension("sock"),
    )
    .unwrap();
    let output = command(runtime.path(), "stop");
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("legacy daemon identity cannot be authenticated"),
        "{output:?}"
    );
    assert!(daemon.0.try_wait().unwrap().is_none());
    assert!(legacy_pid.with_extension("sock").exists());
    assert!(!command(runtime.path(), "status").status.success());
    fs::rename(
        legacy_pid.with_extension("sock"),
        pid_file.with_extension("sock"),
    )
    .unwrap();
    fs::remove_file(legacy_pid).unwrap();
    fs::write(pid_file, original).unwrap();
    assert!(command(runtime.path(), "stop").status.success());
}
