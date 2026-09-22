//! Symbol-level `is_test` marks per language, plus the file-level path
//! heuristic that the indexer applies on top of them.

use codesage_parser::discover::is_test_like_path;
use codesage_parser::extract::{extract_symbols, file_is_test_by_syntax};
use codesage_parser::parse::parse_file;
use codesage_protocol::{Language, Symbol};

fn symbols(source: &str, language: Language, path: &str) -> Vec<Symbol> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, path).unwrap()
}

fn is_test(symbols: &[Symbol], name: &str) -> bool {
    let matches: Vec<&Symbol> = symbols.iter().filter(|s| s.name == name).collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one symbol named {name}: {matches:?}"
    );
    matches[0].is_test
}

const RUST_SOURCE: &str = r#"
pub fn product() {}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture;

    fn helper() {}

    mod nested {
        pub fn deep_helper() {}
    }

    #[test]
    fn plain_test() { helper(); }
}

#[cfg(all(test, feature = "slow"))]
mod slow_tests {
    fn slow_helper() {}
}

#[cfg(all(feature = "slow", test))]
mod trailing_test_gate {
    fn trailing_helper() {}
}

#[cfg(all(unix, all(test, feature = "slow")))]
mod nested_all_gate {
    fn nested_all_helper() {}
}

#[cfg(any(test, feature = "slow"))]
mod any_gated {
    fn any_helper() {}
}

#[cfg(not(test))]
mod not_test_gated {
    fn not_test_helper() {}
}

#[cfg(feature = "slow")]
mod feature_gated {
    fn gated_product() {}
}

#[tokio::test(flavor = "multi_thread")]
async fn tokio_case() {}

#[rstest]
fn rstest_case() {}

#[sqlx::test]
async fn sqlx_case() {}

#[async_std::test]
async fn async_std_case() {}

#[test_case(1, 2)]
fn cased(a: u8, b: u8) {}

// A doc line between attribute and item does not break the attribute run.
#[test]
/// documented
fn documented_test() {
    fn inner_helper() {}
}

#[tests]
fn misspelled_attribute() {}

#[cfg_attr(test, derive(Debug))]
struct NotTestGated;
"#;

#[test]
fn rust_cfg_test_modules_and_test_attributes_mark_symbols() {
    let syms = symbols(RUST_SOURCE, Language::Rust, "src/lib.rs");
    assert!(!is_test(&syms, "product"));
    assert!(is_test(&syms, "tests"));
    assert!(is_test(&syms, "Fixture"));
    assert!(is_test(&syms, "helper"));
    assert!(is_test(&syms, "nested"));
    assert!(is_test(&syms, "deep_helper"), "any depth inside cfg(test)");
    assert!(is_test(&syms, "plain_test"));
    assert!(is_test(&syms, "slow_tests"));
    assert!(
        is_test(&syms, "slow_helper"),
        "cfg(all(test, ...)) gates on test"
    );
    assert!(is_test(&syms, "trailing_test_gate"));
    assert!(
        is_test(&syms, "trailing_helper"),
        "test may sit anywhere in the cfg(all(...)) list"
    );
    assert!(
        is_test(&syms, "nested_all_helper"),
        "a nested cfg(all(...)) still gates on test"
    );
    assert!(
        !is_test(&syms, "any_gated"),
        "cfg(any(test, ...)) also holds outside a test build"
    );
    assert!(!is_test(&syms, "any_helper"));
    assert!(!is_test(&syms, "not_test_gated"));
    assert!(!is_test(&syms, "not_test_helper"));
    assert!(!is_test(&syms, "feature_gated"));
    assert!(!is_test(&syms, "gated_product"));
    assert!(is_test(&syms, "tokio_case"));
    assert!(is_test(&syms, "rstest_case"));
    assert!(is_test(&syms, "sqlx_case"));
    assert!(is_test(&syms, "async_std_case"));
    assert!(is_test(&syms, "cased"));
    assert!(is_test(&syms, "documented_test"));
    assert!(is_test(&syms, "inner_helper"), "nested inside a #[test] fn");
    assert!(!is_test(&syms, "misspelled_attribute"));
    assert!(!is_test(&syms, "NotTestGated"));
}

const PYTHON_SOURCE: &str = r#"
import pytest

def product():
    pass

def test_product():
    def inner_helper():
        pass
    inner_helper()

class TestProduct:
    def setup_method(self):
        pass

    def test_it(self):
        pass

class Testament:
    def read(self):
        pass

@pytest.fixture
def db():
    return None

@pytest.fixture(scope="module")
def session_db():
    return None

@pytest.mark.parametrize("x", [1, 2])
def check_values(x):
    pass

@pytest.mark.slow
class SlowSuite:
    def run(self):
        pass

@dataclass
class Plain:
    pass
"#;

