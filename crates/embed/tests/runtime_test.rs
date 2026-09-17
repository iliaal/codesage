#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use codesage_embed::model::{init_for_main, init_ort_dylib, ort_runtime_dylib};

const CHILD: &str = "CODESAGE_TEST_RUNTIME_CHILD";
const RUNTIME: &str = "CODESAGE_TEST_RUNTIME_PATH";

fn check_child(loadable: bool) {
    let expected = PathBuf::from(std::env::var_os(RUNTIME).unwrap());
    let before: BTreeMap<_, _> = std::env::vars_os().collect();
    // These public fallbacks must remain safe when the caller already has threads.
    std::thread::spawn(move || {
        assert_eq!(ort_runtime_dylib().unwrap(), Some(expected.clone()));
        if loadable {
            init_ort_dylib().unwrap();
            // Exercise the actual native API, not just path selection or dlopen.
            ort::session::Session::builder().unwrap();
        } else {
            let error = init_ort_dylib().unwrap_err();
            assert!(format!("{error:#}").contains(expected.to_str().unwrap()));
        }
        init_for_main();
        assert_eq!(ort_runtime_dylib().unwrap(), Some(expected));
        assert_eq!(std::env::vars_os().collect::<BTreeMap<_, _>>(), before);
    })
    .join()
    .unwrap();
}

fn run_children(test: &str, native_runtime: Option<&Path>) {
    let fixture = tempfile::tempdir().unwrap();
    let site = fixture.path().join("site-packages");
    let capi = site.join("onnxruntime/capi");
    let nvidia = site.join("nvidia/cuda_runtime/lib");
    let bin = fixture.path().join("bin");
    std::fs::create_dir_all(&capi).unwrap();
    std::fs::create_dir_all(&nvidia).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(nvidia.join("libcudart.so.12"), b"discovery fixture").unwrap();
    let discovered = capi.join("libonnxruntime.so.1.24.0");
    if let Some(runtime) = native_runtime {
        std::os::unix::fs::symlink(runtime, &discovered).unwrap();
    } else {
        std::fs::write(
            &discovered,
            b"invalid runtime: loading must report an error",
        )
        .unwrap();
    }
    let python = bin.join("python3");
    std::fs::write(
        &python,
        "#!/bin/sh\nprintf '%s\\n' \"$CODESAGE_TEST_SITE_PACKAGES\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();

    for configured in [
        Some(discovered.as_os_str()),
        Some(std::ffi::OsStr::new("")),
        None,
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--nocapture"]);
        if native_runtime.is_some() {
            command.arg("--ignored");
        }
        command
            .env(CHILD, "1")
            .env(RUNTIME, &discovered)
            .env("PATH", &bin)
            .env("CODESAGE_TEST_SITE_PACKAGES", &site)
            .env("CODESAGE_NVIDIA_LIBS", site.join("nvidia"))
            .env(
                "LD_LIBRARY_PATH",
                fixture.path().join("unchanged-loader-path"),
            );
        if let Some(path) = configured {
            command.env("ORT_DYLIB_PATH", path);
        } else {
            command.env_remove("ORT_DYLIB_PATH");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "configured={configured:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn runtime_fallback_preserves_environment_and_reports_load_errors() {
    if std::env::var_os(CHILD).is_some() {
        check_child(false);
        return;
    }
    run_children(
        "runtime_fallback_preserves_environment_and_reports_load_errors",
        None,
    );
}

#[test]
#[ignore = "requires CODESAGE_TEST_ORT_DYLIB pointing to ONNX Runtime >= 1.24"]
fn explicit_and_discovered_runtimes_load_without_environment_writes() {
    if std::env::var_os(CHILD).is_some() {
        check_child(true);
        return;
    }
    let runtime = std::env::var_os("CODESAGE_TEST_ORT_DYLIB")
        .expect("set CODESAGE_TEST_ORT_DYLIB to a compatible ONNX Runtime shared library");
    let runtime = std::fs::canonicalize(runtime).unwrap();
    run_children(
        "explicit_and_discovered_runtimes_load_without_environment_writes",
        Some(&runtime),
    );
}
