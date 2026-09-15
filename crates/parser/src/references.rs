use std::sync::LazyLock;

use anyhow::Result;
use codesage_protocol::{Language, Reference, ReferenceKind};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

static PHP_REF_QUERY: &str = include_str!("queries/php_refs.scm");
static PYTHON_REF_QUERY: &str = include_str!("queries/python_refs.scm");
static C_REF_QUERY: &str = include_str!("queries/c_refs.scm");
static CPP_REF_QUERY: &str = include_str!("queries/cpp_refs.scm");
static JAVA_REF_QUERY: &str = include_str!("queries/java_refs.scm");
static RUST_REF_QUERY: &str = include_str!("queries/rust_refs.scm");
static JS_REF_QUERY: &str = include_str!("queries/javascript_refs.scm");
// TS `extends_clause` and type patterns cannot compile against the JS grammar.
static TS_REF_QUERY: &str = include_str!("queries/typescript_refs.scm");
static GO_REF_QUERY: &str = include_str!("queries/go_refs.scm");

/// Compiled reference query + cached capture indices, lazily initialized once
/// per language. `rhs_idx` is `Some` only for JS/TS, whose value-destructure
/// patterns capture the right-hand side as `@rhs` for the import-binding
/// filter in `extract_references`.
struct RefQuerySpec {
    query: Query,
    ref_idx: u32,
    rhs_idx: Option<u32>,
}

fn compile_ref_query(lang: tree_sitter::Language, src: &str) -> RefQuerySpec {
    let query = Query::new(&lang, src).expect("embedded .scm reference query compiles");
    let ref_idx = query
        .capture_index_for_name("ref")
        .expect("embedded .scm has @ref capture");
    let rhs_idx = query.capture_index_for_name("rhs");
    RefQuerySpec {
        query,
        ref_idx,
        rhs_idx,
    }
}

static PHP_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::Php), PHP_REF_QUERY));
static PY_REF: LazyLock<RefQuerySpec> = LazyLock::new(|| {
    compile_ref_query(
        crate::parse::ts_language(Language::Python),
        PYTHON_REF_QUERY,
    )
});
static C_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::C), C_REF_QUERY));
static CPP_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::Cpp), CPP_REF_QUERY));
static JAVA_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::Java), JAVA_REF_QUERY));
static RUST_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::Rust), RUST_REF_QUERY));
static JS_REF: LazyLock<RefQuerySpec> = LazyLock::new(|| {
    compile_ref_query(
        crate::parse::ts_language(Language::JavaScript),
        JS_REF_QUERY,
    )
});
static TS_REF: LazyLock<RefQuerySpec> = LazyLock::new(|| {
    compile_ref_query(
        crate::parse::ts_language(Language::TypeScript),
        TS_REF_QUERY,
    )
});
static GO_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(crate::parse::ts_language(Language::Go), GO_REF_QUERY));

fn ref_query_for(lang: Language) -> &'static RefQuerySpec {
    match lang {
        Language::Php => &PHP_REF,
        Language::Python => &PY_REF,
        Language::C => &C_REF,
        Language::Cpp => &CPP_REF,
        Language::Java => &JAVA_REF,
        Language::Rust => &RUST_REF,
        Language::JavaScript => &JS_REF,
        Language::TypeScript => &TS_REF,
        Language::Go => &GO_REF,
    }
}

/// Every (language, reference-query-source) pair. Counterpart to
/// [`crate::extract::SYMBOL_QUERY_SOURCES`]; iterated by `crate::validate`.
pub(crate) const REF_QUERY_SOURCES: &[(Language, &str)] = &[
    (Language::Php, PHP_REF_QUERY),
    (Language::Python, PYTHON_REF_QUERY),
    (Language::C, C_REF_QUERY),
    (Language::Cpp, CPP_REF_QUERY),
    (Language::Java, JAVA_REF_QUERY),
    (Language::Rust, RUST_REF_QUERY),
    (Language::JavaScript, JS_REF_QUERY),
    (Language::TypeScript, TS_REF_QUERY),
    (Language::Go, GO_REF_QUERY),
];

