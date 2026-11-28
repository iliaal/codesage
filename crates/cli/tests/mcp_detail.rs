use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Server {
    child: Child,
    responses: Receiver<String>,
    id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Self {
        Self::start_mode(false)
    }

    fn start_mode(legacy: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
        command
            .args(["mcp", "--direct"])
            .env("CODESAGE_WATCH", "0")
            .env("CODESAGE_DIAGNOSTICS", "1")
            .env_remove("CODESAGE_ENVELOPE")
            .env_remove("CODESAGE_MCP_TEST_QUERY_EMBEDDING")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if legacy {
            command.env("CODESAGE_ENVELOPE", "legacy");
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            responses,
            id: 0,
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "detail-test", "version": "1"}
            }),
        );
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        )
        .unwrap();
        loop {
            let line = self
                .responses
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }

    fn raw_call(&mut self, root: &Path, tool: &str, mut args: Value) -> Value {
        args["project"] = json!(root);
        self.request("tools/call", json!({"name": tool, "arguments": args}))
    }

    fn call(&mut self, root: &Path, tool: &str, args: Value) -> Value {
        let result = self.raw_call(root, tool, args);
        assert_ne!(result["isError"], true, "{result}");
        let value = result["structuredContent"].clone();
        assert_eq!(value["cost"]["bytes"], value.to_string().len(), "{tool}");
        assert!(value["cost"]["ms"].is_u64(), "{tool}");
        value
    }
}

fn command(root: &Path, binary: &str, args: &[&str]) {
    let output = Command::new(binary)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_reference_budget(result: &Value, tokens: usize, exceeded: bool) {
    assert_eq!(result["cost"]["budget_tokens"], tokens);
    assert_eq!(result.to_string().len() > tokens * 4, exceeded);
    if exceeded {
        assert_eq!(result["cost"]["budget_exceeded"], true);
    } else {
        assert!(result["cost"].get("budget_exceeded").is_none());
    }
}

fn reference_budget_recovery(result: &Value, legacy: bool) -> &Value {
    if legacy {
        assert_eq!(result["_meta"]["truncated"], true);
        &result["_meta"]
    } else {
        assert_eq!(result["completeness"]["kind"], "truncated");
        &result["completeness"]["recover"]
    }
}

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let source = format!(
        "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
        "    leaf();\n".repeat(100)
    );
    std::fs::write(root.join("src/lib.rs"), &source).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='detail_fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    command(root, "git", &["init", "-q"]);
    command(root, "git", &["add", "src/lib.rs", "Cargo.toml"]);
    command(
        root,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    for revision in 1..=3 {
        std::fs::write(
            root.join("src/lib.rs"),
            format!("{source}// revision {revision}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("src/helper.rs"),
            format!("pub fn helper() -> u32 {{ {revision} }}\n"),
        )
        .unwrap();
        command(root, "git", &["add", "src/lib.rs", "src/helper.rs"]);
        command(
            root,
            "git",
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "update",
            ],
        );
    }
    command(root, env!("CARGO_BIN_EXE_codesage"), &["init"]);
    command(
        root,
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    command(
        root,
        env!("CARGO_BIN_EXE_codesage"),
        &["git-index", "--full"],
    );
}

#[test]
fn compact_hundred_reference_response_is_under_four_kib_and_recovers_all_rows() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let standard = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","detail":"standard"}),
    );
    assert_eq!(standard["results"].as_array().unwrap().len(), 100);
    let compact = server.raw_call(root.path(), "find_references", json!({"name":"leaf"}));
    assert_ne!(compact["isError"], true, "{compact}");
    let payload = &compact["structuredContent"];
    assert_eq!(payload["cost"]["detail"], "compact");
    assert!(
        compact.to_string().len() < 4096,
        "{} bytes: {compact}",
        compact.to_string().len()
    );
    assert_eq!(payload["cost"]["bytes"], payload.to_string().len());
    let rows = payload["results"].as_array().unwrap();
    assert!(rows.len() >= 3 && rows.len() < 100, "{payload}");
    assert_eq!(payload["completeness"]["kind"], "truncated");
    assert_eq!(
        payload["completeness"]["recover"]["trimmed"][0]["omitted"],
        100 - rows.len()
    );
    for row in rows {
        assert_eq!(row["from"], "sym:src/lib.rs#caller");
        assert_eq!(row["to"], "sym:src/lib.rs#leaf");
        assert_eq!(row["kind"], "call");
        assert!(row.get("content").is_none());
    }
    assert_eq!(
        &standard["results"].as_array().unwrap()[..rows.len()],
        rows.as_slice()
    );
    let recovery = &payload["completeness"]["recover"];
    let recovered = server.call(
        root.path(),
        recovery["tool"].as_str().unwrap(),
        recovery["arguments"].clone(),
    );
    assert_eq!(recovered["results"], standard["results"]);
}

