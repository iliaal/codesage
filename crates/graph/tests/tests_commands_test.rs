//! `recommend_tests` emits runnable commands per framework and surfaces the
//! test modules living inside the changed files. Each fixture is a real tree
//! indexed structurally, so runner detection reads the same manifests an
//! agent would.

use std::path::Path;
use std::time::Duration;

use codesage_graph::{
    ReachabilityOptions, build_review_rehearsal, full_index, recommend_tests_with_reachability,
};
use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Language,
    TestCommand, TestRecommendations,
};
use codesage_storage::Database;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

fn indexed(root: &Path) -> Database {
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    db
}

fn recs(root: &Path, db: &Database, inputs: &[&str]) -> TestRecommendations {
    let opts = ReachabilityOptions {
        project_root: Some(root.to_path_buf()),
        deadline: Duration::from_secs(60),
        ..ReachabilityOptions::default()
    };
    let inputs: Vec<String> = inputs.iter().map(|s| s.to_string()).collect();
    recommend_tests_with_reachability(db, &inputs, &opts).unwrap()
}

fn commands(r: &TestRecommendations) -> Vec<&str> {
    r.commands.iter().map(|c| c.command.as_str()).collect()
}

fn find<'a>(r: &'a TestRecommendations, command: &str) -> &'a TestCommand {
    r.commands
        .iter()
        .find(|c| c.command == command)
        .unwrap_or_else(|| panic!("missing command `{command}` in {:?}", commands(r)))
}

fn rust_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"acme-core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        root,
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   use super::*;\n\
         \x20   fn helper() -> u32 { 1 }\n\
         \x20   #[test]\n\
         \x20   fn adds() { assert_eq!(add(1, 1), 2); }\n\
         \x20   #[test]\n\
         \x20   fn adds_zero() { assert_eq!(add(helper(), 0), 1); }\n\
         }\n",
    );
    write(
        root,
        "tests/integration.rs",
        "#[test]\nfn it_works() { assert_eq!(acme_core::add(2, 2), 4); }\n",
    );
    write(root, "tests/other.rs", "#[test]\nfn other() {}\n");
    write(
        root,
        "tests/suite/main.rs",
        "#[test]\nfn suite_runs() { acme_core::add(0, 0); }\n",
    );
    dir
}

#[test]
fn rust_fixture_yields_per_target_cargo_commands_and_the_inline_module() {
    let dir = rust_fixture();
    let root = dir.path();
    let db = indexed(root);

    let r = recs(root, &db, &["src/lib.rs"]);

    let cmds = commands(&r);
    assert!(
        cmds.contains(&"cargo test -p acme-core --test integration"),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&"cargo test -p acme-core --test other"),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&"cargo test -p acme-core --test suite"),
        "{cmds:?}"
    );
    let integration = find(&r, "cargo test -p acme-core --test integration");
    assert_eq!(integration.covers, vec!["tests/integration.rs".to_string()]);
    assert_eq!(integration.framework, "cargo");
    assert_eq!(integration.source, "convention");

    let inline = find(&r, "cargo test -p acme-core tests::");
    assert_eq!(inline.source, "inline");
    assert_eq!(inline.covers, vec!["tests".to_string()]);
    assert_eq!(
        r.inline_test_modules.len(),
        1,
        "{:?}",
        r.inline_test_modules
    );
    let module = &r.inline_test_modules[0];
    assert_eq!(module.file, "src/lib.rs");
    assert_eq!(module.module, "tests");
    // `helper` is a plain fn inside the module and must not count.
    assert_eq!(module.test_count, 2);
    assert!(
        r.notes
            .iter()
            .any(|n| n.starts_with("1 inline test module(s) in the changed files (2 test(s))")),
        "{:?}",
        r.notes
    );

    // The changed file's own tests lead; the convention sweep follows,
    // sorted by text.
    assert_eq!(
        cmds,
        vec![
            "cargo test -p acme-core tests::",
            "cargo test -p acme-core --test integration",
            "cargo test -p acme-core --test other",
            "cargo test -p acme-core --test suite",
        ]
    );
}