/// Rust grouped-use prefix: for `use a::b::{X, Y}`, the leaf captured inside the
/// `use_list` is bare (`X`). Walk up through any chain of enclosing
/// `scoped_use_list` nodes, collecting their `path` fields, so the stored import
/// name resolves to `a::b::X`. Returns `None` when the node is not inside a
/// grouped use (e.g. a call or macro identifier).
fn rust_grouped_use_prefix(node: &Node, source: &[u8]) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "use_list" => {}
            "scoped_use_list" => {
                if let Some(path) = n.child_by_field_name("path")
                    && let Ok(text) = path.utf8_text(source)
                {
                    segments.push(text.to_string());
                }
            }
            _ => break,
        }
        current = n.parent();
    }
    if segments.is_empty() {
        None
    } else {
        segments.reverse();
        Some(segments.join("::"))
    }
}

/// PHP group-use prefix: for `use App\Models\{User, Post};`, the clause leaf is
/// the bare suffix (`User`). Walk up to the enclosing `namespace_use_declaration`
/// and read its base `namespace_name` so the stored import resolves to
/// `App\Models\User`. Returns `None` for a plain (non-group) use, whose clause
/// already carries the full name.
fn php_group_use_prefix(node: &Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "namespace_use_clause" | "namespace_use_group" => {}
            "namespace_use_declaration" => {
                let mut cursor = n.walk();
                for child in n.named_children(&mut cursor) {
                    if child.kind() == "namespace_name" {
                        return child.utf8_text(source).ok().map(str::to_string);
                    }
                }
                return None;
            }
            _ => return None,
        }
        current = n.parent();
    }
    None
}

fn php_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import), // namespace_use_declaration
        1 => Some(ReferenceKind::Call),   // function_call_expression
        2 => Some(ReferenceKind::Instantiation), // object_creation_expression
        3..=6 => Some(ReferenceKind::Call), // scoped scope/name, member, nullsafe method names
        7 | 8 => Some(ReferenceKind::Inheritance), // class extends / implements
        9 => Some(ReferenceKind::TraitUse), // use_declaration inside class
        10..=13 => Some(ReferenceKind::TypeHint), // param / promoted-property / return type hints
        14 => Some(ReferenceKind::Import), // group use (use App\Models\{User, Post};)
        _ => None,
    }
}

fn python_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import),        // import statement
        1 => Some(ReferenceKind::Import),        // from X import (module)
        2 => Some(ReferenceKind::ImportBinding), // from X import Y (specific name)
        3 => Some(ReferenceKind::ImportBinding), // from X import Y as Z (aliased)
        4 | 5 => Some(ReferenceKind::Call),      // call expression
        6 => Some(ReferenceKind::Import),        // relative import module (from . import x)
        // Classify decorators as calls, matching Java annotations.
        7..=10 => Some(ReferenceKind::Call),
        _ => None,
    }
}

fn c_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 | 1 => Some(ReferenceKind::Include), // preproc_include (system_lib_string, string_literal)
        2 => Some(ReferenceKind::Call),        // call_expression
        _ => None,
    }
}

fn cpp_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 | 1 => Some(ReferenceKind::Include),       // preproc_include
        2..=5 => Some(ReferenceKind::Call), // bare / qualified / member / template-fn calls
        6..=8 => Some(ReferenceKind::Instantiation), // new T / new ns::T / new T<U>
        9..=11 => Some(ReferenceKind::Inheritance), // base class clauses
        12 | 13 => Some(ReferenceKind::Import), // using-declaration
        _ => None,
    }
}

fn java_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Call),              // method_invocation
        1..=3 => Some(ReferenceKind::Instantiation), // object_creation_expression
        4..=9 => Some(ReferenceKind::Inheritance),   // extends / implements
        10 | 11 => Some(ReferenceKind::Import),      // import_declaration
        // Classify annotations as calls, matching Python decorators.
        12..=15 => Some(ReferenceKind::Call),
        _ => None,
    }
}

fn rust_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 | 1 => Some(ReferenceKind::Import),  // use_declaration
        2 | 3 => Some(ReferenceKind::Call),    // call_expression
        4 | 5 => Some(ReferenceKind::Call),    // macro_invocation
        6 => Some(ReferenceKind::Call),        // method call (obj.method())
        7 => Some(ReferenceKind::Inheritance), // impl Trait for Type
        8 => Some(ReferenceKind::TypeHint),    // type of an impl block
        9..=12 => Some(ReferenceKind::Import), // renamed / glob / grouped / braced use
        _ => None,
    }
}