#[test]
fn capped_mcp_references_keep_production_callers_first() {
    let root = tempfile::tempdir().unwrap();
    let anchor_calls = "    anchor();\n".repeat(120);
    let test_calls = "    test_only_anchor();\n".repeat(120);
    let mixed_calls = "    mixed_anchor();\n".repeat(20);
    std::fs::write(
        root.path().join("lib.rs"),
        format!(
            "pub fn anchor() {{}}\npub fn test_only_anchor() {{}}\n\
             pub fn mixed_anchor() {{}}\npub fn small_anchor() {{}}\n\
             #[test]\nfn first_test() {{\n{anchor_calls}{test_calls}{mixed_calls}small_anchor();\n}}\n\
             pub fn first_product() {{ mixed_anchor(); small_anchor(); }}\n\
             #[test]\nfn second_test() {{\n{anchor_calls}{test_calls}{mixed_calls}small_anchor();\n}}\n\
             pub fn second_product() {{ anchor(); mixed_anchor(); small_anchor(); }}\n"
        ),
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_codesage");
    command(root.path(), binary, &["init"]);
    command(root.path(), binary, &["index", "--no-semantic"]);

    let cli_rows = |name: &str| {
        let output = Command::new(binary)
            .current_dir(root.path())
            .args(["find-references", name, "--kind", "call", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        result["results"].as_array().unwrap().clone()
    };
    let anchor = cli_rows("anchor");
    assert_eq!(anchor.len(), 241);
    assert!(anchor[..240].iter().all(|row| row["is_test"] == true));
    assert_eq!(anchor[240]["from_symbol"], "second_product");

    let mixed = cli_rows("mixed_anchor");
    assert_eq!(mixed.len(), 42);
    assert_eq!(mixed[0]["from_symbol"], "first_test");
    assert_eq!(mixed[20]["from_symbol"], "first_product");
    assert_eq!(mixed[21]["from_symbol"], "second_test");
    assert_eq!(mixed[41]["from_symbol"], "second_product");

    let tests = cli_rows("test_only_anchor");
    assert_eq!(tests.len(), 240);
    assert!(tests.iter().all(|row| row["is_test"] == true));
    let small = cli_rows("small_anchor");
    assert_eq!(small.len(), 4);
    assert_eq!(small[0]["from_symbol"], "first_test");

    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        for (args, original, production, budget) in [
            (json!({"name":"anchor","kind":"call"}), &anchor, 1, 462),
            (
                json!({"name":"anchor","kind":"call","detail":"standard","budget_tokens":8000}),
                &anchor,
                1,
                8000,
            ),
            (
                json!({"name":"mixed_anchor","kind":"call","detail":"standard","budget_tokens":800}),
                &mixed,
                2,
                800,
            ),
        ] {
            let result = server.call(root.path(), "find_references", args.clone());
            assert_reference_budget(&result, budget, false);
            let rows = result["results"].as_array().unwrap();
            assert!(
                rows.len() > production && rows.len() < original.len(),
                "{args}: {result}"
            );
            let (product, tests): (Vec<_>, Vec<_>) = original
                .iter()
                .cloned()
                .partition(|row| row["is_test"] != true);
            assert_eq!(product.len(), production);
            let expected: Vec<_> = product.into_iter().chain(tests).take(rows.len()).collect();
            assert_eq!(rows, &expected, "{args}");
            let recovery = reference_budget_recovery(&result, legacy);
            if let Some(trimmed) = recovery.pointer("/trimmed/0") {
                assert_eq!(trimmed["field"], "/results");
                assert_eq!(trimmed["omitted"], original.len() - rows.len(), "{args}");
                assert_eq!(trimmed["returned"], rows.len());
            } else {
                let total = if legacy { "total_results" } else { "total" };
                assert_eq!(recovery[total], original.len(), "{args}");
                assert_eq!(recovery["returned"], rows.len());
            }
        }

        let test_only = server.call(
            root.path(),
            "find_references",
            json!({"name":"test_only_anchor","kind":"call"}),
        );
        let rows = test_only["results"].as_array().unwrap();
        assert!(!rows.is_empty() && rows.len() < tests.len());
        assert_eq!(rows, &tests[..rows.len()]);

        let uncapped = server.call(
            root.path(),
            "find_references",
            json!({"name":"small_anchor","kind":"call","detail":"standard","budget_tokens":8000}),
        );
        assert_eq!(uncapped["results"], json!(small));
    }
}

fn full_reference_fixture(root: &Path) {
    let mut source = String::from("pub fn anchor() {}\n");
    for (attribute, name, comment) in [
        ("#[test]\n", "first_test", "a".repeat(2600)),
        ("", "first_product", "short".to_owned()),
        ("#[test]\n", "second_test", "b".repeat(2000)),
        ("", "second_product", "c".repeat(2300)),
        ("#[test]\n", "third_test", "d".repeat(2100)),
    ] {
        source.push_str(&format!(
            "{attribute}fn {name}() {{\n   anchor(); // {comment}\n}}\n"
        ));
    }
    std::fs::write(root.join("lib.rs"), source).unwrap();
    let binary = env!("CARGO_BIN_EXE_codesage");
    command(root, binary, &["init"]);
    command(root, binary, &["index", "--no-semantic"]);
}

fn call_full_references(server: &mut Server, root: &Path, budget: usize) -> Value {
    server.call(
        root,
        "find_references",
        json!({"name":"anchor","kind":"call","detail":"full","budget_tokens":budget}),
    )
}

fn assert_shortened_reference_snippets(
    result: &Value,
    recovery: &Value,
    original: &[Value],
) -> usize {
    let rows = result["results"].as_array().unwrap();
    let mut shortened = Vec::new();
    for (index, (row, original)) in rows.iter().zip(original).enumerate() {
        let mut unchanged = original.clone();
        let original_snippet = original["snippet"].as_str().unwrap();
        let snippet = row["snippet"].as_str().unwrap();
        if let Some(prefix) = snippet.strip_suffix("\n…[truncated by MCP budget]") {
            assert!(original_snippet.starts_with(prefix));
            assert!(prefix.len() < original_snippet.len());
            unchanged["snippet"] = json!(snippet);
            shortened.push(json!(format!("/results/{index}/snippet")));
        }
        assert_eq!(row, &unchanged);
    }
    if shortened.is_empty() {
        assert!(recovery.get("shortened").is_none(), "{recovery}");
    } else {
        assert_eq!(recovery["shortened"], json!(shortened), "{recovery}");
    }
    shortened.len()
}

#[test]
fn full_reference_budget_shortening_keeps_original_row_order() {
    let root = tempfile::tempdir().unwrap();
    full_reference_fixture(root.path());
    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        let original = call_full_references(&mut server, root.path(), 8000);
        let original = original["results"].as_array().unwrap();
        assert_eq!(original.len(), 5);
        assert_eq!(original[0]["from_symbol"], "first_test");
        assert_eq!(original[1]["from_symbol"], "first_product");

        let result = call_full_references(&mut server, root.path(), 1600);
        assert_reference_budget(&result, 1600, false);
        let recovery = reference_budget_recovery(&result, legacy);
        assert_eq!(result["results"].as_array().unwrap().len(), original.len());
        assert_eq!(recovery["trimmed"], json!([]));
        assert!(assert_shortened_reference_snippets(&result, recovery, original) > 0);
    }
}

#[test]
fn early_reference_budget_leaves_room_to_shorten_full_snippets() {
    let root = tempfile::tempdir().unwrap();
    let comment = "a".repeat(33_000);
    std::fs::write(
        root.path().join("lib.rs"),
        format!(
            "fn anchor() {{}}\n\
             #[test]\nfn test_one() {{ anchor(); }}\n\
             #[test]\nfn test_two() {{ anchor(); }}\n\
             #[test]\nfn test_three() {{ anchor(); }}\n\n\n\
             fn production() {{\n    anchor(); // {comment}\n}}\n"
        ),
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_codesage");
    command(root.path(), binary, &["init"]);
    command(root.path(), binary, &["index", "--no-semantic"]);

    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        let original = server.call(
            root.path(),
            "find_references",
            json!({"name":"anchor","kind":"call","detail":"standard","budget_tokens":8000}),
        );
        let original = original["results"].as_array().unwrap();
        assert_eq!(original.len(), 4);
        assert_eq!(original[0]["from_symbol"], "test_one");
        assert_eq!(original[3]["from_symbol"], "production");
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":5}));
        assert_eq!(stats["tools"]["find_references"]["executions"], 2);

        let result = call_full_references(&mut server, root.path(), 8000);
        assert_reference_budget(&result, 8000, false);
        let rows = result["results"].as_array().unwrap();
        assert_eq!(rows.len(), original.len());
        for (row, expected) in rows.iter().zip(original) {
            let mut facts = row.as_object().unwrap().clone();
            facts.remove("snippet").unwrap();
            assert_eq!(facts.remove("snippet_source").unwrap(), "working_tree");
            assert_eq!(&facts, expected.as_object().unwrap());
        }
        let snippet = rows[3]["snippet"].as_str().unwrap();
        assert!(snippet.starts_with("    anchor(); // aaa"));
        assert!(snippet.ends_with("\n…[truncated by MCP budget]"));
        assert!(snippet.len() < comment.len());
        let recovery = reference_budget_recovery(&result, legacy);
        assert_eq!(recovery["trimmed"], json!([]));
        assert_eq!(recovery["shortened"], json!(["/results/3/snippet"]));
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":5}));
        assert_eq!(stats["tools"]["find_references"]["requests"], 2);
        assert_eq!(stats["tools"]["find_references"]["executions"], 5);
        let request = &stats["recent_requests"][0];
        let execution = &stats["recent_executions"][0];
        assert_eq!(request["tool"], "find_references");
        assert_eq!(request["outcome"], "success");
        assert_eq!(request["execution_id"], execution["id"]);
        assert_eq!(
            execution["project"],
            json!(root.path().canonicalize().unwrap())
        );
        assert_eq!(execution["work_class"], "interactive");
        assert_eq!(execution["outcome"], "success");
        assert_eq!(stats["work"]["requests"], 0);
        assert_eq!(stats["work"]["running"], json!([0, 0, 0]));
    }
}

#[test]
fn full_reference_budget_caps_many_repeated_escaped_snippets() {
    let root = tempfile::tempdir().unwrap();
    let calls = "anchor(); ".repeat(32);
    let comment = "\"quoted\" \\\t雪".repeat(3500);
    let long_line = format!("    {calls}// {comment}");
    std::fs::write(
        root.path().join("lib.rs"),
        format!(
            "fn anchor() {{}}\n#[test]\nfn test_callers() {{\n{long_line}\n}}\n\
             fn production() {{ anchor(); }}\n"
        ),
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_codesage");
    command(root.path(), binary, &["init"]);
    command(root.path(), binary, &["index", "--no-semantic"]);

    let mut server = Server::start();
    let original = server.call(
        root.path(),
        "find_references",
        json!({"name":"anchor","kind":"call","detail":"standard","budget_tokens":8000}),
    );
    let original = original["results"].as_array().unwrap();
    assert_eq!(original.len(), 33);
    assert!(original[..32].iter().all(|row| row["is_test"] == true));
    assert_eq!(original[32]["from_symbol"], "production");

    let result = call_full_references(&mut server, root.path(), 8000);
    assert_reference_budget(&result, 8000, false);
    let rows = result["results"].as_array().unwrap();
    assert_eq!(rows.len(), original.len());
    for (row, expected) in rows.iter().zip(original) {
        let mut facts = row.as_object().unwrap().clone();
        let snippet = facts.remove("snippet").unwrap();
        assert_eq!(facts.remove("snippet_source").unwrap(), "working_tree");
        assert_eq!(&facts, expected.as_object().unwrap());
        if row["is_test"] == true {
            let prefix = snippet
                .as_str()
                .unwrap()
                .strip_suffix("\n…[truncated by MCP budget]")
                .unwrap();
            assert!(long_line.starts_with(prefix));
            assert!(prefix.contains("\t雪"));
        } else {
            assert_eq!(snippet, "fn production() { anchor(); }");
        }
    }
    let recovery = reference_budget_recovery(&result, false);
    assert_eq!(recovery["trimmed"], json!([]));
    let mut paths: Vec<_> = (0..32)
        .map(|index| format!("/results/{index}/snippet"))
        .collect();
    paths.sort();
    assert_eq!(recovery["shortened"], json!(paths));
}

#[test]
fn full_reference_budget_trim_identifies_retained_shortened_snippets() {
    let root = tempfile::tempdir().unwrap();
    full_reference_fixture(root.path());
    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        let original = call_full_references(&mut server, root.path(), 8000);
        let mut original = original["results"].as_array().unwrap().clone();
        assert_eq!(original.len(), 5);
        original.sort_by_key(|row| row["is_test"] == true);

        for (budget, exceeded) in [(600, false), (256, true)] {
            let result = call_full_references(&mut server, root.path(), budget);
            assert_reference_budget(&result, budget, exceeded);
            let rows = result["results"].as_array().unwrap();
            let recovery = reference_budget_recovery(&result, legacy);
            assert_eq!(
                recovery["trimmed"],
                json!([{"field":"/results", "returned":rows.len(), "omitted":original.len() - rows.len()}])
            );
            let shortened = assert_shortened_reference_snippets(&result, recovery, &original);
            if exceeded {
                assert_eq!(rows.len(), 1);
                assert_eq!(shortened, 0);
            } else {
                assert!((3..5).contains(&rows.len()));
                assert!(shortened > 0);
            }
        }
    }
}

#[test]
fn callable_signatures_reach_default_and_compact_stdio_responses() {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "api.c",
            "int *c_allocate(unsigned count) { return 0; }\nstatic int c_hidden(void) { return 1; }",
        ),
        (
            "api.cpp",
            "int cpp_collect(int count = 2) { return count; }",
        ),
        (
            "api.php",
            "<?php class Api { public static function php_fetch(string $key): ?array { return null; } private function hidden() {} }",
        ),
        (
            "api.py",
            "def py_fetch(key: str) -> list[str]:\n    return []\n",
        ),
        (
            "api.rs",
            "pub fn rust_fetch(key: &str) -> Option<String> { None }",
        ),
        (
            "api.go",
            "package api\nfunc GoFetch(left, right int) (string, error) { return \"\", nil }",
        ),
        (
            "Api.java",
            "public class Api { public static String javaFetch(int limit) { return null; } private void hidden() {} }",
        ),
        (
            "api.js",
            "export function jsFetch(key, limit = 2) { return key; }",
        ),
        (
            "api.ts",
            "export function tsFetch(key: string): string { return key; }",
        ),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    command(root.path(), env!("CARGO_BIN_EXE_codesage"), &["init"]);
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    for name in [
        "c_allocate",
        "cpp_collect",
        "php_fetch",
        "py_fetch",
        "rust_fetch",
        "GoFetch",
        "javaFetch",
        "jsFetch",
        "tsFetch",
    ] {
        let found = server.call(root.path(), "find_symbol", json!({"target": name}));
        let row = &found["results"][0];
        assert!(
            row["signature"]["declaration"]
                .as_str()
                .unwrap()
                .contains(name),
            "{found}"
        );
        assert!(
            !row["signature"]["parameters"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{found}"
        );
        let described = server.call(
            root.path(),
            "describe",
            json!({"target": row["handle"], "sections": ["identity"]}),
        );
        assert_eq!(described["cost"]["detail"], "compact");
        assert_eq!(
            described["card"]["sections"]["identity"]["data"]["signature"],
            row["signature"]
        );
        let compact = server.call(
            root.path(),
            "find_symbol",
            json!({"target": name, "detail": "compact"}),
        );
        assert_eq!(compact["results"][0]["signature"], row["signature"]);
    }
    for (path, name) in [
        ("api.c", "c_allocate"),
        ("api.php", "php_fetch"),
        ("Api.java", "javaFetch"),
    ] {
        let described = server.call(
            root.path(),
            "describe",
            json!({"target": format!("file:{path}"), "sections": ["public_api"]}),
        );
        let api = &described["card"]["sections"]["public_api"]["data"];
        assert_eq!(api["total"], 1, "{described}");
        assert!(
            api["symbols"][0]["signature"]["declaration"]
                .as_str()
                .unwrap()
                .contains(name),
            "{described}"
        );
    }
}

#[test]
fn describe_reads_legacy_signature_schema_without_migration() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let path = root.path().join(".codesage/index.db");
    let db = codesage_storage::Database::open(&path).unwrap();
    db.execute_raw_for_tests("ALTER TABLE symbols DROP COLUMN signature; ALTER TABLE symbols DROP COLUMN arity; ALTER TABLE symbols DROP COLUMN parameters; ALTER TABLE symbols DROP COLUMN return_type; DELETE FROM schema_migrations WHERE name = '0027_symbol_signatures';").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let mut server = Server::start();
    let result = server.call(
        root.path(),
        "describe",
        json!({"target":"sym:src/lib.rs#leaf", "sections":["identity"]}),
    );
    assert!(result["card"]["sections"]["identity"]["data"]["signature"].is_null());
    assert_eq!(result["card"]["handle"], "sym:src/lib.rs#leaf");
    drop(server);
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn detail_aliases_full_snippets_budgets_and_errors_work_over_stdio() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let compact = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"compact"}),
    );
    assert_eq!(
        compact["results"][0]["signature"]["declaration"],
        "pub fn leaf()"
    );
    let full = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"full"}),
    );
    assert!(
        full["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("pub fn leaf()")
    );
    assert_eq!(full["results"][0]["snippet_source"], "working_tree");
    for (alias, args, detail) in [
        (
            "assess_risk",
            json!({"targets":["src/lib.rs"],"verbose":true}),
            "full",
        ),
        (
            "assess_risk",
            json!({"targets":["src/lib.rs"],"detail":"full"}),
            "full",
        ),
        (
            "impact_analysis",
            json!({"target":"leaf","summary_only":true}),
            "compact",
        ),
        (
            "impact_analysis",
            json!({"target":"leaf","summary_only":false}),
            "standard",
        ),
    ] {
        let result = server.call(root.path(), alias, args);
        assert_eq!(result["cost"]["detail"], detail);
        if alias == "assess_risk" {
            let file = &result["files"][0];
            assert!(file.get("churn_score").is_some(), "{result}");
            assert!(file.get("top_coupled").is_some(), "{result}");
        }
    }
    let budget = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","detail":"standard","budget_tokens":800}),
    );
    assert!(budget.to_string().len() <= 3200, "{budget}");
    assert!(budget["results"].as_array().unwrap().len() >= 3);
    assert_eq!(budget["completeness"]["kind"], "truncated");
    let capped = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","budget_tokens":999999}),
    );
    assert_eq!(capped["cost"]["budget_tokens"], 8000);
    for args in [
        json!({"detail":"invalid"}),
        json!({"budget_tokens":0}),
        json!({"budget_tokens":1.5}),
        json!({"verbose":true,"detail":"compact"}),
    ] {
        let result = server.raw_call(root.path(), "assess_risk", args);
        assert_eq!(result["isError"], true);
        let error = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|block| {
                let value = serde_json::from_str::<Value>(block["text"].as_str()?).ok()?;
                value.get("error").is_some().then_some(value)
            })
            .unwrap();
        assert_eq!(error["error"]["code"], "E_PARAM");
        assert!(error["cost"]["ms"].is_u64());
        assert_eq!(error["cost"]["bytes"], error.to_string().len());
    }
}

