use std::sync::LazyLock;

use anyhow::Result;
use codesage_protocol::{Language, Reference, ReferenceKind};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor, Tree};

static PHP_REF_QUERY: &str = include_str!("queries/php_refs.scm");
static PYTHON_REF_QUERY: &str = include_str!("queries/python_refs.scm");
static C_REF_QUERY: &str = include_str!("queries/c_refs.scm");
static RUST_REF_QUERY: &str = include_str!("queries/rust_refs.scm");
static JS_REF_QUERY: &str = include_str!("queries/javascript_refs.scm");
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
    LazyLock::new(|| compile_ref_query(tree_sitter_php::LANGUAGE_PHP.into(), PHP_REF_QUERY));
static PY_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_python::LANGUAGE.into(), PYTHON_REF_QUERY));
static C_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_c::LANGUAGE.into(), C_REF_QUERY));
static RUST_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_rust::LANGUAGE.into(), RUST_REF_QUERY));
static JS_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_javascript::LANGUAGE.into(), JS_REF_QUERY));
static TS_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_typescript::LANGUAGE_TSX.into(), TS_REF_QUERY));
static GO_REF: LazyLock<RefQuerySpec> =
    LazyLock::new(|| compile_ref_query(tree_sitter_go::LANGUAGE.into(), GO_REF_QUERY));

fn ref_query_for(lang: Language) -> &'static RefQuerySpec {
    match lang {
        Language::Php => &PHP_REF,
        Language::Python => &PY_REF,
        Language::C => &C_REF,
        Language::Rust => &RUST_REF,
        Language::JavaScript => &JS_REF,
        Language::TypeScript => &TS_REF,
        Language::Go => &GO_REF,
    }
}

fn php_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import), // namespace_use_declaration
        1 => Some(ReferenceKind::Import), // use_declaration
        2 => Some(ReferenceKind::Call),   // function_call_expression
        3 => Some(ReferenceKind::Instantiation), // object_creation_expression
        4 => Some(ReferenceKind::Call),   // scoped_call_expression
        5 => Some(ReferenceKind::Inheritance), // class extends
        6 => Some(ReferenceKind::Inheritance), // class implements
        7 => Some(ReferenceKind::TraitUse), // use_declaration inside class
        _ => None,
    }
}

fn python_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 => Some(ReferenceKind::Import), // import statement
        1 => Some(ReferenceKind::Import), // from X import (module)
        2 => Some(ReferenceKind::Import), // from X import Y (specific name)
        3 => Some(ReferenceKind::Import), // from X import Y as Z (aliased)
        4 => Some(ReferenceKind::Call),   // call expression
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

fn rust_ref_kind(pattern_index: usize) -> Option<ReferenceKind> {
    match pattern_index {
        0 | 1 => Some(ReferenceKind::Import), // use_declaration
        2 | 3 => Some(ReferenceKind::Call),   // call_expression
        4 | 5 => Some(ReferenceKind::Call),   // macro_invocation
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
        0 => Some(ReferenceKind::Import), // import statement
        1 => Some(ReferenceKind::Import), // require("module")
        2 => Some(ReferenceKind::Call),   // call (identifier)
        3 => Some(ReferenceKind::Call),   // call (member expression)
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
        Language::Rust => rust_ref_kind,
        Language::JavaScript => js_ref_kind,
        Language::TypeScript => js_ref_kind, // same ref structure
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
        let mut to_name = ref_node.utf8_text(source).unwrap_or("").to_string();
        if to_name.is_empty() {
            continue;
        }
        // Strip surrounding quotes from string literals (import sources, require args)
        if (to_name.starts_with('"') && to_name.ends_with('"'))
            || (to_name.starts_with('\'') && to_name.ends_with('\''))
        {
            to_name = to_name[1..to_name.len() - 1].to_string();
            if to_name.is_empty() {
                continue;
            }
        }

        let pos = ref_node.start_position();
        refs.push(Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name,
            kind,
            line: pos.row as u32 + 1,
            col: pos.column as u32,
        });
    }

    Ok(refs)
}