fn go_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import),
        1 | 2 => Some(ReferenceKind::Call),
        _ => None,
    }
}

fn js_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import),             // import statement
        1 => Some(ReferenceKind::Import),             // require("module")
        2 => Some(ReferenceKind::Call),               // call (identifier)
        3 => Some(ReferenceKind::Call),               // call (member expression)
        4 => Some(ReferenceKind::Import),             // re-export (export ... from "src")
        5 => Some(ReferenceKind::Inheritance),        // class Foo extends Bar (JS heritage)
        6 => Some(ReferenceKind::Instantiation),      // new Foo()
        7..=15 => Some(ReferenceKind::ImportBinding), // import / re-export / require bindings
        16 => Some(ReferenceKind::ImportBinding),     // member access off an import binding
        17 => Some(ReferenceKind::ImportBinding),     // const x = require("m").default
        _ => None,
    }
}

/// TypeScript reference kinds. See `typescript_refs.scm`; the pattern order
/// diverges from JS because the JS-only `class_heritage (identifier)`
/// inheritance form is an impossible pattern under the TSX grammar and is
/// dropped, shifting instantiation/inheritance up by one.
fn ts_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import),             // import statement
        1 => Some(ReferenceKind::Import),             // require("module")
        2 => Some(ReferenceKind::Call),               // call (identifier)
        3 => Some(ReferenceKind::Call),               // call (member expression)
        4 => Some(ReferenceKind::Import),             // re-export (export ... from "src")
        5 => Some(ReferenceKind::Instantiation),      // new Foo()
        6 => Some(ReferenceKind::Inheritance),        // class Foo extends Bar (TS extends_clause)
        7..=15 => Some(ReferenceKind::ImportBinding), // import / re-export / require bindings
        16 => Some(ReferenceKind::ImportBinding),     // member access off an import binding
        17 => Some(ReferenceKind::ImportBinding),     // const x = require("m").default
        18 => Some(ReferenceKind::ImportBinding),     // import x = require("m") (binding)
        19 => Some(ReferenceKind::Import),            // import x = require("m") (module)
        20 => Some(ReferenceKind::ImportBinding),     // type via module namespace (ns.Type)
        _ => None,
    }
}

/// JS/TS patterns whose `@ref` is a LOCAL name bound to an imported module:
/// `import` clauses (7-9), `const m = require(...)` (15), `const m =
/// require(...).default` (17), and the TS `import m = require(...)` (18).
/// These form the allowlist the receiver-gated patterns consult.
fn js_ts_binding_pattern(language: Language, pattern: usize) -> bool {
    match language {
        Language::JavaScript => matches!(pattern, 7..=9 | 15 | 17),
        Language::TypeScript => matches!(pattern, 7..=9 | 15 | 17 | 18),
        _ => false,
    }
}

/// JS/TS patterns that capture a receiver as `@rhs` and only name a module
/// export when that receiver is a same-file import binding: value
/// destructuring (13-14), non-call member access (16), and the TS
/// namespace-qualified type (20).
fn js_ts_receiver_gated_pattern(language: Language, pattern: usize) -> bool {
    match language {
        Language::JavaScript => matches!(pattern, 13 | 14 | 16),
        Language::TypeScript => matches!(pattern, 13 | 14 | 16 | 20),
        _ => false,
    }
}

/// True when `property` (the `@ref` of a member-access pattern) sits in a
/// member expression that is the callee of a call: `axios.get(...)`. Pattern 3
/// already records that property as a `Call`, so the member-access pattern
/// must not emit a second row for it. The inner receiver of a chained call
/// (`axios.CancelToken` in `axios.CancelToken.source()`) is not a callee and
/// stays.
fn member_is_callee(property: &Node) -> bool {
    let Some(member) = property.parent() else {
        return false;
    };
    let Some(call) = member.parent() else {
        return false;
    };
    call.kind() == "call_expression"
        && call
            .child_by_field_name("function")
            .is_some_and(|f| f.id() == member.id())
}