#[test]
fn more_than_three_targets_collapse_into_the_crate_suite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[package]\nname = \"wide\"\n");
    write(root, "src/lib.rs", "pub fn f() {}\n");
    for i in 0..4 {
        write(root, &format!("tests/t{i}.rs"), "#[test]\nfn t() {}\n");
    }
    let db = indexed(root);

    let r = recs(root, &db, &["src/lib.rs"]);

    assert_eq!(commands(&r), vec!["cargo test -p wide"]);
    assert_eq!(
        find(&r, "cargo test -p wide").covers,
        vec!["tests/t0.rs", "tests/t1.rs", "tests/t2.rs", "tests/t3.rs"]
    );
}

#[test]
fn hostile_file_names_are_single_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a b", "x;id", "y$(whoami)", "it's"] {
        write(root, &format!("pkg/{name}.py"), "def f():\n    return 1\n");
        write(
            root,
            &format!("tests/test_{name}.py"),
            "def test_f():\n    assert True\n",
        );
    }
    let db = indexed(root);

    let r = recs(
        root,
        &db,
        &[
            "pkg/a b.py",
            "pkg/x;id.py",
            "pkg/y$(whoami).py",
            "pkg/it's.py",
        ],
    );

    assert_eq!(
        commands(&r),
        vec![
            "pytest 'tests/test_a b.py' 'tests/test_it'\\''s.py' 'tests/test_x;id.py' \
             'tests/test_y$(whoami).py'"
        ]
    );
    assert_eq!(
        find(&r, commands(&r)[0]).covers,
        vec![
            "tests/test_a b.py",
            "tests/test_it's.py",
            "tests/test_x;id.py",
            "tests/test_y$(whoami).py"
        ]
    );
}

#[test]
fn leading_dash_paths_are_anchored_with_dot_slash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "-x.py", "def f():\n    return 1\n");
    write(root, "-x_test.py", "def test_f():\n    assert True\n");
    let db = indexed(root);

    let r = recs(root, &db, &["-x.py"]);

    assert_eq!(commands(&r), vec!["pytest ./-x_test.py"]);
    assert_eq!(find(&r, "pytest ./-x_test.py").covers, vec!["-x_test.py"]);
}

#[test]
fn rust_paths_without_a_readable_manifest_omit_the_package_flag() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A workspace-only manifest: no `[package]` anywhere on the ancestor chain.
    write(root, "Cargo.toml", "[workspace]\nmembers = []\n");
    write(
        root,
        "crates/graph/src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    );
    write(root, "crates/graph/tests/it.rs", "#[test]\nfn t() {}\n");
    let db = indexed(root);

    let r = recs(root, &db, &["crates/graph/src/lib.rs"]);

    assert_eq!(
        commands(&r),
        vec!["cargo test tests::", "cargo test --test it"]
    );
}

#[test]
fn nested_crate_reads_its_own_manifest_and_module_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        root,
        "crates/graph/Cargo.toml",
        "[package]\nname = \"codesage-graph\"\nversion.workspace = true\n",
    );
    write(
        root,
        "crates/graph/src/lib.rs",
        "pub mod search;\npub mod util;\n",
    );
    write(
        root,
        "crates/graph/src/search.rs",
        "pub fn run() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn runs() { super::run(); }\n}\n",
    );
    // A module named by convention with no cfg gate is still a test module.
    write(
        root,
        "crates/graph/src/util.rs",
        "pub fn u() {}\nmod util_tests {\n    #[test]\n    fn a() {}\n    #[test]\n    fn b() {}\n    #[test]\n    fn c() {}\n}\n",
    );
    write(
        root,
        "crates/graph/tests/search_test.rs",
        "#[test]\nfn t() {}\n",
    );
    let db = indexed(root);

    let r = recs(
        root,
        &db,
        &["crates/graph/src/search.rs", "crates/graph/src/util.rs"],
    );

    let cmds = commands(&r);
    assert!(
        cmds.contains(&"cargo test -p codesage-graph --test search_test"),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&"cargo test -p codesage-graph search::tests::"),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&"cargo test -p codesage-graph util::util_tests::"),
        "{cmds:?}"
    );
    let modules: Vec<(&str, &str, usize)> = r
        .inline_test_modules
        .iter()
        .map(|m| (m.file.as_str(), m.module.as_str(), m.test_count))
        .collect();
    assert_eq!(
        modules,
        vec![
            ("crates/graph/src/search.rs", "search::tests", 1),
            ("crates/graph/src/util.rs", "util::util_tests", 3),
        ]
    );
}

