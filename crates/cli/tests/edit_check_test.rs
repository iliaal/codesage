use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Session {
    child: Child,
    responses: Receiver<Value>,
    id: u64,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesage"))
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx
                    .send(serde_json::from_str(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            responses,
            id: 0,
        };
        session.request("initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"edit-check-test", "version":"1"}}));
        writeln!(
            session.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .unwrap();
        session
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params})
        )
        .unwrap();
        loop {
            let response = self
                .responses
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }
}

fn run(root: &Path, executable: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(executable)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn mcp_reports_break_before_writing_without_touching_index_or_starting_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = "mod api { pub fn f(a: i32) {} }\nfn caller() { self::api::f(1); }\n";
    std::fs::write(root.join("lib.rs"), source).unwrap();
    run(root, "git", &["init", "-q"]);
    run(root, "git", &["add", "lib.rs"]);
    run(
        root,
        "git",
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    let head = run(root, "git", &["rev-parse", "HEAD"]);
    let git_index = std::fs::read(root.join(".git/index")).unwrap();
    let mut session = Session::start();
    let tools = session.request("tools/list", json!({}));
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "edit_check")
        .unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert!(tool["outputSchema"]["properties"]["incompatible_callers"].is_object());
    for indexed in [false, true] {
        if indexed {
            run(root, env!("CARGO_BIN_EXE_codesage"), &["init"]);
            run(
                root,
                env!("CARGO_BIN_EXE_codesage"),
                &["index", "--no-semantic"],
            );
            std::fs::write(
                root.join("lib.rs"),
                "fn dirty_unique_unindexed_symbol() {}\n",
            )
            .unwrap();
        }
        let before_source = std::fs::read(root.join("lib.rs")).unwrap();
        let before_index = std::fs::read(root.join(".codesage/index.db")).ok();
        let result = session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"f", "replacement":"fn f() {}"}}));
        assert_ne!(result["isError"], true, "{result}");
        let report = &result["structuredContent"];
        assert_eq!(
            report["incompatible_callers"].as_array().unwrap().len(),
            1,
            "{report}"
        );
        assert_eq!(report["arity_changed"], true);
        assert_eq!(report["visibility_changed"], true);
        assert_eq!(report["worktree_matches_head"], !indexed);
        assert_eq!(report["next"], json!([]));
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(std::fs::read(root.join("lib.rs")).unwrap(), before_source);
        assert_eq!(
            std::fs::read(root.join(".codesage/index.db")).ok(),
            before_index
        );
        if !indexed {
            assert!(!root.join(".codesage").exists());
        }
        assert!(!root.join(".codesage/watch.status").exists());
        assert_eq!(run(root, "git", &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), git_index);
    }
    let traversal = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"../lib.rs", "symbol_name":"f", "replacement":"fn f() {}"}})));
    assert_eq!(traversal["error"]["code"], "E_PARAM", "{traversal}");
    assert_eq!(traversal["error"]["remedy"], Value::Null, "{traversal}");
    assert!(
        traversal["error"]["message"]
            .as_str()
            .unwrap()
            .contains("without traversal"),
        "{traversal}"
    );

    let relative = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":"relative/path", "file_path":"lib.rs", "symbol_name":"f", "replacement":"fn f() {}"}})));
    assert_eq!(relative["error"]["code"], "E_PROJECT_PATH", "{relative}");
    assert_eq!(relative["error"]["remedy"], Value::Null, "{relative}");
    assert!(
        relative["error"]["message"]
            .as_str()
            .unwrap()
            .contains("absolute"),
        "{relative}"
    );

    let oversize = "x".repeat(1_048_576 + 10);
    let over_cap = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"f", "replacement":oversize}})));
    assert_eq!(over_cap["error"]["code"], "E_OVER_CAP", "{over_cap}");
    assert_eq!(over_cap["error"]["remedy"], Value::Null, "{over_cap}");
    assert!(
        over_cap["error"]["message"]
            .as_str()
            .unwrap()
            .contains("replacement exceeds 1 MiB"),
        "{over_cap}"
    );

    let missing = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"absent", "replacement":"fn absent() {}"}})));
    assert_eq!(missing["error"]["code"], "E_NOT_FOUND", "{missing}");

    let renamed = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"f", "replacement":"fn g() {}"}})));
    assert_eq!(renamed["error"]["code"], "E_PARAM", "{renamed}");
    assert!(
        renamed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retain the selected symbol name"),
        "{renamed}"
    );

    let broken = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "symbol_name":"f", "replacement":"fn f( {"}})));
    assert_eq!(broken["error"]["code"], "E_PARAM", "{broken}");

    let not_at_head = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"nope.rs", "symbol_name":"f", "replacement":"fn f() {}"}})));
    assert_eq!(not_at_head["error"]["code"], "E_NOT_FOUND", "{not_at_head}");
    assert!(
        not_at_head["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not at Git HEAD"),
        "{not_at_head}"
    );
    assert_eq!(missing["error"]["remedy"], Value::Null, "{missing}");

    std::fs::write(root.join("dup.rs"), "fn f() {}\nmod inner { fn f() {} }\n").unwrap();
    std::fs::write(root.join("broken.rs"), "fn f( {\n").unwrap();
    run(root, "git", &["add", "dup.rs", "broken.rs"]);
    run(
        root,
        "git",
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "duplicate declarations",
        ],
    );
    let unparsable = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"broken.rs", "symbol_name":"f", "replacement":"fn f() {}"}})));
    assert_eq!(unparsable["error"]["code"], "E_INCOMPLETE", "{unparsable}");
    assert_eq!(unparsable["status"], "incomplete", "{unparsable}");
    assert_eq!(unparsable["error"]["remedy"], Value::Null, "{unparsable}");

    let ambiguous = failure(&session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"dup.rs", "symbol_name":"f", "replacement":"fn f() {}"}})));
    assert_eq!(ambiguous["error"]["code"], "E_AMBIGUOUS", "{ambiguous}");
    assert_eq!(
        ambiguous["error"]["remedy"],
        json!({"tool": "edit_check", "arguments": {"project": root, "file_path": "dup.rs", "symbol_name": "f", "replacement": "fn f() {}", "line": 1}}),
        "{ambiguous}"
    );
    assert_eq!(ambiguous["next"].as_array().unwrap().len(), 2);
    for next in ambiguous["next"].as_array().unwrap() {
        assert_eq!(next["tool"], "edit_check");
        assert_eq!(next["arguments"]["replacement"], "fn f() {}");
        assert!(next["arguments"].get("symbol_name").is_none());
        let selected = session.request(
            "tools/call",
            json!({"name":"edit_check", "arguments":next["arguments"]}),
        );
        assert_ne!(selected["isError"], true, "{selected}");
        assert_eq!(
            selected["structuredContent"]["line"],
            next["arguments"]["line"]
        );
    }
    let disambiguated = session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"dup.rs", "symbol_name":"f", "replacement":"fn f() {}", "line": 2}}));
    assert_ne!(disambiguated["isError"], true, "{disambiguated}");
}