/// Node kinds whose body defers an import directive to call time. Only
/// languages that resolve imports at runtime qualify: a C/C++ `#include` in a
/// function body is still textual inclusion, a Rust `use` in a function is a
/// compile-time alias, and PHP `use`, Java `import`, and Go `import` are only
/// admitted at file or namespace scope.
fn lazy_scope_kinds(language: Language) -> &'static [&'static str] {
    match language {
        Language::Python => &["function_definition"],
        Language::JavaScript | Language::TypeScript => &[
            "function_declaration",
            "function_expression",
            "generator_function_declaration",
            "generator_function",
            "arrow_function",
            "method_definition",
        ],
        Language::Rust
        | Language::C
        | Language::Cpp
        | Language::Php
        | Language::Java
        | Language::Go => &[],
    }
}

/// True when `function` runs where it is written: the callee of a call
/// expression, the constructor of a `new` expression, or the receiver of
/// `.call(...)` / `.apply(...)`, each directly or through parentheses.
/// `(function () { ... })()` and `(function () { ... }).call(this)` run when
/// their enclosing scope does, so they defer nothing by themselves; `.bind`
/// only produces another function.
fn is_immediately_invoked(function: &Node, source: &[u8]) -> bool {
    let mut callee = *function;
    let mut parent = function.parent();
    while let Some(p) = parent {
        match p.kind() {
            "parenthesized_expression" => {
                callee = p;
                parent = p.parent();
            }
            "call_expression" => {
                return p
                    .child_by_field_name("function")
                    .is_some_and(|f| f.id() == callee.id());
            }
            "new_expression" => {
                return p
                    .child_by_field_name("constructor")
                    .is_some_and(|c| c.id() == callee.id());
            }
            "member_expression" => {
                let is_receiver = p
                    .child_by_field_name("object")
                    .is_some_and(|o| o.id() == callee.id());
                let invokes = p.child_by_field_name("property").is_some_and(|prop| {
                    matches!(
                        crate::parse::node_text_lossy(&prop, source).as_str(),
                        "call" | "apply"
                    )
                });
                let called = p.parent().is_some_and(|call| {
                    call.kind() == "call_expression"
                        && call
                            .child_by_field_name("function")
                            .is_some_and(|f| f.id() == p.id())
                });
                return is_receiver && invokes && called;
            }
            _ => return false,
        }
    }
    false
}

/// True when an import directive sits inside a function, method, closure, or
/// arrow-function body that is not immediately invoked, for `language`.
fn import_is_lazy(node: &Node, source: &[u8], language: Language) -> bool {
    let kinds = lazy_scope_kinds(language);
    if kinds.is_empty() {
        return false;
    }
    let mut current = node.parent();
    while let Some(n) = current {
        if kinds.contains(&n.kind()) && !is_immediately_invoked(&n, source) {
            return true;
        }
        current = n.parent();
    }
    false
}

/// The pattern-index → `ReferenceKind` map for a language. Counterpart to
/// `crate::extract::kind_map_for`; shared by `extract_references` and the
/// validation gate so both agree on what each `@ref` pattern means.
pub(crate) fn ref_kind_map_for(language: Language) -> fn(usize) -> Option<ReferenceKind> {
    match language {
        Language::Php => php_ref_kind,
        Language::Python => python_ref_kind,
        Language::C => c_ref_kind,
        Language::Cpp => cpp_ref_kind,
        Language::Java => java_ref_kind,
        Language::Rust => rust_ref_kind,
        Language::JavaScript => js_ref_kind,
        Language::TypeScript => ts_ref_kind,
        Language::Go => go_ref_kind,
    }
}

