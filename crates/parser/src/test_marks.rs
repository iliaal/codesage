//! Symbol-level test marks: is this definition test code by the language's
//! own conventions? File-level classification (`tests/` directories,
//! `*_test.go`, `*Test.php`, ...) lives in `discover::is_test_like_path` and
//! is applied by the indexer; this module only sees the syntax tree.

use codesage_protocol::Language;
use tree_sitter::Node;

/// Rust attribute paths that make a function a test. `#[test_case(...)]`
/// and `#[rstest(...)]` carry arguments; the path is compared without them.
const RUST_TEST_ATTRIBUTES: &[&str] = &[
    "test",
    "tokio::test",
    "rstest",
    "sqlx::test",
    "async_std::test",
    "test_case",
];

const JS_TEST_CALLEES: &[&str] = &["describe", "it", "test"];

const JAVA_TEST_ANNOTATIONS: &[&str] = &["Test", "ParameterizedTest"];

/// Per-file facts computed once before symbols are walked.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FileTestContext {
    /// A JavaScript/TypeScript program with a top-level `describe(` / `it(` /
    /// `test(` call is a test file; every symbol in it is test code.
    pub(crate) js_top_level_test_call: bool,
    /// A Rust file headed by the inner attribute `#![cfg(test)]` compiles
    /// only in a test build, so every item in it is test code.
    pub(crate) rust_inner_cfg_test: bool,
}

impl FileTestContext {
    pub(crate) fn scan(root: &Node, source: &[u8], language: Language) -> Self {
        match language {
            Language::JavaScript | Language::TypeScript => Self {
                js_top_level_test_call: js_program_has_top_level_test_call(root, source),
                rust_inner_cfg_test: false,
            },
            Language::Rust => Self {
                js_top_level_test_call: false,
                rust_inner_cfg_test: rust_file_has_inner_cfg_test(root, source),
            },
            _ => Self::default(),
        }
    }

    /// Whether the file's own syntax makes every definition in it test code,
    /// the way a test-like path does. The indexer ORs this with the discovery
    /// path heuristic before storing `files.is_test`.
    pub(crate) fn marks_whole_file(self) -> bool {
        self.js_top_level_test_call || self.rust_inner_cfg_test
    }
}

/// Whether `def_node` is test code by the language's syntax-level rules.
/// Ancestors count: a helper defined inside a `#[cfg(test)]` module, a
/// `Test*` class, a `describe` callback, or a `test_*` function is test code.
pub(crate) fn symbol_is_test(
    def_node: &Node,
    source: &[u8],
    language: Language,
    ctx: FileTestContext,
) -> bool {
    match language {
        Language::Rust => ctx.rust_inner_cfg_test || rust_is_test(def_node, source),
        Language::Python => python_is_test(def_node, source),
        Language::JavaScript | Language::TypeScript => {
            ctx.js_top_level_test_call || js_inside_test_call(def_node, source)
        }
        Language::Java => java_is_test(def_node, source),
        Language::Php | Language::C | Language::Cpp | Language::Go => false,
    }
}

fn node_text(node: &Node, source: &[u8]) -> String {
    crate::parse::node_text_lossy(node, source)
}

