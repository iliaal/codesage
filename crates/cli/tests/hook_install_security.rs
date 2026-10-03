#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const HOOKS: [&str; 5] = [
    "post-commit",
    "post-merge",
    "post-checkout",
    "post-rewrite",
    "pre-commit",
];

fn command(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(root);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn git(root: &Path, args: &[&str]) {
    let output = command("git", root).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "core.hooksPath", ".git/hooks"]);
    fs::create_dir(dir.path().join(".codesage")).unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    fs::write(
        dir.path().join("scripts/leak-check.sh"),
        "#!/bin/sh\nexit 0\n",
    )
    .unwrap();
    dir
}

fn install(root: &Path) -> Output {
    install_command(command(env!("CARGO_BIN_EXE_codesage"), root))
}

fn install_command(mut command: Command) -> Output {
    let mut child = command
        .args(["install-hooks", "--with-leak-check"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("hook installation blocked: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn install_hooks_supports_write_and_search_only_ancestors() {
    fn chown_tree(path: &Path, uid: u32) {
        if fs::symlink_metadata(path).unwrap().is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                chown_tree(&entry.unwrap().path(), uid);
            }
        }
        rustix::fs::chownat(
            rustix::fs::CWD,
            path,
            Some(rustix::fs::Uid::from_raw(uid)),
            None,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    for husky in [false, true] {
        let dir = fixture();
        let parent = dir.path().join("search-only");
        let root = parent.join("project");
        fs::create_dir_all(&root).unwrap();
        for entry in fs::read_dir(dir.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.path() != parent {
                fs::rename(entry.path(), root.join(entry.file_name())).unwrap();
            }
        }
        let hooks = if husky {
            git(&root, &["config", "core.hooksPath", ".husky/_"]);
            let hooks = root.join(".husky");
            fs::create_dir(&hooks).unwrap();
            hooks
        } else {
            root.join(".git/hooks")
        };
        let owner = fs::metadata(dir.path()).unwrap().uid();
        let uid = if owner == 0 { 65534 } else { owner };
        let binary = if owner == 0 {
            let binary = dir.path().join("codesage");
            fs::copy(env!("CARGO_BIN_EXE_codesage"), &binary).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            chown_tree(dir.path(), uid);
            binary
        } else {
            env!("CARGO_BIN_EXE_codesage").into()
        };
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o300)).unwrap();

        let probe = command("sh", &root)
            .uid(uid)
            .args([
                "-c",
                "test \"$(id -u)\" -ne 0 && ! ls \"$1\" >/dev/null 2>&1 && printf 'allowed\\n' > .git/hooks/probe && chmod 755 .git/hooks/probe && test \"$(cat .git/hooks/probe)\" = allowed && rm .git/hooks/probe",
                "probe",
            ])
            .arg(&parent)
            .output()
            .unwrap();
        let git_probe = command("git", &root)
            .uid(uid)
            .args(["rev-parse", "--git-common-dir"])
            .output()
            .unwrap();
        let mut candidate = command(&binary, &root);
        candidate.uid(uid);
        let installed = install_command(candidate);

        let parent_mode = fs::metadata(&parent).unwrap().mode() & 0o777;
        let hooks_mode = fs::metadata(&hooks).unwrap().mode() & 0o777;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(probe.status.success(), "unprivileged access: {probe:?}");
        assert!(git_probe.status.success(), "Git lookup: {git_probe:?}");
        assert!(installed.status.success(), "husky={husky}: {installed:?}");
        assert_eq!(parent_mode, 0o300);
        assert_eq!(hooks_mode, 0o300);
        for name in HOOKS {
            assert!(
                fs::read_to_string(hooks.join(name))
                    .unwrap()
                    .contains("codesage install-hooks"),
                "{name}"
            );
            assert_eq!(
                fs::metadata(hooks.join(name)).unwrap().mode() & 0o777,
                0o755
            );
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o300)).unwrap();

        let target = root.join("link-target");
        fs::write(&target, "# codesage install-hooks\npreserve target\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let hook = hooks.join("post-commit");
        fs::remove_file(&hook).unwrap();
        symlink(&target, &hook).unwrap();
        let mut candidate = command(&binary, &root);
        candidate.uid(uid);
        let refused = install_command(candidate);

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!refused.status.success(), "link accepted: {refused:?}");
        assert_eq!(fs::read_link(&hook).unwrap(), target);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "# codesage install-hooks\npreserve target\n"
        );
        assert_eq!(fs::metadata(target).unwrap().mode() & 0o777, 0o640);
    }
}

