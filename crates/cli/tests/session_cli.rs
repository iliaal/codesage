use std::{
    path::Path,
    process::{Command, Output},
};

#[test]
fn session_end_failure_prints_report_before_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_acyclic_php(root);

    run_ok(codesage(root).arg("init"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );
    run_ok(
        codesage(root)
            .arg("session-start")
            .arg("--session-id")
            .arg("flush-check"),
    );

    write_cyclic_php(root);
    let _ = std::fs::remove_file(root.join("C.php"));
    run_ok(
        codesage(root)
            .arg("index")
            .arg("--full")
            .arg("--no-semantic"),
    );

    let out = codesage(root)
        .arg("session-end")
        .arg("--session-id")
        .arg("flush-check")
        .output()
        .expect("run session-end");
    assert!(
        !out.status.success(),
        "session-end should fail when a new cycle is introduced"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Session flush-check: FAIL") && stdout.contains("NEW cycles"),
        "session-end failure report was not flushed to stdout: {stdout:?}"
    );
}

fn codesage(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codesage"));
    cmd.current_dir(root);
    cmd
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("run command");
    assert!(
        out.status.success(),
        "command failed: status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write_acyclic_php(root: &Path) {
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Mid;\nclass Top { public function x(Mid $m) { return $m->y(null); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Leaf;\nclass Mid { public function y(Leaf $l) { return $l->z(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("C.php"),
        b"<?php\nnamespace App;\nclass Leaf { public function z() { return 1; } }\n",
    )
    .unwrap();
}

fn write_cyclic_php(root: &Path) {
    std::fs::write(
        root.join("A.php"),
        b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller { public function x(Repository $r) { return $r->y(); } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("B.php"),
        b"<?php\nnamespace App;\nuse App\\Controller;\nclass Repository { public function y(Controller $c) { return $c->x(null); } }\n",
    )
    .unwrap();
}