pub fn extract_references(
    tree: &Tree,
    source: &[u8],
    language: Language,
    file_path: &str,
) -> Result<Vec<Reference>> {
    let kind_map = ref_kind_map_for(language);
    let spec = ref_query_for(language);
    let query = &spec.query;
    let name_idx = spec.ref_idx;

    let root = tree.root_node();
    let dead = crate::preproc::DeadRegions::scan(root, source, language);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);
    let rhs_idx = spec.rhs_idx;

    // Receiver filtering needs all same-file import bindings before judging matches.
    struct Pending<'a> {
        pattern: usize,
        node: tree_sitter::Node<'a>,
        rhs: Option<tree_sitter::Node<'a>>,
    }
    let mut pending = Vec::new();
    while let Some(m) = matches.next() {
        let Some(ref_cap) = m.captures().iter().find(|c| c.index == name_idx) else {
            continue;
        };
        // A reference parked in a dead `#if 0` arm exists in no build, so the
        // row must not exist at all — see `crate::preproc`.
        if dead.covers(&ref_cap.node) {
            continue;
        }
        let rhs =
            rhs_idx.and_then(|idx| m.captures().iter().find(|c| c.index == idx).map(|c| c.node));
        pending.push(Pending {
            pattern: m.pattern_index,
            node: ref_cap.node,
            rhs,
        });
    }

    // Only imported receivers identify module exports; arbitrary object keys do not.
    let imports_js_ts = matches!(language, Language::JavaScript | Language::TypeScript);
    let import_bindings: std::collections::HashSet<String> = if imports_js_ts {
        pending
            .iter()
            .filter(|p| js_ts_binding_pattern(language, p.pattern))
            .map(|p| crate::parse::node_text_lossy(&p.node, source))
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut refs = Vec::new();
    for p in &pending {
        let Some(kind) = kind_map(p.pattern) else {
            continue;
        };

        if imports_js_ts && js_ts_receiver_gated_pattern(language, p.pattern) {
            let rhs_bound = p
                .rhs
                .map(|rhs| import_bindings.contains(&crate::parse::node_text_lossy(&rhs, source)))
                .unwrap_or(false);
            if !rhs_bound {
                continue;
            }
        }

        // Pattern 16 (JS and TS): a member expression in callee position is
        // already pattern 3's `Call` row.
        if imports_js_ts && p.pattern == 16 && member_is_callee(&p.node) {
            continue;
        }

        let ref_node = p.node;
        let raw = crate::parse::node_text_lossy(&ref_node, source);
        let stripped = strip_surrounding_quotes(&raw);
        if stripped.is_empty() {
            continue;
        }

        // Grouped-import leaves are captured bare; prepend the enclosing base
        // path so the stored name resolves the same way a flat import does.
        let to_name = match language {
            Language::Rust => rust_grouped_use_prefix(&ref_node, source)
                .map_or_else(|| stripped.to_string(), |p| format!("{p}::{stripped}")),
            Language::Php => php_group_use_prefix(&ref_node, source)
                .map_or_else(|| stripped.to_string(), |p| format!("{p}\\{stripped}")),
            Language::Python if matches!(p.pattern, 1 | 6) => {
                let glob = ref_node.parent().is_some_and(|statement| {
                    let mut cursor = statement.walk();
                    statement
                        .named_children(&mut cursor)
                        .any(|child| child.kind() == "wildcard_import")
                });
                if glob {
                    let separator = if stripped.ends_with('.') { "" } else { "." };
                    format!("{stripped}{separator}*")
                } else {
                    stripped.to_string()
                }
            }
            _ => stripped.to_string(),
        };

        let (row, col) = crate::position::node_start_utf8(&ref_node, source);
        let lazy = matches!(
            kind,
            ReferenceKind::Import | ReferenceKind::ImportBinding | ReferenceKind::Include
        ) && import_is_lazy(&ref_node, source, language);
        if language == Language::Python && kind == ReferenceKind::ImportBinding {
            let statement = ref_node.parent().and_then(|parent| {
                if parent.kind() == "aliased_import" {
                    parent.parent()
                } else {
                    Some(parent)
                }
            });
            if let Some(module) = statement.and_then(|node| node.child_by_field_name("module_name"))
            {
                let module = crate::parse::node_text_lossy(&module, source);
                let separator = if module.ends_with('.') { "" } else { "." };
                refs.push(Reference {
                    from_file: file_path.to_string(),
                    from_symbol: None,
                    to_name: format!("{module}{separator}{to_name}"),
                    kind: ReferenceKind::Import,
                    line: row + 1,
                    col,
                    lazy,
                });
            }
        }
        refs.push(Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name,
            kind,
            line: row + 1,
            col,
            lazy,
        });
    }

    Ok(refs)
}

