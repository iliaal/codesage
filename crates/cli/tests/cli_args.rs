//! Startup argument-handling regression tests for the `codesage` binary.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

/// A non-UTF-8 argument must not panic the binary at startup. `main()` runs the
/// pre-clap shim detector (`is_shim_invocation`) on every invocation; before the
/// fix it iterated `std::env::args()`, which panics mid-iteration on non-UTF-8
/// argv. The `mcp --project <non-utf8>` shape is exactly what `codesage install`
/// writes into agent configs, and the detector scans past `mcp`, so it reaches
/// the non-UTF-8 `--project` value (where a non-`mcp` first positional would
/// early-return before it). It should fail cleanly (canonicalize error), never
/// panic.
#[test]
fn non_utf8_project_arg_does_not_panic_at_startup() {
    let bin = env!("CARGO_BIN_EXE_codesage");
    let bad = OsStr::from_bytes(b"/tmp/\xff\xfe-not-a-project");

    let out = Command::new(bin)
        .arg("mcp")
        .arg("--project")
        .arg(bad)
        .output()
        .expect("spawn codesage");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "non-UTF-8 --project must fail cleanly, not be silently accepted"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("invalid utf-8"),
        "startup panicked on a non-UTF-8 argument instead of erroring cleanly:\n{stderr}"
    );
}

#[test]
fn init_rejects_file_named_codesage_dir() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".codesage"), "not a directory\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage init");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "init must reject a non-directory .codesage entry, not report success; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains(".codesage") && stderr.contains("not a directory"),
        "init should explain the invalid project marker; stderr={stderr:?}"
    );
}