/// Attribute text with whitespace removed and the `#[` / `#![` / `]` shell
/// dropped, e.g. `cfg(test)` or `tokio::test(flavor="multi_thread")`.
fn rust_attribute_body(attribute_item: &Node, source: &[u8]) -> String {
    let mut cursor = attribute_item.walk();
    let inner = attribute_item
        .children(&mut cursor)
        .find(|c| c.kind() == "attribute")
        .unwrap_or(*attribute_item);
    node_text(&inner, source)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Outer attributes of a Rust item are `attribute_item` siblings written
/// directly above it; comments between them do not break the run.
fn rust_outer_attributes(item: &Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev = item.prev_sibling();
    while let Some(node) = prev {
        match node.kind() {
            "attribute_item" => out.push(rust_attribute_body(&node, source)),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = node.prev_sibling();
    }
    out
}

/// Split a comma-separated attribute argument list on the commas that sit
/// outside every nested parenthesis, so `feature="x",all(test)` yields two
/// items rather than three.
fn split_top_level_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&args[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&args[start..]);
    out
}

/// Whether a `cfg(...)` predicate (whitespace already stripped) holds only in
/// a test build: a bare `test`, or an `all(...)` list with a test-only member
/// at any position or nesting depth. `not(test)` and `any(test, ...)` can also
/// hold outside a test build, so neither marks.
fn rust_cfg_predicate_is_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    predicate
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|args| {
            split_top_level_args(args)
                .into_iter()
                .any(rust_cfg_predicate_is_test)
        })
}

fn rust_attribute_is_cfg_test(body: &str) -> bool {
    body.strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(rust_cfg_predicate_is_test)
}

/// `#![cfg(test)]` written at the top of a file gates the whole file. Inner
/// attributes are only legal there, so the scan stays on the root's children.
fn rust_file_has_inner_cfg_test(root: &Node, source: &[u8]) -> bool {
    let mut cursor = root.walk();
    root.children(&mut cursor).any(|child| {
        child.kind() == "inner_attribute_item"
            && rust_attribute_is_cfg_test(&rust_attribute_body(&child, source))
    })
}

fn rust_attribute_is_test_marker(body: &str) -> bool {
    let path = body.split('(').next().unwrap_or(body);
    RUST_TEST_ATTRIBUTES.contains(&path)
}

/// The item itself or any enclosing item carries `#[cfg(test)]`, or the item
/// or an enclosing function carries a test-marker attribute.
fn rust_is_test(def_node: &Node, source: &[u8]) -> bool {
    let mut current = Some(*def_node);
    while let Some(node) = current {
        match node.kind() {
            "mod_item"
            | "function_item"
            | "impl_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "macro_definition"
            | "function_signature_item" => {
                for body in rust_outer_attributes(&node, source) {
                    if rust_attribute_is_cfg_test(&body) {
                        return true;
                    }
                    if node.kind() == "function_item" && rust_attribute_is_test_marker(&body) {
                        return true;
                    }
                }
            }
            _ => {}
        }
        current = node.parent();
    }
    false
}

/// A `def`'s or `class`'s decorators live on the wrapping
/// `decorated_definition`. `@pytest.mark.<x>`, `@pytest.fixture`, and their
/// call forms mark the definition as test code.
fn python_has_pytest_decorator(definition: &Node, source: &[u8]) -> bool {
    let Some(parent) = definition.parent() else {
        return false;
    };
    if parent.kind() != "decorated_definition" {
        return false;
    }
    let mut cursor = parent.walk();
    parent.children(&mut cursor).any(|child| {
        child.kind() == "decorator" && {
            let text: String = node_text(&child, source)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let body = text.trim_start_matches('@');
            body.starts_with("pytest.mark.") || body.starts_with("pytest.fixture")
        }
    })
}

fn python_definition_name(definition: &Node, source: &[u8]) -> Option<String> {
    definition
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))
}

/// `test_*` functions, `Test*` classes, pytest-decorated definitions, and
/// everything nested inside one of those.
fn python_is_test(def_node: &Node, source: &[u8]) -> bool {
    let mut current = Some(*def_node);
    while let Some(node) = current {
        let marked = match node.kind() {
            "function_definition" => {
                python_definition_name(&node, source).is_some_and(|name| name.starts_with("test_"))
                    || python_has_pytest_decorator(&node, source)
            }
            // `Test` followed by an uppercase letter, digit, or nothing;
            // `Testament` is a word, not a suite.
            "class_definition" => {
                python_definition_name(&node, source).is_some_and(|name| {
                    name.strip_prefix("Test")
                        .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_lowercase()))
                }) || python_has_pytest_decorator(&node, source)
            }
            _ => false,
        };
        if marked {
            return true;
        }
        current = node.parent();
    }
    false
}

