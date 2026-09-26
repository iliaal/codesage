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
            "use_list" | "use_as_clause" => {}
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
        // nullable / union / intersection / DNF arms, typed property, variadic,
        // closure and arrow-function return types
        15..=22 => Some(ReferenceKind::TypeHint),
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
        // Preserve Call for decorators and Java annotations so existing kind filters keep working.
        7..=10 => Some(ReferenceKind::Call),
        11 => Some(ReferenceKind::Inheritance),
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
        13 => Some(ReferenceKind::Import),     // external module declaration
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
        21 => Some(ReferenceKind::TypeHint),
        _ => None,
    }
}

fn python_base_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = match node.kind() {
            "identifier" => return Some(node),
            "attribute" => return node.child_by_field_name("attribute"),
            "subscript" => node.child_by_field_name("value")?,
            "parenthesized_expression" => node.named_child(0)?,
            _ => return None,
        };
    }
}

fn ts_type_is_declaration(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "class"
        | "class_declaration"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "type_parameter"
        | "mapped_type_clause" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "infer_type" => parent
            .named_child(0)
            .is_some_and(|name| name.id() == node.id()),
        _ => false,
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
/// expression, the constructor of a `new` expression, the receiver of a
/// `.call` / `.apply` chain that is finally called, or a `.bind(...)` result
/// that is itself invoked, each directly or through parentheses.
/// `(function () { ... })()` and `(function () { ... }).call(this)` run when
/// their enclosing scope does, so they defer nothing by themselves; a stored
/// `.bind(this)` or an unread `.call` only produces another function.
/// Generator calls only create iterators; a directly chained `.next()` is a
/// separate execution step. Stored iterators are not followed through bindings.
fn is_immediately_invoked(function: &Node, source: &[u8]) -> bool {
    let generator = matches!(
        function.kind(),
        "generator_function" | "generator_function_declaration"
    );
    let mut node = *function;
    loop {
        let mut parent = node.parent();
        while let Some(p) = parent {
            if p.kind() != "parenthesized_expression" {
                break;
            }
            node = p;
            parent = p.parent();
        }
        let Some(p) = parent else {
            return false;
        };
        let is_field = |field: &str| {
            p.child_by_field_name(field)
                .is_some_and(|child| child.id() == node.id())
        };
        match p.kind() {
            "call_expression" => {
                return is_field("function")
                    && (!generator || iterator_is_immediately_advanced(&p, source));
            }
            "new_expression" => return !generator && is_field("constructor"),
            "member_expression" if is_field("object") => {
                let property = p
                    .child_by_field_name("property")
                    .map(|prop| crate::parse::node_text_lossy(&prop, source))
                    .unwrap_or_default();
                match property.as_str() {
                    // Calling `f.call` calls `f`; keep climbing from the member.
                    "call" | "apply" => node = p,
                    // Only a bound function that is then called counts.
                    "bind" => match p.parent() {
                        Some(bind_call)
                            if bind_call.kind() == "call_expression"
                                && bind_call
                                    .child_by_field_name("function")
                                    .is_some_and(|f| f.id() == p.id()) =>
                        {
                            node = bind_call
                        }
                        _ => return false,
                    },
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
}

/// Recognize the bounded positive case `generator().next()`, including
/// parentheses around the iterator. This does not infer consumption elsewhere.
fn iterator_is_immediately_advanced(iterator: &Node, source: &[u8]) -> bool {
    let mut node = *iterator;
    while let Some(parent) = node.parent() {
        if parent.kind() != "parenthesized_expression" {
            break;
        }
        node = parent;
    }
    let Some(member) = node.parent() else {
        return false;
    };
    if member.kind() != "member_expression"
        || member
            .child_by_field_name("object")
            .is_none_or(|n| n.id() != node.id())
        || !member
            .child_by_field_name("property")
            .is_some_and(|n| n.utf8_text(source) == Ok("next"))
    {
        return false;
    }
    is_immediately_invoked(&member, source)
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

/// True when a TypeScript import/export row belongs to a directive the
/// compiler erases, so it loads nothing at module load time:
/// `import type ...`, `export type { .. } from`, `import type X = require(..)`,
/// a clause whose every named specifier is `type`, or (for a binding row) the
/// row's own `type` specifier. A default or namespace binding, or any value
/// specifier, keeps the directive load-time.
fn ts_import_is_type_only(node: &Node) -> bool {
    let has_type_token = |n: &Node| {
        let mut cursor = n.walk();
        n.children(&mut cursor)
            .any(|child| !child.is_named() && child.kind() == "type")
    };
    let mut current = Some(*node);
    while let Some(n) = current {
        match n.kind() {
            "import_specifier" | "export_specifier" if has_type_token(&n) => return true,
            "import_statement" | "export_statement" => {
                if has_type_token(&n) {
                    return true;
                }
                let mut cursor = n.walk();
                let clause = n
                    .named_children(&mut cursor)
                    .find(|child| matches!(child.kind(), "import_clause" | "export_clause"));
                return clause.is_some_and(|clause| {
                    let specifiers: Vec<Node> = if clause.kind() == "export_clause" {
                        let mut cursor = clause.walk();
                        clause.named_children(&mut cursor).collect()
                    } else {
                        let mut cursor = clause.walk();
                        let parts: Vec<Node> = clause.named_children(&mut cursor).collect();
                        match parts.as_slice() {
                            [named] if named.kind() == "named_imports" => {
                                let mut cursor = named.walk();
                                named.named_children(&mut cursor).collect()
                            }
                            _ => return false,
                        }
                    };
                    !specifiers.is_empty()
                        && specifiers.iter().all(|specifier| {
                            matches!(specifier.kind(), "import_specifier" | "export_specifier")
                                && has_type_token(specifier)
                        })
                });
            }
            "string"
            | "string_fragment"
            | "identifier"
            | "import_clause"
            | "named_imports"
            | "namespace_import"
            | "import_specifier"
            | "export_clause"
            | "export_specifier"
            | "import_require_clause" => current = n.parent(),
            _ => return false,
        }
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
        // row must not exist at all; see `crate::preproc`.
        if dead.covers(&ref_cap.node) {
            continue;
        }
        let rhs =
            rhs_idx.and_then(|idx| m.captures().iter().find(|c| c.index == idx).map(|c| c.node));
        let node = if language == Language::Python && m.pattern_index == 11 {
            let Some(base) = python_base_name(ref_cap.node) else {
                continue;
            };
            base
        } else {
            ref_cap.node
        };
        if language == Language::TypeScript && m.pattern_index == 21 && ts_type_is_declaration(node)
        {
            continue;
        }
        pending.push(Pending {
            pattern: m.pattern_index,
            node,
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
        // PHP's relative class types name the enclosing class hierarchy, not
        // a class called `self`; PHP keywords are case-insensitive.
        if language == Language::Php
            && kind == ReferenceKind::TypeHint
            && ["self", "static", "parent"]
                .iter()
                .any(|pseudo| stripped.eq_ignore_ascii_case(pseudo))
        {
            continue;
        }

        // Grouped-import leaves are captured bare; prepend the enclosing base
        // path so the stored name resolves the same way a flat import does.
        let to_name = match language {
            Language::Rust if p.pattern == 13 => {
                if rust_module_has_path_attribute(ref_node.parent(), source) {
                    continue;
                }
                let mut parts = vec![stripped.trim_start_matches("r#").to_string()];
                let mut ancestor = ref_node.parent().and_then(|node| node.parent());
                while let Some(node) = ancestor {
                    if node.kind() == "mod_item"
                        && let Some(name) = node.child_by_field_name("name")
                    {
                        if rust_module_has_path_attribute(Some(node), source) {
                            parts.clear();
                            break;
                        }
                        parts.push(
                            crate::parse::node_text_lossy(&name, source)
                                .trim_start_matches("r#")
                                .to_string(),
                        );
                    }
                    ancestor = node.parent();
                }
                if parts.is_empty() {
                    continue;
                }
                parts.reverse();
                // A module declaration names a file, never an item in its parent.
                format!("./{}", parts.join("/"))
            }
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
        ) && (import_is_lazy(&ref_node, source, language)
            || (language == Language::TypeScript && ts_import_is_type_only(&ref_node)));
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
                    to: None,
                    from_line: None,
                    is_test: false,
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
            to: None,
            from_line: None,
            is_test: false,
        });
    }

    Ok(refs)
}

fn rust_module_has_path_attribute(node: Option<tree_sitter::Node<'_>>, source: &[u8]) -> bool {
    let mut previous = node.and_then(|node| node.prev_named_sibling());
    while let Some(attribute) = previous {
        if matches!(attribute.kind(), "line_comment" | "block_comment") {
            previous = attribute.prev_named_sibling();
            continue;
        }
        if attribute.kind() != "attribute_item" {
            break;
        }
        if let Some(inner) = attribute.named_child(0)
            && let Some(name) = inner.named_child(0)
        {
            let name = crate::parse::node_text_lossy(&name, source);
            if name == "path" || (name == "cfg_attr" && rust_path_assignment(inner, source)) {
                return true;
            }
        }
        previous = attribute.prev_named_sibling();
    }
    false
}

fn rust_path_assignment(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier"
            && crate::parse::node_text_lossy(&node, source) == "path"
            && node.next_sibling().is_some_and(|next| next.kind() == "=")
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
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
    fn typescript_type_only_directives_are_lazy_and_value_imports_are_not() {
        let src = "import type { A } from './a';\n\
                   import { type B, type C } from './b';\n\
                   import { type D, E } from './d';\n\
                   export type { F } from './f';\n\
                   export { type G } from './g';\n\
                   export { type H, I } from './h';\n\
                   import type J from './j';\n\
                   import type * as K from './k';\n\
                   import L, { type M } from './l';\n\
                   import './side';\n\
                   export * from './all';\n\
                   import N = require('./n');\n\
                   import type O = require('./o');\n\
                   import {} from './empty';\n\
                   export type P = { q: number };\n\
                   export function use(x: A): number { return x.q; }\n";
        let refs = refs_from_source(src, Language::TypeScript);
        for spec in ["./a", "./b", "./f", "./g", "./j", "./k", "./o"] {
            assert_eq!(lazy_flags(&refs, spec), vec![true], "{spec}");
        }
        for spec in ["./d", "./h", "./l", "./side", "./all", "./n", "./empty"] {
            assert_eq!(lazy_flags(&refs, spec), vec![false], "{spec}");
        }
        // Binding rows follow their own specifier inside a mixed clause.
        assert_eq!(lazy_flags(&refs, "B"), vec![true]);
        assert_eq!(lazy_flags(&refs, "D"), vec![true]);
        assert_eq!(lazy_flags(&refs, "E"), vec![false]);
        assert_eq!(lazy_flags(&refs, "L"), vec![false]);
        assert_eq!(lazy_flags(&refs, "M"), vec![true]);
        assert_eq!(lazy_flags(&refs, "N"), vec![false]);
    }

    #[test]
    fn javascript_has_no_type_only_directives() {
        let src = "import { b } from './b.js';\nexport { c } from './c.js';\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(lazy_flags(&refs, "./b.js"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./c.js"), vec![false]);
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
        let src = "(function () { require('./call'); }).call(this);\n(function () { require('./apply'); }.apply(null, []));\nnew (function () { require('./ctor'); })();\n(function () { require('./chain'); }).call.call(null, this);\n(function () { require('./bound-now'); }).bind(this)();\n(function () { require('./bound-call'); }).bind(this).call(null);\nconst bound = (function () { require('./bound'); }).bind(this);\nconst later = (function () { require('./later'); }).call;\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(lazy_flags(&refs, "./call"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./apply"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./chain"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./bound-now"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./bound-call"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./ctor"), vec![false]);
        assert_eq!(lazy_flags(&refs, "./bound"), vec![true]);
        // `.call` read but never invoked.
        assert_eq!(lazy_flags(&refs, "./later"), vec![true]);
    }

    #[test]
    fn generator_creation_keeps_imports_lazy_until_direct_advancement() {
        let source = r#"
(function* () { require('./direct'); })();
(async function* () { require('./async'); })();
(function* () { require('./call'); }).call(null);
(function* () { require('./apply'); }).apply(null, []);
(function* () { require('./bound'); }).bind(null)();
(function* () { require('./advanced'); })().next();
((function* () { require('./advanced-call'); }).call(null)).next();
(function* () { require('./unread-next'); })().next;
function later() { (function* () { require('./nested'); })().next(); }
(function () { require('./ordinary'); })();
"#;
        for language in [Language::JavaScript, Language::TypeScript] {
            let tree = crate::parse::parse_file(source.as_bytes(), language).unwrap();
            assert!(!tree.root_node().has_error(), "{language:?}");
            let refs = refs_from_source(source, language);
            for name in [
                "./direct",
                "./async",
                "./call",
                "./apply",
                "./bound",
                "./unread-next",
                "./nested",
            ] {
                assert_eq!(lazy_flags(&refs, name), vec![true], "{language:?}: {name}");
            }
            for name in ["./advanced", "./advanced-call", "./ordinary"] {
                assert_eq!(lazy_flags(&refs, name), vec![false], "{language:?}: {name}");
            }
        }
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

    #[test]
    fn rust_external_modules_keep_their_scope_without_claiming_inline_modules() {
        let refs = refs_from_source(
            "#[doc = \"path=example\"] mod sibling; mod outer { pub mod child; mod nested { mod leaf; } } mod r#type { mod r#match; }",
            Language::Rust,
        );
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .map(|r| r.to_name.as_str())
            .collect();
        assert_eq!(
            imports,
            [
                "./sibling",
                "./outer/child",
                "./outer/nested/leaf",
                "./type/match"
            ]
        );
    }

    #[test]
    fn rust_path_attributes_do_not_claim_the_default_module_file() {
        let refs = refs_from_source(
            "#[path = \"elsewhere.rs\"] /* module */ mod redirected; #[path = \"elsewhere\"] mod outer { mod child; } #[cfg_attr(feature = \"a\", path = \"other.rs\")] mod conditional;",
            Language::Rust,
        );
        assert!(refs.iter().all(|r| r.kind != ReferenceKind::Import));
    }

    #[test]
    fn rust_nested_cfg_attributes_distinguish_path_assignments_from_quoted_text() {
        let refs = refs_from_source(
            r#"#[cfg_attr(feature = "a", cfg_attr(feature = "b", path = "other.rs"))] mod redirected;
               #[cfg_attr(feature = "a", doc = "path = example.rs")] mod preserved;"#,
            Language::Rust,
        );
        let imports: Vec<_> = refs
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Import)
            .map(|reference| reference.to_name.as_str())
            .collect();
        assert_eq!(imports, ["./preserved"]);
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
