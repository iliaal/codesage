#![cfg(unix)]

use std::process::Command;

#[test]
fn stats_rejects_recent_above_retention_cap() {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["daemon", "stats", "--recent", "257"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("256"));
}

#[test]
fn stats_without_daemon_fails_without_creating_runtime_state() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join("absent-runtime");
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime)
        .args(["stats", "--json", "--recent", "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no compatible daemon"));
    assert!(
        !runtime.exists(),
        "stats must not spawn or create runtime state"
    );
}