/// The contract block of a failed `edit_check` result; mirrors `failure` in
/// `mcp_errors.rs` for the fields this fixture checks.
fn failure(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.get("structuredContent").is_none(), "{result}");
    let content = result["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let blocks: Vec<Value> = content
        .iter()
        .filter_map(|part| part["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .collect();
    assert_eq!(blocks.len(), 1, "one contract block per failure: {result}");
    let block = blocks.into_iter().next().unwrap();
    assert_eq!(block["tool"], "edit_check", "{block}");
    assert_eq!(block["complete"], false, "{block}");
    if block["error"]["code"] == "E_AMBIGUOUS" {
        assert!(!block["next"].as_array().unwrap().is_empty(), "{block}");
    } else {
        assert_eq!(block["next"], json!([]), "{block}");
    }
    assert_eq!(block["error"]["message"], text, "{block}");
    block
}

fn cli_fixture(source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), source).unwrap();
    run(dir.path(), "git", &["init", "-q"]);
    run(dir.path(), "git", &["add", "lib.rs"]);
    run(
        dir.path(),
        "git",
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    dir
}

fn cli(root: &Path, args: &[&str], input: &[u8]) -> Output {
    cli_with_env(root, args, input, &[], &[])
}

fn cli_with_env(
    root: &Path,
    args: &[&str],
    input: &[u8],
    env: &[(&str, &Path)],
    removed: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    for name in removed {
        command.env_remove(name);
    }
    let mut child = command
        .current_dir(root)
        .args(args)
        .envs(env.iter().copied())
        .env("CODESAGE_WATCH", "1")
        .env("ORT_DYLIB_PATH", root.join("missing-ort-library"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn cli_report(output: Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn cli_error(output: Output, code: i32, message: &str) {
    assert_eq!(output.status.code(), Some(code));
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

#[test]
fn cli_reads_file_and_stdin_with_full_mcp_payload_parity() {
    let dir = cli_fixture("mod api { pub fn f(a: i32) {} }\nfn caller() { self::api::f(1); }\n");
    let root = dir.path();
    let cwd = root.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(cwd.join(".codesage")).unwrap();
    let replacement = "fn f() {}";
    std::fs::write(cwd.join("replacement.rs"), replacement).unwrap();
    let source_before = std::fs::read(root.join("lib.rs")).unwrap();
    let git_index_before = std::fs::read(root.join(".git/index")).unwrap();
    let head = String::from_utf8(run(root, "git", &["rev-parse", "HEAD"])).unwrap();
    let from_file = cli_report(cli(
        &cwd,
        &["edit-check", "lib.rs", "f", "replacement.rs", "--json"],
        b"",
    ));
    assert_eq!(from_file["head"], head.trim());
    assert_eq!(from_file["arity_changed"], true);
    assert_eq!(from_file["visibility_changed"], true);
    assert_eq!(
        from_file["incompatible_callers"][0]["expression"],
        "self::api::f"
    );
    assert_eq!(from_file["worktree_matches_head"], true);
    for args in [
        vec!["edit-check", "lib.rs", "f", "-", "--json"],
        vec!["edit-check", "lib.rs", "sym:lib.rs#api::f@1", "--json"],
    ] {
        assert_eq!(
            cli_report(cli(&cwd, &args, replacement.as_bytes())),
            from_file
        );
    }
    let mut session = Session::start();
    let result = session.request("tools/call", json!({"name":"edit_check", "arguments":{"project":root, "file_path":"lib.rs", "target":"f", "replacement":replacement}}));
    assert_ne!(result["isError"], true, "{result}");
    let mut mcp_payload = result["structuredContent"].clone();
    for key in ["tool", "cost", "completeness", "next"] {
        mcp_payload.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(from_file, mcp_payload);
    assert_eq!(std::fs::read(root.join("lib.rs")).unwrap(), source_before);
    assert_eq!(
        std::fs::read(root.join(".git/index")).unwrap(),
        git_index_before
    );
    assert!(!root.join(".codesage").exists());
}

#[test]
fn cli_uses_head_despite_divergent_worktree_and_unusable_index() {
    let dir = cli_fixture("pub fn f(a: i32) {}\n");
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "fn uncommitted() {}\n").unwrap();
    std::fs::create_dir(root.join(".codesage")).unwrap();
    let sentinel = format!("unreadable-index-sentinel:{}", root.display());
    std::fs::write(root.join(".codesage/index.db"), &sentinel).unwrap();
    std::fs::write(root.join(".codesage/config.toml"), "invalid = [").unwrap();
    let report = cli_report(cli(
        root,
        &["edit-check", "lib.rs", "f", "--json"],
        b"fn f() {}",
    ));
    assert_eq!(report["before"]["arity"]["minimum"], 1);
    assert_eq!(report["worktree_matches_head"], false);
    assert!(report["unknown"].as_array().unwrap().iter().any(|reason| {
        reason
            .as_str()
            .unwrap()
            .contains("working file differs from HEAD")
    }));
    assert_eq!(
        std::fs::read_to_string(root.join("lib.rs")).unwrap(),
        "fn uncommitted() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".codesage/index.db")).unwrap(),
        sentinel
    );
    assert!(!root.join(".codesage/watch.status").exists());
}

#[test]
fn cli_replacement_byte_boundary_applies_to_files_and_stdin() {
    let dir = cli_fixture("fn f() {}");
    let root = dir.path();
    let mut replacement = b"fn f() {}".to_vec();
    replacement.resize(1_048_576, b' ');
    let at_limit = cli_report(cli(
        root,
        &["edit-check", "lib.rs", "f", "--json"],
        &replacement,
    ));
    assert_eq!(at_limit["symbol"], "f");
    std::fs::write(root.join("replacement.rs"), &replacement).unwrap();
    assert_eq!(
        cli_report(cli(
            root,
            &["edit-check", "lib.rs", "f", "replacement.rs", "--json"],
            b"",
        )),
        at_limit
    );
    replacement.push(b' ');
    std::fs::write(root.join("replacement.rs"), &replacement).unwrap();
    for (args, input) in [
        (
            vec!["edit-check", "lib.rs", "f", "--json"],
            replacement.as_slice(),
        ),
        (
            vec!["edit-check", "lib.rs", "f", "replacement.rs", "--json"],
            b"".as_slice(),
        ),
    ] {
        cli_error(cli(root, &args, input), 1, "replacement exceeds 1 MiB");
    }
}

#[test]
fn cli_refuses_bad_replacements_and_paths() {
    let dir = cli_fixture("fn f() {}\n");
    let root = dir.path();
    for (input, message) in [
        (b"".as_slice(), "replacement is empty"),
        (b" \n\t".as_slice(), "replacement is empty"),
        (b"\xff".as_slice(), "replacement must be UTF-8"),
        (b"fn f( {".as_slice(), "replacement produces syntax errors"),
        (b"fn g() {}".as_slice(), "retain the selected symbol name"),
    ] {
        cli_error(
            cli(root, &["edit-check", "lib.rs", "f", "--json"], input),
            1,
            message,
        );
    }
    std::fs::write(root.join("replacement.rs"), b"\xff").unwrap();
    cli_error(
        cli(
            root,
            &["edit-check", "lib.rs", "f", "replacement.rs", "--json"],
            b"",
        ),
        1,
        "replacement must be UTF-8",
    );
    cli_error(
        cli(
            root,
            &["edit-check", "lib.rs", "f", "absent.rs", "--json"],
            b"",
        ),
        1,
        "opening replacement from absent.rs",
    );
    cli_error(
        cli(
            root,
            &["edit-check", "../lib.rs", "f", "--json"],
            b"fn f() {}",
        ),
        1,
        "without traversal",
    );
    cli_error(
        cli(
            root,
            &["edit-check", "missing.rs", "f", "--json"],
            b"fn f() {}",
        ),
        1,
        "not at Git HEAD",
    );
    cli_error(
        cli(
            root,
            &["edit-check", "lib.rs", "sym:other.rs#f", "--json"],
            b"fn f() {}",
        ),
        1,
        "handle names `other.rs`",
    );
    let Some(outside) = repository_free_dir() else {
        return skip("the outside case: no candidate directory where Git finds no repository");
    };
    cli_error(
        cli(
            outside.path(),
            &["edit-check", "lib.rs", "f", "--json"],
            b"",
        ),
        1,
        "no Git worktree found",
    );
}

/// The variables `codesage_graph::git_command` strips from every Git child.
const STRIPPED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_REFERENCE_BACKEND",
];

/// A fresh directory from which Git, run as CodeSage runs it, finds no
/// repository in any parent directory. The system temp directory can sit
/// under a repository or on its own mount, where Git stops at the boundary
/// with a different refusal, so `~/.cache` is tried too.
fn repository_free_dir() -> Option<tempfile::TempDir> {
    let cache = std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache"));
    for base in std::iter::once(std::env::temp_dir()).chain(cache) {
        if std::fs::create_dir_all(&base).is_err() {
            continue;
        }
        let Ok(dir) = tempfile::Builder::new()
            .prefix("codesage-discovery-")
            .tempdir_in(&base)
        else {
            continue;
        };
        let Some(output) = probe_git(dir.path()) else {
            continue;
        };
        if output.status.code() == Some(128)
            && String::from_utf8_lossy(&output.stderr).lines().any(|line| {
                line.starts_with("fatal: not a git repository (or any of the parent directories)")
            })
        {
            return Some(dir);
        }
    }
    None
}

/// `git rev-parse --show-toplevel` from `dir`, run as CodeSage runs it.
fn probe_git(dir: &Path) -> Option<Output> {
    let mut probe = Command::new("git");
    probe
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    for name in STRIPPED_GIT_ENV {
        probe.env_remove(name);
    }
    probe.output().ok()
}

/// A missing precondition fails under CI, where it would hide a regression,
/// and is reported as a skip on a developer host.
fn skip(reason: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "precondition missing under CI: {reason}"
    );
    eprintln!("skipping {reason}");
}

/// `safe.directory` is honored only from system, global, and command-line or
/// environment-scoped config, so dropping all of them keeps a
/// `safe.directory = *` from overriding the simulated foreign owner.
const FOREIGN_OWNER: [(&str, &str); 3] = [
    ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// Environment-scoped config that could carry `safe.directory = *`.
const ENV_SCOPED_CONFIG: [&str; 2] = ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"];

fn foreign_owner_env() -> Vec<(&'static str, &'static Path)> {
    FOREIGN_OWNER
        .iter()
        .map(|&(name, value)| (name, Path::new(value)))
        .collect()
}

#[test]
fn cli_reports_git_refusal_instead_of_a_missing_worktree() {
    let dir = cli_fixture("fn f() {}\n");
    let output = cli_with_env(
        dir.path(),
        &["edit-check", "lib.rs", "f", "--json"],
        b"fn f() {}",
        &foreign_owner_env(),
        &ENV_SCOPED_CONFIG,
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("dubious ownership"), "{stderr}");
    assert!(!stderr.contains("no Git worktree found"), "{stderr}");
}

#[test]
fn cli_runs_from_inside_the_git_directory() {
    let dir = cli_fixture("pub fn f(a: i32) {}\n");
    let report = cli_report(cli(
        &dir.path().join(".git"),
        &["edit-check", "lib.rs", "f", "--json"],
        b"fn f() {}",
    ));
    assert_eq!(report["arity_changed"], true);
}

#[test]
fn cli_ignores_an_ancestor_git_entry_that_git_rejects() {
    let Some(base) = repository_free_dir() else {
        return skip("no candidate directory where Git finds no repository");
    };
    let base_path = base.path().canonicalize().unwrap();
    std::fs::create_dir(base_path.join(".git")).unwrap();
    let cwd = base_path.join("work").join("nested");
    std::fs::create_dir_all(&cwd).unwrap();
    cli_error(
        cli(&cwd, &["edit-check", "lib.rs", "f", "--json"], b""),
        1,
        "no Git worktree found",
    );
}

/// Unlike the test above, this keeps the system temp directory, which may be
/// its own mount: Git then stops at the boundary, and the stray entry must
/// still fail the structural fallback.
#[test]
fn cli_ignores_a_stray_git_entry_in_the_system_temp_dir() {
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    if probe_git(&base_path).is_none_or(|output| output.status.success()) {
        return skip("the system temp directory lies inside a Git worktree");
    }
    std::fs::create_dir(base_path.join(".git")).unwrap();
    let cwd = base_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    cli_error(
        cli(&cwd, &["edit-check", "lib.rs", "f", "--json"], b""),
        1,
        "no Git worktree found",
    );
}

#[test]
fn cli_outside_a_core_worktree_takes_the_git_holder_as_root() {
    let dir = cli_fixture("pub fn f(a: i32) {}\n");
    let root = dir.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    run(
        root,
        "git",
        &[
            "config",
            "core.worktree",
            root.join("sub").to_str().unwrap(),
        ],
    );
    cli_error(
        cli(root, &["edit-check", "lib.rs", "f", "--json"], b"fn f() {}"),
        1,
        "project must be the Git worktree root",
    );
}

#[test]
fn cli_resolves_a_linked_worktree_from_a_subdirectory() {
    let dir = cli_fixture("pub fn f(a: i32) {}\n");
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    run(
        dir.path(),
        "git",
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    let cwd = linked.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let report = cli_report(cli(
        &cwd,
        &["edit-check", "lib.rs", "f", "--json"],
        b"fn f() {}",
    ));
    assert_eq!(report["arity_changed"], true);
    assert_eq!(report["visibility_changed"], true);
}

#[test]
fn cli_disambiguates_by_head_line_or_handle() {
    let dir = cli_fixture("fn f() {}\nmod inner { fn f() {} }\n");
    let root = dir.path();
    cli_error(
        cli(root, &["edit-check", "lib.rs", "f", "--json"], b"fn f() {}"),
        1,
        "starting at lines 1, 2",
    );
    cli_error(
        cli(
            root,
            &["edit-check", "lib.rs", "f", "--line", "3", "--json"],
            b"fn f() {}",
        ),
        1,
        "starting at line 3",
    );
    for args in [
        vec!["edit-check", "lib.rs", "f", "--line", "2", "--json"],
        vec!["edit-check", "lib.rs", "sym:lib.rs#inner::f@2", "--json"],
    ] {
        assert_eq!(cli_report(cli(root, &args, b"fn f() {}"))["line"], 2);
    }
    cli_error(
        cli(
            root,
            &["edit-check", "lib.rs", "f", "--line", "0", "--json"],
            b"",
        ),
        2,
        "positive integer",
    );
}

#[test]
fn cli_text_mode_exposes_limits_and_discovery_help() {
    let dir = cli_fixture("pub fn f(a: i32) {}\n");
    let output = cli(dir.path(), &["edit-check", "lib.rs", "f"], b"fn f() {}");
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("changed: arity=true, visibility=true"),
        "{text}"
    );
    assert!(text.contains("unknown:"), "{text}");
    assert!(
        text.contains("not a compilation or safety verdict"),
        "{text}"
    );
    let help = cli(dir.path(), &["edit-check", "--help"], b"");
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8(help.stdout).unwrap();
    for required in [
        "Git worktree root",
        "relative to the current directory",
        "Exit 0",
        "exit 1",
        "exit 2",
        "--json",
    ] {
        assert!(text.contains(required), "missing {required}: {text}");
    }
}
