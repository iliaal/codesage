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
// TypeScript mirrors JavaScript's reference patterns (import / require / call /
// member-call / re-export / instantiation) but adds one TS-only pattern for
// class inheritance: TS wraps the superclass in an `extends_clause` node that
// does not exist in the JavaScript grammar, so it cannot live in the shared
// JS file (the query would fail to compile against tree-sitter-javascript).
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
        0 => Some(ReferenceKind::Import),   // import statement
        1 => Some(ReferenceKind::Import),   // from X import (module)
        2 => Some(ReferenceKind::Import),   // from X import Y (specific name)
        3 => Some(ReferenceKind::Import),   // from X import Y as Z (aliased)
        4 | 5 => Some(ReferenceKind::Call), // call expression
        6 => Some(ReferenceKind::Import),   // relative import module (from . import x)
        // Decorators (@property, @retry(..), @app.route, @app.route(..)).
        // Filed as Call to match Java's annotation handling, so decoration
        // sites surface through the same kind query.
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
        // Annotation usages (`@Override`, `@Test(...)`, `@pkg.Foo`). Filed as
        // Call to match Python decorator handling — agents querying
        // `find_references("Test", kind="call")` get the decoration sites.
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
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);
    let rhs_idx = spec.rhs_idx;

    // Collect first: the JS/TS value-destructure filter below needs the full
    // set of same-file import bindings before it can judge any one match.
    // Nodes borrow the tree, so holding them past the cursor is free.
    struct Pending<'a> {
        pattern: usize,
        node: tree_sitter::Node<'a>,
        rhs: Option<tree_sitter::Node<'a>>,
    }
    let mut pending = Vec::new();
    while let Some(m) = matches.next() {
        let Some(ref_cap) = m.captures.iter().find(|c| c.index == name_idx) else {
            continue;
        };
        let rhs =
            rhs_idx.and_then(|idx| m.captures.iter().find(|c| c.index == idx).map(|c| c.node));
        pending.push(Pending {
            pattern: m.pattern_index,
            node: ref_cap.node,
            rhs,
        });
    }

    // Names bound to an imported module in this file (see
    // `js_ts_binding_pattern`). A value-destructure `const { X } = rhs` or a
    // member access `rhs.X` only names a module export when `rhs` is one of
    // these; otherwise it is an arbitrary object (`const { data } =
    // response`, `response.data`) and its keys are not references to anything.
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

        // Receiver-gated patterns (JS and TS): keep only shapes whose receiver
        // is a same-file import binding. `const { Axios } = axios` and
        // `axios.CancelToken` (imported) stay; `const { data } = response` and
        // `response.data` (not imported) are dropped.
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
            _ => stripped.to_string(),
        };

        let (row, col) = crate::position::node_start_utf8(&ref_node, source);
        refs.push(Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name,
            kind,
            line: row + 1,
            col,
        });
    }

    Ok(refs)
}

/// Strip a single pair of matching surrounding `"` or `'` quotes from a
/// reference token (import source paths, `require()` arguments). Returns
/// the inner slice on a match, the original on no match.
///
/// Length guard avoids a slice panic on a tree-sitter `(string)` capture
/// of a single bare quote — possible from malformed/truncated source where
/// the parser still emits a partial node. Without it,
/// `s[1..s.len() - 1]` on a 1-byte string panics with
/// `slice index starts at 1 but ends at 0` and aborts the indexer worker.
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
        // Regression: the previous inline `[1..len-1]`
        // slice panicked on `"\""`, aborting the rayon-parallel indexer
        // worker for the entire run when tree-sitter emitted a 1-byte
        // string capture on malformed input.
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
        // `axios.CancelToken.source()`: pattern 3 records the callee `source`;
        // the receiver `CancelToken` used to be recorded nowhere.
        let src = "import axios from './lib/axios.js';\n\
                   const source = axios.CancelToken.source();\n\
                   assert.strictEqual(typeof axios.CancelToken, 'function');\n\
                   const t = new axios.CancelToken(fn);\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 3);
        assert_eq!(rows(&refs, "source", ReferenceKind::Call), 1);
        // Callee members stay pattern 3's row only: no second row for `source`
        // or `strictEqual`.
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
        // Nothing binds `response` or `exports` to a module here, so neither
        // `data` nor `CancelToken` may become a reference: `response.data`
        // would otherwise make every HTTP test a dependent of any symbol
        // named `data`. This is the deliberate gap: a receiver bound by
        // `await import(...)` is also unbound and its members are dropped.
        let src = "import axios from './lib/axios.js';\n\
                   const response = await axios.get('/x');\n\
                   const body = response.data;\n\
                   const exports = (await import('axios'));\n\
                   expect(typeof exports.CancelToken).toBe('function');\n";
        let refs = refs_from_source(src, Language::JavaScript);
        assert_eq!(rows(&refs, "data", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "CancelToken", ReferenceKind::ImportBinding), 0);
        assert_eq!(rows(&refs, "get", ReferenceKind::Call), 1);
        // The callee member is pattern 3's row alone: `member_is_callee`
        // keeps pattern 16 from adding a duplicate ImportBinding for `get`.
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
