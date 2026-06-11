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
    let dir = tempfile::tempdir().unwrap();
    let bad_name = OsStr::from_bytes(b"\xff\xfe-not-a-project");
    let bad = dir.path().join(bad_name);
    fs::create_dir(&bad).expect("create non-UTF-8 project dir");

    let out = Command::new(bin)
        .arg("mcp")
        .arg("--project")
        .arg(&bad)
        .output()
        .expect("spawn codesage");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "non-UTF-8 --project must fail cleanly, not be silently accepted"
    );
    assert!(
        stderr.contains("UTF-8") || stderr.contains("utf-8"),
        "expected UTF-8 rejection message, got:\n{stderr}"
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

#[test]
fn version_like_subcommand_arg_is_not_hijacked() {
    let dir = tempfile::tempdir().unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .arg("find-symbol")
        .arg("--")
        .arg("--version")
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage find-symbol");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "subcommand should receive --version as data and fail on missing project; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.starts_with("codesage "),
        "subcommand argument was handled as top-level --version; stdout={stdout:?}"
    );
    assert!(
        stderr.contains("not a codesage project"),
        "subcommand did not reach normal project-root validation; stderr={stderr:?}"
    );
}

#[test]
fn index_rejects_zero_batch_size_at_parse_time() {
    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .arg("index")
        .arg("--batch-size")
        .arg("0")
        .output()
        .expect("spawn codesage index");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid batch size should be rejected by clap; stderr={stderr:?}"
    );
    assert!(
        stderr.contains("positive integer"),
        "parse error should explain the positive integer requirement; stderr={stderr:?}"
    );
}