/// The identifier a `describe.each(...)`, `it.only(...)`, or plain
/// `test(...)` call ultimately names.
fn js_callee_root<'a>(function: &Node<'a>) -> Option<Node<'a>> {
    let mut node = *function;
    loop {
        match node.kind() {
            "identifier" => return Some(node),
            "member_expression" => node = node.child_by_field_name("object")?,
            // `describe.each([...])("name", fn)`: the inner call names it.
            "call_expression" => node = node.child_by_field_name("function")?,
            _ => return None,
        }
    }
}

fn js_call_is_test(call: &Node, source: &[u8]) -> bool {
    call.child_by_field_name("function")
        .and_then(|f| js_callee_root(&f))
        .is_some_and(|id| JS_TEST_CALLEES.contains(&node_text(&id, source).as_str()))
}

/// Whether the program's top level declares its own `describe`, `it`, or
/// `test` (a function declaration, or a `const` / `let` / `var` declarator
/// with that plain name, exported or not). A framework import binds through
/// `import_statement` or a destructuring pattern and does not count.
fn js_program_binds_test_callee(root: &Node, source: &[u8]) -> bool {
    let mut cursor = root.walk();
    root.children(&mut cursor).any(|statement| {
        let declaration = if statement.kind() == "export_statement" {
            match statement.child_by_field_name("declaration") {
                Some(declaration) => declaration,
                None => return false,
            }
        } else {
            statement
        };
        match declaration.kind() {
            "function_declaration" => declaration
                .child_by_field_name("name")
                .is_some_and(|name| JS_TEST_CALLEES.contains(&node_text(&name, source).as_str())),
            "lexical_declaration" | "variable_declaration" => {
                let mut inner = declaration.walk();
                declaration.children(&mut inner).any(|declarator| {
                    declarator.kind() == "variable_declarator"
                        && declarator.child_by_field_name("name").is_some_and(|name| {
                            name.kind() == "identifier"
                                && JS_TEST_CALLEES.contains(&node_text(&name, source).as_str())
                        })
                })
            }
            _ => false,
        }
    })
}

/// A top-level `describe(` / `it(` / `test(` call marks the program as a
/// test file unless the program itself declares that callee: a product
/// module with its own `function test(...)` is calling its own code.
fn js_program_has_top_level_test_call(root: &Node, source: &[u8]) -> bool {
    if js_program_binds_test_callee(root, source) {
        return false;
    }
    let mut cursor = root.walk();
    root.children(&mut cursor).any(|statement| {
        statement.kind() == "expression_statement"
            && statement.child(0).is_some_and(|expr| {
                expr.kind() == "call_expression" && js_call_is_test(&expr, source)
            })
    })
}

fn js_inside_test_call(def_node: &Node, source: &[u8]) -> bool {
    let mut current = def_node.parent();
    while let Some(node) = current {
        if node.kind() == "call_expression" && js_call_is_test(&node, source) {
            return true;
        }
        current = node.parent();
    }
    false
}

/// A Java method carrying `@Test` or `@ParameterizedTest` (simple or
/// qualified, with or without arguments).
fn java_is_test(def_node: &Node, source: &[u8]) -> bool {
    if def_node.kind() != "method_declaration" {
        return false;
    }
    let mut cursor = def_node.walk();
    let Some(modifiers) = def_node
        .children(&mut cursor)
        .find(|c| c.kind() == "modifiers")
    else {
        return false;
    };
    let mut inner = modifiers.walk();
    modifiers.children(&mut inner).any(|child| {
        matches!(child.kind(), "marker_annotation" | "annotation")
            && child.child_by_field_name("name").is_some_and(|name| {
                let text = node_text(&name, source);
                let simple = text.rsplit('.').next().unwrap_or(&text);
                JAVA_TEST_ANNOTATIONS.contains(&simple)
            })
    })
}