#[test]
fn nested_test_modules_count_toward_the_outermost_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[package]\nname = \"nested\"\n");
    write(
        root,
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn outer() {}\n    mod inner_tests {\n        #[test]\n        fn inner() {}\n    }\n}\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["src/lib.rs"]);

    assert_eq!(
        r.inline_test_modules.len(),
        1,
        "{:?}",
        r.inline_test_modules
    );
    assert_eq!(r.inline_test_modules[0].module, "tests");
    assert_eq!(r.inline_test_modules[0].test_count, 2);
    assert_eq!(commands(&r), vec!["cargo test -p nested tests::"]);
}

#[test]
fn rust_file_without_test_module_emits_no_inline_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[package]\nname = \"plain\"\n");
    write(root, "src/lib.rs", "pub mod io;\npub fn f() {}\n");
    write(root, "src/io.rs", "pub fn g() {}\n");
    let db = indexed(root);

    let r = recs(root, &db, &["src/lib.rs"]);

    assert!(
        r.inline_test_modules.is_empty(),
        "{:?}",
        r.inline_test_modules
    );
    assert!(r.commands.is_empty(), "{:?}", r.commands);
    let json = serde_json::to_value(&r).unwrap();
    assert!(
        json.get("commands").is_none(),
        "empty commands must be omitted: {json}"
    );
    assert!(json.get("inline_test_modules").is_none(), "{json}");
}

#[test]
fn python_fixture_yields_one_pytest_command_and_inline_tests_in_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pkg/mod.py",
        "def add(a, b):\n    return a + b\n\n\ndef test_add_inline():\n    assert add(1, 1) == 2\n\n\nclass TestAdd:\n    def test_method(self):\n        assert True\n\n    def helper(self):\n        pass\n",
    );
    write(root, "pkg/other.py", "def sub(a, b):\n    return a - b\n");
    write(
        root,
        "tests/test_mod.py",
        "from pkg.mod import add\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    );
    write(
        root,
        "tests/test_other.py",
        "from pkg.other import sub\n\ndef test_sub():\n    assert sub(2, 1) == 1\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["pkg/other.py", "pkg/mod.py"]);

    let convention = find(&r, "pytest tests/test_mod.py tests/test_other.py");
    assert_eq!(convention.framework, "pytest");
    assert_eq!(convention.source, "convention");
    assert_eq!(
        convention.covers,
        vec![
            "tests/test_mod.py".to_string(),
            "tests/test_other.py".to_string()
        ]
    );
    let inline = find(&r, "pytest pkg/mod.py");
    assert_eq!(inline.source, "inline");
    assert_eq!(
        r.inline_test_modules.len(),
        1,
        "{:?}",
        r.inline_test_modules
    );
    assert_eq!(r.inline_test_modules[0].module, "pkg.mod");
    assert_eq!(r.inline_test_modules[0].file, "pkg/mod.py");
    assert_eq!(r.inline_test_modules[0].test_count, 2);
}

#[test]
fn laravel_marker_switches_php_runner_to_artisan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "artisan", "#!/usr/bin/env php\n<?php\n");
    write(
        root,
        "app/Services/Billing.php",
        "<?php\nnamespace App\\Services;\nclass Billing { public function charge() {} }\n",
    );
    write(
        root,
        "tests/Unit/Services/BillingTest.php",
        "<?php\nnamespace Tests\\Unit\\Services;\nclass BillingTest { public function testCharge() {} }\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["app/Services/Billing.php"]);

    let cmd = find(&r, "php artisan test tests/Unit/Services/BillingTest.php");
    assert_eq!(cmd.framework, "artisan");
    assert_eq!(
        cmd.covers,
        vec!["tests/Unit/Services/BillingTest.php".to_string()]
    );
}

