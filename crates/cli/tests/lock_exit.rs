//! A `codesage index` that finds the project lock held for its whole wait
//! window must exit with a distinct nonzero status. It used to exit 0 with
//! nothing indexed, and the installed git hook then recorded the tree as
//! indexed and skipped every later run on the same HEAD.

#![cfg(unix)]

use std::process::Command;

#[test]
fn index_exits_75_when_the_lock_is_held_for_the_whole_wait() {
    let bin = env!("CARGO_BIN_EXE_codesage");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".codesage")).unwrap();
    // Hold the same advisory flock the binary takes.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".codesage/indexing.lock"))
        .unwrap();
    lock.try_lock().expect("fresh lockfile must lock");

    let out = Command::new(bin)
        .args(["index", "--lock-wait", "0"])
        .current_dir(root)
        .output()
        .expect("spawn codesage");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(75),
        "held lock must be exit 75, got {:?}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains("another codesage indexer is running"),
        "stderr must name the contention:\n{stderr}"
    );
    drop(lock);
}