#[test]
fn python_test_names_and_pytest_decorators_mark_symbols() {
    let syms = symbols(PYTHON_SOURCE, Language::Python, "app/models.py");
    assert!(!is_test(&syms, "product"));
    assert!(is_test(&syms, "test_product"));
    assert!(is_test(&syms, "inner_helper"), "nested in a test_ function");
    assert!(is_test(&syms, "TestProduct"));
    assert!(is_test(&syms, "setup_method"), "method of a Test* class");
    assert!(is_test(&syms, "test_it"));
    assert!(
        !is_test(&syms, "Testament"),
        "Test followed by lowercase is a word"
    );
    assert!(!is_test(&syms, "read"));
    assert!(is_test(&syms, "db"));
    assert!(is_test(&syms, "session_db"));
    assert!(is_test(&syms, "check_values"));
    assert!(is_test(&syms, "SlowSuite"));
    assert!(is_test(&syms, "run"), "method of a pytest-marked class");
    assert!(!is_test(&syms, "Plain"));
}

const JS_NESTED_SOURCE: &str = r#"
export function product() {}

function registerSuite() {
  describe("product", () => {
    function helper() {}
    it("works", () => {
      class Probe {}
    });
    test.each([1, 2])("each %i", () => {});
  });
}

describe.skip("skipped", () => {
  function skippedHelper() {}
});

function unrelated() {}
"#;

#[test]
fn javascript_symbols_inside_describe_it_test_callbacks_are_tests() {
    // The only top-level test call is `describe.skip(...)`, which is a member
    // call rooted at `describe`: the file counts as a test file, so every
    // symbol is marked. Check callback nesting on a file without that call.
    let syms = symbols(JS_NESTED_SOURCE, Language::JavaScript, "src/product.js");
    assert!(
        is_test(&syms, "product"),
        "top-level describe.skip marks the file"
    );

    let without_top_level = JS_NESTED_SOURCE.replace(
        "describe.skip(\"skipped\", () => {\n  function skippedHelper() {}\n});",
        "",
    );
    let syms = symbols(&without_top_level, Language::JavaScript, "src/product.js");
    assert!(!is_test(&syms, "product"));
    assert!(!is_test(&syms, "registerSuite"));
    assert!(is_test(&syms, "helper"), "inside a describe callback");
    assert!(is_test(&syms, "Probe"), "inside an it callback");
    assert!(!is_test(&syms, "unrelated"));
}

const TS_TOP_LEVEL_SOURCE: &str = r#"
import { product } from "./product";

const fixture = { a: 1 };

function makeInput(): number { return 1; }

describe("product", () => {
  it("works", () => {
    expect(product(makeInput())).toBe(1);
  });
});
"#;

#[test]
fn typescript_file_with_top_level_describe_marks_every_symbol() {
    let syms = symbols(
        TS_TOP_LEVEL_SOURCE,
        Language::TypeScript,
        "src/product.check.ts",
    );
    assert!(is_test(&syms, "fixture"));
    assert!(is_test(&syms, "makeInput"));

    let plain =
        "export const fixture = { a: 1 };\nexport function makeInput(): number { return 1; }\n";
    let syms = symbols(plain, Language::TypeScript, "src/product.ts");
    assert!(!is_test(&syms, "fixture"));
    assert!(!is_test(&syms, "makeInput"));
}

const JS_LOCAL_TEST_BINDING_SOURCE: &str = r#"
import { product } from "./product";

function test(name, fn) {
  return fn(name);
}

const it = (label) => label;

let describe = null;

export function probe() {
  return test("probe", product);
}

test("startup", () => probe());
it("label");
"#;

const TS_VITEST_IMPORT_SOURCE: &str = r#"
import { test, expect } from "vitest";
import { product } from "./product";

function makeInput(): number { return 1; }

test("product", () => {
  expect(product(makeInput())).toBe(1);
});
"#;

#[test]
fn a_locally_declared_test_callee_does_not_make_the_file_a_test() {
    let syms = symbols(
        JS_LOCAL_TEST_BINDING_SOURCE,
        Language::JavaScript,
        "src/runner.js",
    );
    assert!(!is_test(&syms, "test"), "the module's own function");
    assert!(!is_test(&syms, "it"));
    assert!(!is_test(&syms, "describe"));
    assert!(!is_test(&syms, "probe"));

    let exported_var = JS_LOCAL_TEST_BINDING_SOURCE
        .replace("function test(name, fn)", "export function test(name, fn)")
        .replace("let describe = null;", "export var describe = null;");
    let syms = symbols(&exported_var, Language::JavaScript, "src/runner.js");
    assert!(
        !is_test(&syms, "test"),
        "an exported local binding still counts"
    );
    assert!(!is_test(&syms, "probe"));

    let syms = symbols(
        TS_VITEST_IMPORT_SOURCE,
        Language::TypeScript,
        "src/product.check.ts",
    );
    assert!(
        is_test(&syms, "makeInput"),
        "an imported framework callee marks the file"
    );
}

const JAVA_SOURCE: &str = r#"
package com.acme;

import org.junit.jupiter.api.Test;

public class ProductTest {
    private int counter;

    @Test
    void works() {}

    @ParameterizedTest
    @ValueSource(ints = {1, 2})
    void worksWith(int value) {}

    @org.junit.Test
    public void legacyWorks() {}

    @Override
    public String toString() { return ""; }