#[test]
fn plain_php_project_uses_phpunit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/Service/Foo.php",
        "<?php\nnamespace App\\Service;\nclass Foo { public function bar() {} }\n",
    );
    write(
        root,
        "src/Service/FooTest.php",
        "<?php\nnamespace App\\Service;\nclass FooTest { public function testBar() {} }\n",
    );
    write(
        root,
        "tests/Service/FooTest.php",
        "<?php\nnamespace Tests\\Service;\nclass FooTest { public function testBar() {} }\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["src/Service/Foo.php"]);

    let cmd = find(
        &r,
        "vendor/bin/phpunit src/Service/FooTest.php tests/Service/FooTest.php",
    );
    assert_eq!(cmd.framework, "phpunit");
    assert_eq!(cmd.source, "convention");
}

#[test]
fn phpt_suites_map_to_run_tests_per_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ext/foo/foo.c", "int foo(void) { return 1; }\n");
    write(
        root,
        "ext/foo/tests/001.phpt",
        "--TEST--\n001\n--FILE--\n<?php ?>\n--EXPECT--\n",
    );
    write(
        root,
        "ext/foo/tests/002.phpt",
        "--TEST--\n002\n--FILE--\n<?php ?>\n--EXPECT--\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["ext/foo/foo.c"]);

    let cmd = find(&r, "php run-tests.php ext/foo/tests");
    assert_eq!(cmd.framework, "run-tests");
    assert_eq!(
        cmd.covers,
        vec![
            "ext/foo/tests/001.phpt".to_string(),
            "ext/foo/tests/002.phpt".to_string()
        ]
    );
}

#[test]
fn withheld_phpt_directory_is_named_as_a_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ext/big/big.c", "int big(void) { return 1; }\n");
    for i in 0..51 {
        write(
            root,
            &format!("ext/big/tests/{i:03}.phpt"),
            "--TEST--\nx\n--FILE--\n<?php ?>\n--EXPECT--\n",
        );
    }
    let db = indexed(root);

    let r = recs(root, &db, &["ext/big/big.c"]);

    assert!(
        r.primary.is_empty(),
        "withheld above the cap: {:?}",
        r.primary
    );
    let cmd = find(&r, "php run-tests.php ext/big/tests");
    assert_eq!(cmd.covers, vec!["ext/big/tests".to_string()]);
}

#[test]
fn go_fixture_yields_go_test_over_package_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pkg/util/util.go",
        "package util\n\nfunc Add(a, b int) int { return a + b }\n",
    );
    write(
        root,
        "pkg/util/util_test.go",
        "package util\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) { Add(1, 2) }\n",
    );
    write(root, "main.go", "package main\n\nfunc main() {}\n");
    write(
        root,
        "main_test.go",
        "package main\n\nimport \"testing\"\n\nfunc TestMain(t *testing.T) {}\n",
    );
    let db = indexed(root);

    let r = recs(root, &db, &["pkg/util/util.go", "main.go"]);

    let cmd = find(&r, "go test . ./pkg/util");
    assert_eq!(cmd.framework, "go");
    assert_eq!(
        cmd.covers,
        vec![
            "main_test.go".to_string(),
            "pkg/util/util_test.go".to_string()
        ]
    );
}

#[test]
fn js_runner_follows_vitest_config_presence() {
    let make = |with_vitest: bool| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if with_vitest {
            write(root, "vitest.config.ts", "export default {};\n");
        }
        write(
            root,
            "src/button.ts",
            "export function button() { return 1; }\n",
        );
        write(
            root,
            "src/button.test.ts",
            "import { button } from './button';\ntest('button', () => { button(); });\n",
        );
        dir
    };

    let dir = make(true);
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/button.ts"]);
    let cmd = find(&r, "npx vitest run src/button.test.ts");
    assert_eq!(cmd.framework, "vitest");

    // Without a config, a manifest, or a runner import nothing names a
    // runner, so no command is guessed.
    let dir = make(false);
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/button.ts"]);
    assert_no_js_command(&r, "src/button.test.ts");
}

