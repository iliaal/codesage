#![cfg(unix)]

use std::fs;
use std::io::pipe;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn cli(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    command.args(args).current_dir(root).stdin(Stdio::null());
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn version_and_indexed_query_exit_without_panicking_on_a_closed_pipe() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    assert_success(&cli(root, &["init"]).output().unwrap());
    fs::write(root.join("lib.rs"), "pub fn closed_pipe_target() {}\n").unwrap();
    assert_success(
        &cli(root, &["index", "--no-semantic", "--no-features"])
            .output()
            .unwrap(),
    );

    let version = cli(root, &["--version"]).output().unwrap();
    assert_success(&version);
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("codesage "));
    let query = cli(root, &["find-symbol", "closed_pipe_target", "--json"])
        .output()
        .unwrap();
    assert_success(&query);
    let json: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(json["results"][0]["name"], "closed_pipe_target");

    for args in [
        &["--version"][..],
        &["find-symbol", "closed_pipe_target", "--json"][..],
    ] {
        let (reader, writer) = pipe().unwrap();
        drop(reader);
        let output = cli(root, args).stdout(writer).output().unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGPIPE), "{args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("panicked"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
