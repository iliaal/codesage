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

/// Compiled reference query + cached @ref capture index, lazily initialized
/// once per language.
struct RefQuerySpec {
    query: Query,
    ref_idx: u32,
}

fn compile_ref_query(lang: tree_sitter::Language, src: &str) -> RefQuerySpec {
    let query = Query::new(&lang, src).expect("embedded .scm reference query compiles");
    let ref_idx = query
        .capture_index_for_name("ref")
        .expect("embedded .scm has @ref capture");
    RefQuerySpec { query, ref_idx }
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
        0 => Some(ReferenceKind::Import),        // import statement
        1 => Some(ReferenceKind::Import),        // require("module")
        2 => Some(ReferenceKind::Call),          // call (identifier)
        3 => Some(ReferenceKind::Call),          // call (member expression)
        4 => Some(ReferenceKind::Import),        // re-export (export ... from "src")
        5 => Some(ReferenceKind::Inheritance),   // class Foo extends Bar (JS heritage)
        6 => Some(ReferenceKind::Instantiation), // new Foo()
        _ => None,
    }
}

/// TypeScript reference kinds. See `typescript_refs.scm`; the pattern order
/// diverges from JS because the JS-only `class_heritage (identifier)`
/// inheritance form is an impossible pattern under the TSX grammar and is
/// dropped, shifting instantiation/inheritance up by one.
fn ts_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import),        // import statement
        1 => Some(ReferenceKind::Import),        // require("module")
        2 => Some(ReferenceKind::Call),          // call (identifier)
        3 => Some(ReferenceKind::Call),          // call (member expression)
        4 => Some(ReferenceKind::Import),        // re-export (export ... from "src")
        5 => Some(ReferenceKind::Instantiation), // new Foo()
        6 => Some(ReferenceKind::Inheritance),   // class Foo extends Bar (TS extends_clause)
        _ => None,
    }
}

pub fn extract_references(
    tree: &Tree,
    source: &[u8],
    language: Language,
    file_path: &str,
) -> Result<Vec<Reference>> {
    let kind_map: fn(usize) -> Option<ReferenceKind> = match language {
        Language::Php => php_ref_kind,
        Language::Python => python_ref_kind,
        Language::C => c_ref_kind,
        Language::Cpp => cpp_ref_kind,
        Language::Java => java_ref_kind,
        Language::Rust => rust_ref_kind,
        Language::JavaScript => js_ref_kind,
        Language::TypeScript => ts_ref_kind,
        Language::Go => go_ref_kind,
    };

    let spec = ref_query_for(language);
    let query = &spec.query;
    let name_idx = spec.ref_idx;

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);

    let mut refs = Vec::new();

    while let Some(m) = matches.next() {
        let Some(kind) = kind_map(m.pattern_index) else {
            continue;
        };

        let Some(ref_cap) = m.captures.iter().find(|c| c.index == name_idx) else {
            continue;
        };

        let ref_node = ref_cap.node;
        let raw = ref_node.utf8_text(source).unwrap_or("");
        let stripped = strip_surrounding_quotes(raw);
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
}