#[test]
fn fitting_describe_rows_survive_a_small_requested_budget() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start();
    let normal = server.call(
        root.path(),
        "describe",
        json!({
            "target":"src/lib.rs", "detail":"standard", "sections":["symbols"]
        }),
    );
    assert_eq!(
        normal["card"]["sections"]["symbols"]["data"]["top"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        normal.to_string().len() < 1024,
        "fixture must fit smallest budget: {normal}"
    );
    for budget in [256, 400] {
        let bounded = server.call(root.path(), "describe", json!({
            "target":"src/lib.rs", "detail":"standard", "sections":["symbols"], "budget_tokens":budget
        }));
        assert_eq!(bounded["card"], normal["card"]);
        assert_eq!(bounded["completeness"], normal["completeness"], "{bounded}");
        assert!(
            bounded["cost"].get("budget_exceeded").is_none(),
            "{bounded}"
        );
        assert!(bounded.to_string().len() <= budget * 4);
    }
}

#[test]
fn final_reference_budget_keeps_legacy_and_unified_counts_consistent() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(500)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    for budget in [400, 800] {
        let result = server.call(
            root.path(),
            "find_references",
            json!({
                "name":"leaf", "detail":"standard", "budget_tokens":budget
            }),
        );
        let retained = result["results"].as_array().unwrap().len();
        assert!(retained > 0 && retained < 500);
        assert_eq!(result["_meta"]["total_results"], 500);
        assert_eq!(result["_meta"]["returned"], retained, "{result}");
        assert_eq!(
            result["completeness"]["prior"]["recover"]["returned"], retained,
            "{result}"
        );
        assert_eq!(
            result["completeness"]["recover"]["trimmed"][0]["omitted"],
            500 - retained
        );
        assert_eq!(
            result["completeness"]["recover"]["trimmed"][0]["returned"],
            retained
        );
    }
}