#[test]
fn install_hooks_refuses_directory_and_fifo_leaves_without_blocking() {
    for name in HOOKS {
        for fifo in [false, true] {
            let dir = fixture();
            let hook = dir.path().join(".git/hooks").join(name);
            if fifo {
                let output = command("mkfifo", dir.path()).arg(&hook).output().unwrap();
                assert!(output.status.success(), "mkfifo: {output:?}");
            } else {
                fs::create_dir(&hook).unwrap();
            }
            let before = fs::symlink_metadata(&hook).unwrap();

            let output = install(dir.path());

            assert!(!output.status.success(), "{name}, fifo={fifo}: {output:?}");
            let after = fs::symlink_metadata(&hook).unwrap();
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.mode(), after.mode());
        }
    }
}

#[test]
fn install_hooks_refuses_symlinked_default_and_husky_parent_directories() {
    for husky in [false, true] {
        let dir = fixture();
        let root = dir.path();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let parent = if husky {
            git(root, &["config", "core.hooksPath", ".husky/_"]);
            fs::create_dir(target.join("_")).unwrap();
            root.join(".husky")
        } else {
            let hooks = root.join(".git/hooks");
            fs::remove_dir_all(&hooks).unwrap();
            hooks
        };
        symlink(&target, &parent).unwrap();
        let before = fs::read_dir(&target).unwrap().count();

        let output = install(root);

        assert!(!output.status.success(), "husky={husky}: {output:?}");
        assert_eq!(fs::read_link(&parent).unwrap(), target);
        assert_eq!(fs::read_dir(&target).unwrap().count(), before);
        for name in HOOKS {
            assert!(!target.join(name).exists());
        }
    }
}

#[test]
fn husky_exclude_links_are_refused_and_preserved() {
    for live in [false, true] {
        let dir = fixture();
        let root = dir.path();
        fs::create_dir(root.join(".husky")).unwrap();
        git(root, &["config", "core.hooksPath", ".husky/_"]);
        let target = root.join("exclude-target");
        let marker = format!("foreign exclude {}\n", root.display());
        if live {
            fs::write(&target, &marker).unwrap();
        }
        let exclude = root.join(".git/info/exclude");
        fs::remove_file(&exclude).unwrap();
        symlink(&target, &exclude).unwrap();

        let output = install(root);

        assert!(!output.status.success(), "live={live}: {output:?}");
        assert_eq!(fs::read_link(&exclude).unwrap(), target);
        if live {
            assert_eq!(fs::read_to_string(target).unwrap(), marker);
        } else {
            assert!(!target.exists());
        }
    }
}

