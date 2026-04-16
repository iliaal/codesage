use anyhow::{Context, Result};
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
    let ts_language = match language {
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    };

    let (query_src, kind_map): (&str, fn(usize) -> Option<ReferenceKind>) = match language {
        Language::Php => (PHP_REF_QUERY, php_ref_kind),
        Language::Python => (PYTHON_REF_QUERY, python_ref_kind),
        Language::C => (C_REF_QUERY, c_ref_kind),
        Language::Rust => (RUST_REF_QUERY, rust_ref_kind),
        Language::JavaScript => (JS_REF_QUERY, js_ref_kind),
        Language::TypeScript => (TS_REF_QUERY, js_ref_kind), // same ref structure
        Language::Go => (GO_REF_QUERY, go_ref_kind),
    };

    let query = Query::new(&ts_language, query_src).context("failed to compile reference query")?;

    let name_idx = query
        .capture_index_for_name("ref")
        .context("query has no @ref capture")?;

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

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