#[test]
fn full_snippets_shrink_before_rows_or_evidence_are_dropped() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{ let value = \"{}\"; }}\n",
            "é".repeat(40_000)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let standard = server.call(
        root.path(),
        "find_symbol",
        json!({"name":"leaf","detail":"standard"}),
    );
    let result = server.call(
        root.path(),
        "find_symbol",
        json!({
            "name":"leaf", "detail":"full", "budget_tokens":800
        }),
    );
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["results"][0]["handle"],
        standard["results"][0]["handle"]
    );
    let snippet = result["results"][0]["snippet"].as_str().unwrap();
    assert!(snippet.starts_with("pub fn leaf() { let value = \"é"));
    assert!(snippet.ends_with("[truncated by MCP budget]"));
    assert!(
        result.to_string().len() <= 3200,
        "{} bytes",
        result.to_string().len()
    );
    assert!(result["cost"].get("budget_exceeded").is_none(), "{result}");
    assert_eq!(result["completeness"]["kind"], "truncated");
}

#[test]
fn irreducible_describe_budget_preserves_symbol_coordinate_tuple() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{\n    let value = 7;\n}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(100)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    let mut server = Server::start();
    let result = server.call(
        root.path(),
        "describe",
        json!({
            "target":"sym:src/lib.rs#leaf", "detail":"standard", "budget_tokens":256
        }),
    );
    assert_eq!(result["card"]["handle"], "sym:src/lib.rs#leaf");
    assert_eq!(
        result["card"]["sections"]["identity"]["data"]["lines"],
        json!([1, 3])
    );
    assert_eq!(result["cost"]["budget_exceeded"], true);
}

