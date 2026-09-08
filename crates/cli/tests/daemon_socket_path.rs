#![cfg(unix)]

use std::process::{Command, Stdio};

#[test]
fn startup_rejects_overlong_runtime_paths_before_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join("é".repeat(100));
    for command in ["mcp", "daemon"] {
        for setting in [
            "CODESAGE_DAEMON_RUNTIME_DIR",
            "XDG_RUNTIME_DIR",
            "--runtime-dir",
        ] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"));
            child
                .arg(command)
                .env_remove("CODESAGE_DAEMON_RUNTIME_DIR")
                .env_remove("XDG_RUNTIME_DIR")
                .stdin(Stdio::null());
            if setting.starts_with("--") {
                child.arg(setting).arg(&runtime);
            } else {
                child.env(setting, &runtime);
            }
            let output = child.output().unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "{command} {setting}: {stderr}"
            );
            assert!(
                stderr.contains("invalid daemon socket pathname"),
                "{stderr}"
            );
            assert!(stderr.contains(&runtime.display().to_string()), "{stderr}");
            assert!(stderr.contains(setting), "{stderr}");
            assert!(stderr.contains("shorter runtime directory"), "{stderr}");
            assert!(!stderr.contains("spawned codesage daemon"), "{stderr}");
            assert!(
                !runtime.exists(),
                "{command} {setting} created runtime state"
            );
        }
    }
}