/// Strip matching quotes, preserving partial one-byte captures from malformed input.
fn strip_surrounding_quotes(s: &str) -> &str {
    if s.len() < 2 {
        return s;
    }
    let bytes = s.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::strip_surrounding_quotes;

    #[test]
    fn strips_balanced_double_quotes() {
        assert_eq!(strip_surrounding_quotes("\"foo\""), "foo");
    }

    #[test]
    fn strips_balanced_single_quotes() {
        assert_eq!(strip_surrounding_quotes("'foo'"), "foo");
    }

    #[test]
    fn leaves_unquoted_unchanged() {
        assert_eq!(strip_surrounding_quotes("foo"), "foo");
    }

    #[test]
    fn does_not_panic_on_single_bare_quote() {
        assert_eq!(strip_surrounding_quotes("\""), "\"");
        assert_eq!(strip_surrounding_quotes("'"), "'");
    }

    #[test]
    fn leaves_empty_string_unchanged() {
        assert_eq!(strip_surrounding_quotes(""), "");
    }

    #[test]
    fn leaves_mismatched_quotes_unchanged() {
        assert_eq!(strip_surrounding_quotes("\"foo'"), "\"foo'");
        assert_eq!(strip_surrounding_quotes("'foo\""), "'foo\"");
    }

    use codesage_protocol::{Language, Reference, ReferenceKind};

    #[test]
    fn python_import_bindings_keep_their_module_context() {
        let refs = refs_from_source(
            "from api import run as call\nfrom . import child\n",
            Language::Python,
        );
        assert!(
            refs.iter()
                .any(|r| r.to_name == "run" && r.kind == ReferenceKind::ImportBinding)
        );
        assert!(
            !refs
                .iter()
                .any(|r| r.to_name == "run" && r.kind == ReferenceKind::Import)
        );
        assert!(
            refs.iter()
                .any(|r| r.to_name == "api" && r.kind == ReferenceKind::Import)
        );
        assert!(
            refs.iter()
                .any(|r| r.to_name == "api.run" && r.kind == ReferenceKind::Import)
        );
        assert!(
            refs.iter()
                .any(|r| r.to_name == ".child" && r.kind == ReferenceKind::Import)
        );
    }

    fn lazy_flags(refs: &[Reference], name: &str) -> Vec<bool> {
        refs.iter()
            .filter(|r| r.to_name == name)
            .map(|r| r.lazy)
            .collect()
    }

    #[test]
    fn python_function_body_imports_are_lazy_and_module_scope_imports_are_not() {
        let src = "import os\nfrom a import run\nclass K:\n    import json\n    def m(self):\n        import sys\n        from b import other as o\ndef f():\n    from . import child\n    return other()\n";
        let refs = refs_from_source(src, Language::Python);
        assert_eq!(lazy_flags(&refs, "os"), vec![false]);
        assert_eq!(lazy_flags(&refs, "a"), vec![false]);
        assert_eq!(lazy_flags(&refs, "run"), vec![false]);
        assert_eq!(lazy_flags(&refs, "a.run"), vec![false]);
        assert_eq!(lazy_flags(&refs, "json"), vec![false]);
        assert_eq!(lazy_flags(&refs, "sys"), vec![true]);
        assert_eq!(lazy_flags(&refs, "b"), vec![true]);
        assert_eq!(lazy_flags(&refs, "b.other"), vec![true]);
        assert_eq!(lazy_flags(&refs, ".child"), vec![true]);
        // Binding row (lazy) plus the call row inside f (never lazy).
        assert_eq!(lazy_flags(&refs, "other"), vec![true, false]);
    }

    #[test]
    fn javascript_require_inside_functions_is_lazy() {
        let src = "import top from './top.js';\nconst eager = require('./eager');\nfunction f() { const { x } = require('./fn'); return x; }\nconst g = () => require('./arrow');\nclass C { m() { return require('./method'); } }\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(lazy_flags(&refs, "./top.js"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./eager"), vec![false]);
        assert_eq!(lazy_flags(&refs, "eager"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./fn"), vec![true]);
        assert_eq!(lazy_flags(&refs, "x"), vec![true]);
        assert_eq!(lazy_flags(&refs, "./arrow"), vec![true]);
        assert_eq!(lazy_flags(&refs, "./method"), vec![true]);
        // Call rows are never lazy, wherever they sit.
        assert_eq!(lazy_flags(&refs, "require"), vec![false; 4]);
    }

    #[test]
    fn typescript_require_inside_functions_is_lazy() {
        let src = "import top = require('./top');\nexport function f(): number { const m = require('./fn'); return m.x; }\n";
        let refs = refs_from_source(src, Language::TypeScript);
        assert_eq!(lazy_flags(&refs, "./top"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./fn"), vec![true]);
        assert_eq!(lazy_flags(&refs, "m"), vec![true]);
    }

    #[test]
    fn javascript_module_scope_iife_require_is_eager_but_nested_iife_is_lazy() {
        let src = "(function () { const a = require('./iife'); })();\n(() => require('./arrow-iife'))();\n(async function () { require('./async-iife'); }());\nfunction outer() { (function () { require('./inner'); })(); }\nconst h = (function () { return require('./factory'); });\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(lazy_flags(&refs, "./iife"), vec![false]);
        assert_eq!(lazy_flags(&refs, "a"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./arrow-iife"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./async-iife"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./inner"), vec![true]);
        // Parenthesized but never called: still a deferred body.
        assert_eq!(lazy_flags(&refs, "./factory"), vec![true]);
    }

    #[test]
    fn javascript_call_apply_and_new_invoked_wrappers_are_eager_but_bind_is_lazy() {
        let src = "(function () { require('./call'); }).call(this);\n(function () { require('./apply'); }.apply(null, []));\nnew (function () { require('./ctor'); })();\nconst bound = (function () { require('./bound'); }).bind(this);\nconst later = (function () { require('./later'); }).call;\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(lazy_flags(&refs, "./call"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./apply"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./ctor"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./bound"), vec![true]);
        // `.call` read but never invoked.
        assert_eq!(lazy_flags(&refs, "./later"), vec![true]);
    }

    #[test]
    fn python_nested_and_async_function_imports_are_lazy() {
        let src = "async def a():\n    import aio\ndef outer():\n    def inner():\n        import deep\n    return inner\n";
        let refs = refs_from_source(src, Language::Python);
        assert_eq!(lazy_flags(&refs, "aio"), vec![true]);
        assert_eq!(lazy_flags(&refs, "deep"), vec![true]);
    }

    #[test]
    fn rust_function_local_use_stays_eager() {
        let src = "use crate::top::A;\nfn f() {\n    use crate::inner::B;\n    let c = || { use crate::closure::C; C::new() };\n    B::new(); c()\n}\n";
        let refs = refs_from_source(src, Language::Rust);
        assert_eq!(lazy_flags(&refs, "crate::top::A"), vec![false]);
        assert_eq!(lazy_flags(&refs, "crate::inner::B"), vec![false]);
        assert_eq!(lazy_flags(&refs, "crate::closure::C"), vec![false]);
    }

    #[test]
    fn c_and_cpp_include_inside_a_function_body_stays_eager() {
        let src = "#include <stdio.h>\n#include \"top.h\"\nint main(void) {\n#include \"body.h\"\n    return 0;\n}\n";
        let refs = refs_from_source(src, Language::C);
        assert_eq!(lazy_flags(&refs, "top.h"), vec![false]);
        assert_eq!(lazy_flags(&refs, "body.h"), vec![false]);
        let refs = refs_from_source(src, Language::Cpp);
        assert_eq!(lazy_flags(&refs, "body.h"), vec![false]);
    }

    #[test]
    fn php_and_java_and_go_imports_are_never_lazy() {
        let php = refs_from_source(
            "<?php\nnamespace App;\nuse App\\Models\\User;\nclass K { function m() { return new User(); } }\n",
            Language::Php,
        );
        assert!(php.iter().all(|r| !r.lazy), "{php:?}");
        let java = refs_from_source(
            "import java.util.List;\nclass K { void m() { List<String> l = null; } }\n",
            Language::Java,
        );
        assert!(java.iter().all(|r| !r.lazy), "{java:?}");
        let go = refs_from_source(
            "package main\nimport \"fmt\"\nfunc main() { fmt.Println() }\n",
            Language::Go,
        );
        assert!(go.iter().all(|r| !r.lazy), "{go:?}");
    }

    fn refs_from_source(source: &str, language: Language) -> Vec<Reference> {
        let bytes = source.as_bytes();
        let tree = crate::parse::parse_file(bytes, language).unwrap();
        super::extract_references(&tree, bytes, language, "inline").unwrap()
    }

    fn rows(refs: &[Reference], name: &str, kind: ReferenceKind) -> usize {
        refs.iter()
            .filter(|r| r.to_name == name && r.kind == kind)
            .count()
    }

    #[test]
    fn javascript_member_access_off_an_import_binding_names_the_property() {
        let src = "import axios from './lib/axios.js';\n\
                   const source = axios.CancelToken.source();\n\
                   assert.strictEqual(typeof axios.CancelToken, 'function');\n\
                   const t = new axios.CancelToken(fn);\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 3);
        assert_eq!(rows(&refs, "source", ReferenceKind::Call), 1);
        assert_eq!(rows(&refs, "source", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "strictEqual", ReferenceKind::ImportBinding), 0);
    }

    #[test]
    fn javascript_member_access_off_a_require_binding_names_the_property() {
        let src = "const exports = require('axios');\n\
                   expect(typeof exports.CancelToken).toBe('function');\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 1);
    }

    #[test]
    fn javascript_member_access_off_an_unbound_receiver_is_ignored() {
        // Dynamic `await import(...)` bindings are intentionally unsupported.
        let src = "import axios from './lib/axios.js';\n\
                   const response = await axios.get('/x');\n\
                   const body = response.data;\n\
                   const exports = (await import('axios'));\n\
                   expect(typeof exports.CancelToken).toBe('function');\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "data", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "get", ReferenceKind::Call), 1);
        assert_eq!(rows(&refs, "get", ReferenceKind::ImportBinding), 0);
    }

    #[test]
    fn javascript_require_default_binds_the_local_for_later_unpacks() {
        let src = "const axios = require('axios').default;\n\
                   const { CanceledError } = axios;\n\
                   const e = axios.AxiosError;\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "axios", ReferenceKind::Import), 1);
        assert_eq!(rows(&refs, "axios", ReferenceKind::ImportBinding), 1);
        assert_eq!(
            rows(&refs, "CanceledError", ReferenceKind::ImportBinding),
            1
        );
        assert_eq!(rows(&refs, "AxiosError", ReferenceKind::ImportBinding), 1);
    }

    #[test]
    fn typescript_member_access_and_namespaced_types_off_an_import_binding() {
        let src = "import axios from 'axios';\n\
                   const source = axios.CancelToken.source();\n\
                   const h: axios.AxiosHeaders = new axios.AxiosHeaders();\n\
                   const r = await axios.get('/x');\n";
        let refs = refs_from_source(src, Language::TypeScript);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 1);
        assert_eq!(rows(&refs, "AxiosHeaders", ReferenceKind::ImportBinding), 2);
        assert_eq!(rows(&refs, "source", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "get", ReferenceKind::Call), 1);
        assert_eq!(rows(&refs, "get", ReferenceKind::ImportBinding), 0);
    }

    #[test]
    fn typescript_import_equals_require_binds_the_module_and_the_local() {
        let src = "import axios = require('axios');\n\
                   const t = new axios.CancelToken((c: axios.Canceler) => {});\n";
        let refs = refs_from_source(src, Language::TypeScript);
        assert_eq!(rows(&refs, "axios", ReferenceKind::Import), 1);
        assert_eq!(rows(&refs, "axios", ReferenceKind::ImportBinding), 1);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 1);
        assert_eq!(rows(&refs, "Canceler", ReferenceKind::ImportBinding), 1);
    }

    #[test]
    fn typescript_member_access_and_types_off_an_unbound_receiver_are_ignored() {
        let src = "const body = response.data;\n\
                   const h: ns.Header = make();\n";
        let refs = refs_from_source(src, Language::TypeScript);
        assert_eq!(rows(&refs, "data", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "Header", ReferenceKind::ImportBinding), 0);
    }
}