    void helper() {}
}
"#;

#[test]
fn java_test_annotated_methods_are_tests_and_the_rest_are_not() {
    let syms = symbols(
        JAVA_SOURCE,
        Language::Java,
        "src/main/java/com/acme/ProductTest.java",
    );
    assert!(
        !is_test(&syms, "ProductTest"),
        "class level is the file heuristic's job"
    );
    assert!(!is_test(&syms, "counter"));
    assert!(is_test(&syms, "works"));
    assert!(is_test(&syms, "worksWith"));
    assert!(is_test(&syms, "legacyWorks"), "qualified annotation name");
    assert!(!is_test(&syms, "toString"));
    assert!(!is_test(&syms, "helper"));
}

#[test]
fn file_level_heuristic_covers_go_php_js_java_and_c_conventions() {
    for path in [
        "pkg/server/server_test.go",
        "tests/Feature/LoginTest.php",
        "app/Services/PaymentTest.php",
        "src/components/Button.test.tsx",
        "src/components/Button.spec.js",
        "src/__tests__/Button.js",
        "src/test/java/com/acme/ProductTest.java",
        "lib/parser_test.cc",
        "ext/standard/tests/strings/strlen.phpt",
        "crates/graph/tests/risk_test.rs",
        "src\\__tests__\\Button.js",
    ] {
        assert!(is_test_like_path(path), "{path} is a test file");
    }
    for path in [
        "pkg/server/server.go",
        "app/Services/Payment.php",
        "src/components/Button.tsx",
        "src/main/java/com/acme/Product.java",
        "lib/parser.cc",
        "crates/graph/src/search.rs",
        "src/testing_helpers.rs",
    ] {
        assert!(!is_test_like_path(path), "{path} is product code");
    }
}

#[test]
fn languages_without_syntax_rules_rely_on_the_file_heuristic_only() {
    let go = "package server\n\nfunc TestServer(t *testing.T) {}\n\nfunc helper() {}\n";
    let syms = symbols(go, Language::Go, "server_test.go");
    assert!(
        syms.iter().all(|s| !s.is_test),
        "parser leaves Go unmarked: {syms:?}"
    );

    let php = "<?php\nclass LoginTest extends TestCase {\n  public function testLogin() {}\n}\n";
    let syms = symbols(php, Language::Php, "tests/LoginTest.php");
    assert!(
        syms.iter().all(|s| !s.is_test),
        "parser leaves PHP unmarked: {syms:?}"
    );
}

const RUST_INNER_CFG_TEST_SOURCE: &str = r#"
#![cfg(test)]

pub fn helper_in_test_only_file() {}

struct TestOnlyFixture;

mod inner {
    pub fn deep() {}
}
"#;

const RUST_INNER_FEATURE_SOURCE: &str = r#"
#![cfg(feature = "slow")]
#![allow(dead_code)]

pub fn gated_product() {}
"#;

const RUST_INNER_NOT_TEST_SOURCE: &str = r#"
#![cfg(not(test))]

pub fn product_only() {}
"#;

#[test]
fn a_rust_file_headed_by_inner_cfg_test_marks_every_item() {
    let syms = symbols(RUST_INNER_CFG_TEST_SOURCE, Language::Rust, "src/helpers.rs");
    assert!(is_test(&syms, "helper_in_test_only_file"));
    assert!(is_test(&syms, "TestOnlyFixture"));
    assert!(is_test(&syms, "inner"));
    assert!(is_test(&syms, "deep"));

    let syms = symbols(RUST_INNER_FEATURE_SOURCE, Language::Rust, "src/slow.rs");
    assert!(
        !is_test(&syms, "gated_product"),
        "an inner cfg on a feature is not a test gate"
    );

    let syms = symbols(RUST_INNER_NOT_TEST_SOURCE, Language::Rust, "src/product.rs");
    assert!(
        !is_test(&syms, "product_only"),
        "#![cfg(not(test))] excludes the file from test builds"
    );
}

#[test]
fn file_level_verdict_matches_the_whole_file_marks() {
    let cases = [
        (RUST_INNER_CFG_TEST_SOURCE, Language::Rust, true),
        (RUST_INNER_FEATURE_SOURCE, Language::Rust, false),
        (RUST_INNER_NOT_TEST_SOURCE, Language::Rust, false),
        // `#[cfg(test)] mod tests { ... }` gates a module, not the file.
        (RUST_SOURCE, Language::Rust, false),
        (TS_TOP_LEVEL_SOURCE, Language::TypeScript, true),
        (TS_VITEST_IMPORT_SOURCE, Language::TypeScript, true),
        (JS_LOCAL_TEST_BINDING_SOURCE, Language::JavaScript, false),
        (
            "export function makeInput(): number { return 1; }\n",
            Language::TypeScript,
            false,
        ),
        (PYTHON_SOURCE, Language::Python, false),
    ];
    for (source, language, expected) in cases {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        assert_eq!(
            file_is_test_by_syntax(&tree, source.as_bytes(), language),
            expected,
            "{language:?} file-level verdict"
        );
    }
}