#[test]
fn first_legacy_budget_cut_discloses_counts_and_retry_without_an_envelope() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let mut server = Server::start_mode(true);
    let result = server.call(
        root.path(),
        "find_references",
        json!({"name":"leaf","budget_tokens":256}),
    );
    assert!(result.get("completeness").is_none(), "{result}");
    assert!(result.get("tool").is_none(), "{result}");
    assert!(result.get("index").is_none(), "{result}");
    let returned = result["results"].as_array().unwrap().len();
    assert!(returned > 0 && returned < 100);
    assert_eq!(result["_meta"]["truncated"], true);
    assert_eq!(result["_meta"]["total_results"], 100);
    assert_eq!(result["_meta"]["returned"], returned);
    assert_eq!(result["_meta"]["trimmed"][0]["omitted"], 100 - returned);
    let retry = &result["_meta"]["recover"];
    let recovered = server.call(
        root.path(),
        retry["tool"].as_str().unwrap(),
        retry["arguments"].clone(),
    );
    assert_eq!(recovered["results"].as_array().unwrap().len(), 100);
    assert!(recovered.get("completeness").is_none());
}

#[test]
fn request_diagnostics_include_final_rendering_in_default_and_legacy_modes() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    std::fs::write(
        root.path().join("src/lib.rs"),
        format!(
            "pub fn leaf() {{}}\npub fn caller() {{\n{}}}\n",
            "    leaf();\n".repeat(200)
        ),
    )
    .unwrap();
    command(
        root.path(),
        env!("CARGO_BIN_EXE_codesage"),
        &["index", "--no-semantic"],
    );
    for legacy in [false, true] {
        let mut server = Server::start_mode(legacy);
        let response = server.call(
            root.path(),
            "find_references",
            json!({"name":"leaf","budget_tokens":256}),
        );
        let cost_ms = response["cost"]["ms"].as_u64().unwrap();
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":20}));
        let request = stats["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == "find_references")
            .unwrap();
        let wall_ms = request["wall_ms"].as_u64().unwrap();
        assert!(
            wall_ms + 1 >= cost_ms,
            "legacy={legacy}: request {wall_ms}ms excludes response work measured at {cost_ms}ms"
        );
        assert_eq!(request["outcome"], "success");
        let help = server.call(root.path(), "help", json!({"tool":"find_references"}));
        assert_eq!(
            help["price_list"]["tools"]["find_references"]["request_wall_ms"],
            stats["tools"]["find_references"]["request_wall_ms"]
        );
        let error = server.raw_call(
            root.path(),
            "find_references",
            json!({"name":"leaf","detail":"invalid"}),
        );
        assert_eq!(error["isError"], true);
        let contract = error["content"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|item| {
                let value = serde_json::from_str::<Value>(item["text"].as_str()?).ok()?;
                value.get("error").is_some().then_some(value)
            })
            .unwrap();
        let stats = server.call(root.path(), "daemon_stats", json!({"recent":20}));
        let request = stats["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == "find_references")
            .unwrap();
        assert_eq!(request["outcome"], "error");
        assert!(
            request["wall_ms"].as_u64().unwrap() + 1 >= contract["cost"]["ms"].as_u64().unwrap()
        );
    }
}