fn assert_no_js_command(r: &TestRecommendations, path: &str) {
    assert!(
        r.commands
            .iter()
            .all(|c| !c.covers.iter().any(|p| p == path)),
        "{:?}",
        commands(r)
    );
    assert!(
        r.notes.iter().any(
            |n| n.starts_with("no JavaScript/TypeScript test runner named for") && n.contains(path)
        ),
        "{:?}",
        r.notes
    );
}

/// A root package with `src/a.<ext>` and its sibling test.
fn js_fixture(package_json: Option<&str>, ext: &str, test_body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    if let Some(manifest) = package_json {
        write(root, "package.json", manifest);
    }
    write(
        root,
        &format!("src/a.{ext}"),
        "export function a() { return 1; }\n",
    );
    write(root, &format!("src/a.test.{ext}"), test_body);
    dir
}

const JS_TEST_BODY: &str = "import { a } from './a';\ntest('a', () => { a(); });\n";

#[test]
fn js_runner_from_vitest_dependency() {
    let dir = js_fixture(
        Some(r#"{"name":"app","devDependencies":{"vitest":"^2.0.0","typescript":"^5"}}"#),
        "ts",
        JS_TEST_BODY,
    );
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/a.ts"]);
    let cmd = find(&r, "npx vitest run src/a.test.ts");
    assert_eq!(cmd.framework, "vitest");
    assert_eq!(cmd.covers, vec!["src/a.test.ts".to_string()]);
}

#[test]
fn js_runner_from_jest_test_script() {
    let dir = js_fixture(
        Some(r#"{"name":"app","scripts":{"test":"jest --coverage"}}"#),
        "js",
        JS_TEST_BODY,
    );
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/a.js"]);
    assert_eq!(find(&r, "npx jest src/a.test.js").framework, "jest");
}

#[test]
fn js_runner_from_mocha_test_script() {
    let dir = js_fixture(
        Some(r#"{"name":"app","scripts":{"test":"c8 mocha"},"devDependencies":{"jest":"^29"}}"#),
        "js",
        "const { a } = require('./a');\ndescribe('a', () => { it('works', () => { a(); }); });\n",
    );
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/a.js"]);
    assert_eq!(find(&r, "npx mocha src/a.test.js").framework, "mocha");
    assert!(
        !commands(&r).iter().any(|c| c.contains("jest")),
        "{:?}",
        commands(&r)
    );
}

#[test]
fn node_test_import_outranks_the_package_runner() {
    let dir = js_fixture(
        Some(r#"{"name":"app","scripts":{"test":"mocha"}}"#),
        "js",
        "import { test } from 'node:test';\nimport { a } from './a.js';\ntest('a', () => { a(); });\n",
    );
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/a.js"]);
    assert_eq!(find(&r, "node --test src/a.test.js").framework, "node:test");
    assert!(
        !commands(&r).iter().any(|c| c.contains("mocha")),
        "{:?}",
        commands(&r)
    );
}

#[test]
fn nested_packages_resolve_their_own_runner_or_defer_to_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let js = "export function f() { return 1; }\n";
    let test =
        |name: &str| format!("import {{ f }} from './{name}';\ntest('f', () => {{ f(); }});\n");
    write(
        root,
        "package.json",
        r#"{"name":"mono","private":true,"workspaces":["packages/*"],"devDependencies":{"jest":"^29"}}"#,
    );
    write(root, "src/a.ts", js);
    write(root, "src/a.test.ts", &test("a"));
    write(
        root,
        "packages/web/package.json",
        r#"{"name":"web","scripts":{"test":"vitest run"}}"#,
    );
    write(root, "packages/web/src/b.ts", js);
    write(root, "packages/web/src/b.test.ts", &test("b"));
    write(root, "packages/lib/package.json", r#"{"name":"lib"}"#);
    write(root, "packages/lib/src/c.ts", js);
    write(root, "packages/lib/src/c.test.ts", &test("c"));
    let db = indexed(root);

    let r = recs(
        root,
        &db,
        &["src/a.ts", "packages/web/src/b.ts", "packages/lib/src/c.ts"],
    );

    let web = find(&r, "(cd packages/web && npx vitest run src/b.test.ts)");
    assert_eq!(web.framework, "vitest");
    assert_eq!(web.covers, vec!["packages/web/src/b.test.ts".to_string()]);
    let rooted = find(&r, "npx jest packages/lib/src/c.test.ts src/a.test.ts");
    assert_eq!(rooted.framework, "jest");
    assert_eq!(
        rooted.covers,
        vec![
            "packages/lib/src/c.test.ts".to_string(),
            "src/a.test.ts".to_string()
        ]
    );
}

#[test]
fn js_without_runner_evidence_gets_a_note_and_no_command() {
    let dir = js_fixture(
        Some(r#"{"name":"app","scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#),
        "js",
        JS_TEST_BODY,
    );
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &["src/a.js"]);
    assert_no_js_command(&r, "src/a.test.js");
    assert!(
        !commands(&r).iter().any(|c| c.starts_with("npx")),
        "{:?}",
        commands(&r)
    );
}

#[test]
fn java_runner_follows_build_manifest() {
    let make = |manifest: &str| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, manifest, "");
        write(
            root,
            "src/main/java/com/acme/Foo.java",
            "package com.acme;\npublic class Foo { public int bar() { return 1; } }\n",
        );
        write(
            root,
            "src/test/java/com/acme/FooTest.java",
            "package com.acme;\npublic class FooTest { public void testBar() {} }\n",
        );
        write(
            root,
            "src/main/java/com/acme/Baz.java",
            "package com.acme;\npublic class Baz { public int q() { return 1; } }\n",
        );
        write(
            root,
            "src/test/java/com/acme/BazTest.java",
            "package com.acme;\npublic class BazTest { public void testQ() {} }\n",
        );
        dir
    };
    let inputs = [
        "src/main/java/com/acme/Foo.java",
        "src/main/java/com/acme/Baz.java",
    ];

    let dir = make("pom.xml");
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &inputs);
    let cmd = find(&r, "mvn -Dtest=BazTest,FooTest test");
    assert_eq!(cmd.framework, "maven");

    let dir = make("build.gradle.kts");
    let db = indexed(dir.path());
    let r = recs(dir.path(), &db, &inputs);
    let cmd = find(&r, "gradle test --tests BazTest --tests FooTest");
    assert_eq!(cmd.framework, "gradle");
}

#[test]
fn feature_test_command_is_merged_with_its_own_source() {
    let dir = rust_fixture();
    let root = dir.path();
    let db = indexed(root);
    db.upsert_feature(&FeatureRecord {
        feature_id: "feat_0123456789abcdef".to_string(),
        title: "acme-core".to_string(),
        summary: String::new(),
        kind: FeatureKind::Library,
        source: "cargo-lib".to_string(),
        confidence: FeatureConfidence::High,
        entry_path: "src/lib.rs".to_string(),
        entry_symbol: None,
        entry_route: None,
        entry_command: None,
        test_command: Some("cargo test --package acme-core".to_string()),
        language: Language::Rust,
        tags: Vec::new(),
        trust_boundaries: Vec::new(),
        files: vec![FeatureFileRef {
            path: "src/lib.rs".to_string(),
            role: FeatureFileRole::Entry,
            reason: None,
        }],
    })
    .unwrap();

    // The caller's spelling is what `covers` reports for feature commands.
    let r = recs(root, &db, &["./src/lib.rs"]);

    let cmd = find(&r, "cargo test --package acme-core");
    assert_eq!(cmd.source, "feature_test_command");
    assert_eq!(cmd.framework, "cargo");
    assert_eq!(cmd.covers, vec!["./src/lib.rs".to_string()]);
    // Inline first, then the feature's runner, then the convention sweep.
    assert_eq!(
        commands(&r),
        vec![
            "cargo test -p acme-core tests::",
            "cargo test --package acme-core",
            "cargo test -p acme-core --test integration",
            "cargo test -p acme-core --test other",
            "cargo test -p acme-core --test suite",
        ]
    );
}

fn feature_with_command(entry_path: &str, test_command: &str) -> FeatureRecord {
    FeatureRecord {
        feature_id: format!("feat_{:016x}", test_command.len()),
        title: entry_path.to_string(),
        summary: String::new(),
        kind: FeatureKind::Library,
        source: "cargo-lib".to_string(),
        confidence: FeatureConfidence::High,
        entry_path: entry_path.to_string(),
        entry_symbol: None,
        entry_route: None,
        entry_command: None,
        test_command: Some(test_command.to_string()),
        language: Language::Rust,
        tags: Vec::new(),
        trust_boundaries: Vec::new(),
        files: vec![FeatureFileRef {
            path: entry_path.to_string(),
            role: FeatureFileRole::Entry,
            reason: None,
        }],
    }
}

#[test]
fn identical_command_from_two_sources_keeps_the_feature_source() {
    let dir = rust_fixture();
    let root = dir.path();
    let db = indexed(root);
    db.upsert_feature(&feature_with_command(
        "src/lib.rs",
        "cargo test -p acme-core --test integration",
    ))
    .unwrap();

    let r = recs(root, &db, &["src/lib.rs"]);

    let cmd = find(&r, "cargo test -p acme-core --test integration");
    assert_eq!(cmd.source, "feature_test_command");
    assert_eq!(
        cmd.covers,
        vec!["src/lib.rs".to_string(), "tests/integration.rs".to_string()]
    );
    assert_eq!(
        commands(&r)
            .iter()
            .filter(|c| **c == "cargo test -p acme-core --test integration")
            .count(),
        1
    );
}

#[test]
fn multiline_feature_command_is_dropped_with_a_note() {
    let dir = rust_fixture();
    let root = dir.path();
    let db = indexed(root);
    let feature = feature_with_command("src/lib.rs", "cargo test\nrm -rf /");
    db.upsert_feature(&feature).unwrap();

    let r = recs(root, &db, &["src/lib.rs"]);

    assert!(
        r.commands
            .iter()
            .all(|c| c.source != "feature_test_command"),
        "{:?}",
        r.commands
    );
    assert!(
        r.notes.iter().any(|n| n.starts_with(&format!(
            "feature test_command dropped from `commands` for {}",
            feature.feature_id
        ))),
        "{:?}",
        r.notes
    );
}

#[test]
fn rehearsal_summary_quotes_the_top_commands() {
    let dir = rust_fixture();
    let root = dir.path();
    let db = indexed(root);

    let rehearsal = build_review_rehearsal(root, &db, &["src/lib.rs".to_string()]).unwrap();

    let note = rehearsal
        .summary_notes
        .iter()
        .find(|n| n.starts_with("Test commands: "))
        .unwrap_or_else(|| panic!("no test-commands note in {:?}", rehearsal.summary_notes));
    assert_eq!(
        note,
        "Test commands: cargo test -p acme-core tests::; \
         cargo test -p acme-core --test integration; cargo test -p acme-core --test other; \
         cargo test -p acme-core --test suite"
    );
    assert!(
        rehearsal
            .summary_notes
            .iter()
            .any(|n| n.starts_with("Run tests: ")),
        "the file-list note remains: {:?}",
        rehearsal.summary_notes
    );
}

#[test]
fn rehearsal_note_caps_at_five_commands() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    let mut files = Vec::new();
    for i in 0..7 {
        write(
            root,
            &format!("crates/c{i}/Cargo.toml"),
            &format!("[package]\nname = \"c{i}\"\n"),
        );
        write(root, &format!("crates/c{i}/src/lib.rs"), "pub fn f() {}\n");
        write(
            root,
            &format!("crates/c{i}/tests/it.rs"),
            "#[test]\nfn t() {}\n",
        );
        files.push(format!("crates/c{i}/src/lib.rs"));
    }
    let db = indexed(root);

    let rehearsal = build_review_rehearsal(root, &db, &files).unwrap();

    let note = rehearsal
        .summary_notes
        .iter()
        .find(|n| n.starts_with("Test commands: "))
        .unwrap_or_else(|| panic!("no test-commands note in {:?}", rehearsal.summary_notes));
    assert_eq!(
        note,
        "Test commands: cargo test -p c0 --test it; cargo test -p c1 --test it; \
         cargo test -p c2 --test it; cargo test -p c3 --test it; \
         cargo test -p c4 --test it (+2 more)"
    );
}