#[test]
fn normal_default_and_husky_installs_preserve_foreign_hooks_and_refresh_owned_hooks() {
    for husky in [false, true] {
        let dir = fixture();
        let root = dir.path();
        let hooks = if husky {
            let hooks = root.join(".husky");
            fs::create_dir(&hooks).unwrap();
            git(root, &["config", "core.hooksPath", ".husky/_"]);
            hooks
        } else {
            root.join(".git/hooks")
        };
        let foreign = format!("#!/bin/sh\n# foreign {}\nexit 0\n", root.display());
        fs::write(hooks.join("post-commit"), &foreign).unwrap();
        fs::write(hooks.join("pre-commit"), &foreign).unwrap();
        fs::write(
            hooks.join("post-merge"),
            "# codesage install-hooks\nold hook\n",
        )
        .unwrap();
        let output = install(root);
        assert!(output.status.success(), "husky={husky}: {output:?}");
        assert_eq!(
            fs::read_to_string(hooks.join("post-commit")).unwrap(),
            foreign
        );
        assert_eq!(
            fs::read_to_string(hooks.join("pre-commit")).unwrap(),
            foreign
        );
        for name in ["post-merge", "post-checkout", "post-rewrite"] {
            let hook = hooks.join(name);
            let body = fs::read_to_string(&hook).unwrap();
            assert!(body.contains("codesage install-hooks"), "{name}: {body}");
            assert!(!body.contains("old hook"));
            assert_eq!(
                fs::metadata(hook).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let own = hooks.join("post-merge");
        let inode = fs::metadata(&own).unwrap().ino();
        let output = install(root);
        assert!(output.status.success(), "idempotent install: {output:?}");
        assert_eq!(fs::metadata(&own).unwrap().ino(), inode);
        if husky {
            let exclude = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
            for name in ["post-merge", "post-checkout", "post-rewrite"] {
                assert_eq!(
                    exclude
                        .lines()
                        .filter(|line| *line == format!("/.husky/{name}"))
                        .count(),
                    1
                );
            }
        }
    }
}

#[test]
fn normal_install_produces_all_five_executable_hooks() {
    let dir = fixture();
    let output = install(dir.path());
    assert!(output.status.success(), "{output:?}");
    for name in HOOKS {
        let path = dir.path().join(".git/hooks").join(name);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("codesage install-hooks")
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[test]
fn installed_leak_check_hook_executes_the_opted_in_repository_script() {
    let dir = fixture();
    let script = dir.path().join("scripts/leak-check.sh");
    fs::write(&script, "#!/bin/sh\nprintf 'ran\\n' > leak-ran\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let output = install(dir.path());
    assert!(output.status.success(), "{output:?}");
    let output = command(dir.path().join(".git/hooks/pre-commit"), dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(dir.path().join("leak-ran")).unwrap(), b"ran\n");
}

#[test]
fn absolute_husky_runtime_paths_install_in_their_user_directory() {
    let dir = fixture();
    let external = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let runtime = external.path().join("_");
    fs::create_dir(&runtime).unwrap();
    fs::write(runtime.join("h"), b"#!/bin/sh\n").unwrap();
    git(
        dir.path(),
        &["config", "core.hooksPath", runtime.to_str().unwrap()],
    );
    let output = install(dir.path());
    assert!(output.status.success(), "{output:?}");
    for name in HOOKS {
        assert!(external.path().join(name).is_file());
        assert!(!runtime.join(name).exists());
    }
}

#[test]
fn safe_husky_configuration_does_not_touch_an_unused_symlinked_default_directory() {
    let dir = fixture();
    let root = dir.path();
    let target = root.join("unused-default");
    fs::create_dir(&target).unwrap();
    let defaults = root.join(".git/hooks");
    fs::remove_dir_all(&defaults).unwrap();
    symlink(&target, &defaults).unwrap();
    fs::create_dir(root.join(".husky")).unwrap();
    git(root, &["config", "core.hooksPath", ".husky/_"]);

    let output = install(root);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read_link(&defaults).unwrap(), target);
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    for name in HOOKS {
        assert!(root.join(".husky").join(name).is_file());
    }
}

#[test]
fn husky_exclude_updates_preserve_non_utf8_bytes_and_permissions() {
    let dir = fixture();
    let root = dir.path();
    fs::create_dir(root.join(".husky")).unwrap();
    git(root, &["config", "core.hooksPath", ".husky/_"]);
    let exclude = root.join(".git/info/exclude");
    let existing = b"\xff# preserved\n";
    fs::write(&exclude, existing).unwrap();
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o640)).unwrap();
    let output = install(root);
    assert!(output.status.success(), "{output:?}");
    let bytes = fs::read(&exclude).unwrap();
    assert!(bytes.starts_with(existing));
    assert_eq!(fs::metadata(&exclude).unwrap().mode() & 0o777, 0o640);
}

#[test]
fn configured_parent_segments_preserve_traversal_and_refuse_link_components() {
    let dir = fixture();
    let root = dir.path();
    fs::create_dir(root.join(".git/hooks/nested")).unwrap();
    git(
        root,
        &["config", "core.hooksPath", ".git/hooks/nested/../."],
    );
    let output = install(root);
    assert!(
        output.status.success(),
        "ordinary parent traversal: {output:?}"
    );

    let link = root.join(".git/hooks/link");
    symlink(root.join(".git/hooks/nested"), &link).unwrap();
    git(root, &["config", "core.hooksPath", ".git/hooks/link/../."]);
    let output = install(root);
    assert!(
        !output.status.success(),
        "symlink before parent traversal: {output:?}"
    );
    assert!(link.is_symlink());
}

#[test]
fn unrelated_custom_hooks_path_stays_refused() {
    let dir = fixture();
    let custom = dir.path().join("custom");
    fs::create_dir(&custom).unwrap();
    git(dir.path(), &["config", "core.hooksPath", "custom"]);
    let output = install(dir.path());
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not look like a Husky setup"));
    assert_eq!(fs::read_dir(custom).unwrap().count(), 0);
}

#[test]
fn install_hooks_refuses_dangling_and_live_symlink_leaves() {
    for (name, husky) in HOOKS
        .into_iter()
        .flat_map(|name| [(name, false), (name, true)])
    {
        for live in [false, true] {
            let dir = fixture();
            let root = dir.path();
            let hooks = if husky {
                fs::create_dir(root.join(".husky")).unwrap();
                git(root, &["config", "core.hooksPath", ".husky/_"]);
                root.join(".husky")
            } else {
                root.join(".git/hooks")
            };
            let target = root.join("target");
            let marker = format!("codesage install-hooks\n{} {name}\n", root.display());
            if live {
                fs::write(&target, &marker).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
            }
            let hook = hooks.join(name);
            symlink(&target, &hook).unwrap();

            let output = install(root);

            assert!(
                !output.status.success(),
                "{name}, husky={husky}, live={live}: {output:?}"
            );
            assert_eq!(fs::read_link(&hook).unwrap(), target);
            if live {
                assert_eq!(fs::read_to_string(&target).unwrap(), marker);
                assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    0o640
                );
            } else {
                assert!(!target.exists(), "dangling {name} target was created");
            }
        }
    }
}
