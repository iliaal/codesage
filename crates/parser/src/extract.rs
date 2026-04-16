use anyhow::{Context, Result};
use codesage_protocol::{Language, Symbol, SymbolKind};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

static PHP_QUERY: &str = include_str!("queries/php.scm");
static PYTHON_QUERY: &str = include_str!("queries/python.scm");
static C_QUERY: &str = include_str!("queries/c.scm");
static RUST_QUERY: &str = include_str!("queries/rust.scm");
static JS_QUERY: &str = include_str!("queries/javascript.scm");
static TS_QUERY: &str = include_str!("queries/typescript.scm");
static GO_QUERY: &str = include_str!("queries/go.scm");

fn php_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        2 => Some(SymbolKind::Method),
        3 => Some(SymbolKind::Trait),
        4 => Some(SymbolKind::Interface),
        5 => Some(SymbolKind::Enum),
        6 => Some(SymbolKind::Constant),
        7 => Some(SymbolKind::Namespace),
        _ => None,
    }
}

fn python_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        _ => None,
    }
}

fn c_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0..=2 => Some(SymbolKind::Function), // normal, pointer-return, macro-wrapped
        3 => Some(SymbolKind::Struct),
        4 => Some(SymbolKind::Enum),
        5 => Some(SymbolKind::Constant), // typedef
        6 => Some(SymbolKind::Macro),
        _ => None,
    }
}

fn rust_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Struct),
        2 => Some(SymbolKind::Enum),
        3 => Some(SymbolKind::Trait),
        4 => Some(SymbolKind::Constant), // type alias
        5 => Some(SymbolKind::Constant), // const
        6 => Some(SymbolKind::Constant), // static
        7 => Some(SymbolKind::Module),
        8 => Some(SymbolKind::Macro),
        _ => None,
    }
}

fn js_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        2 => Some(SymbolKind::Method),
        3 | 4 => Some(SymbolKind::Constant), // exported/top-level const
        5 => Some(SymbolKind::Class),        // export default class
        6 => Some(SymbolKind::Constant),     // exports.X = ...
        _ => None,
    }
}

fn go_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Method),
        2 => Some(SymbolKind::Struct),    // placeholder; refined by refine_go_type_kind
        3 => Some(SymbolKind::Constant),  // type alias (type X = Y)
        4 => Some(SymbolKind::Constant),  // const
        _ => None,
    }
}

fn ts_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        2 => Some(SymbolKind::Method),
        3 => Some(SymbolKind::Interface),
        4 => Some(SymbolKind::Constant), // type alias
        5 => Some(SymbolKind::Enum),
        6 | 7 => Some(SymbolKind::Constant), // exported/top-level const
        8 => Some(SymbolKind::Class),        // export default class
        _ => None,
    }
}

pub fn extract_symbols(
    tree: &Tree,
    source: &[u8],
    language: Language,
    file_path: &str,
) -> Result<Vec<Symbol>> {
    let ts_language = match language {
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    };

    let (query_src, kind_map): (&str, fn(usize) -> Option<SymbolKind>) = match language {
        Language::Php => (PHP_QUERY, php_kind_map),
        Language::Python => (PYTHON_QUERY, python_kind_map),
        Language::C => (C_QUERY, c_kind_map),
        Language::Rust => (RUST_QUERY, rust_kind_map),
        Language::JavaScript => (JS_QUERY, js_kind_map),
        Language::TypeScript => (TS_QUERY, ts_kind_map),
        Language::Go => (GO_QUERY, go_kind_map),
    };

    let query =
        Query::new(&ts_language, query_src).context("failed to compile tree-sitter query")?;

    let name_idx = query
        .capture_index_for_name("name")
        .context("query has no @name capture")?;
    let def_idx = query
        .capture_index_for_name("def")
        .context("query has no @def capture")?;

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let namespace = match language {
        Language::Php => find_php_namespace(&root, source),
        _ => None,
    };

    let mut symbols = Vec::new();
    let mut seen_defs = std::collections::HashSet::new();

    while let Some(m) = matches.next() {
        let Some(kind) = kind_map(m.pattern_index) else {
            continue;
        };

        let name_capture = m.captures.iter().find(|c| c.index == name_idx);
        let def_capture = m.captures.iter().find(|c| c.index == def_idx);

        let (Some(name_cap), Some(def_cap)) = (name_capture, def_capture) else {
            continue;
        };

        let name_node = name_cap.node;
        let def_node = def_cap.node;

        let def_id = (def_node.start_byte(), def_node.end_byte());
        if !seen_defs.insert(def_id) {
            continue;
        }

        let name = name_node.utf8_text(source).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }

        if kind == SymbolKind::Namespace {
            continue;
        }

        let mut kind = kind;
        if kind == SymbolKind::Function
            && (language == Language::Python || language == Language::Rust)
            && is_inside_impl_or_class(&def_node, language)
        {
            kind = SymbolKind::Method;
        }

        if language == Language::Go && kind == SymbolKind::Struct {
            kind = refine_go_type_kind(&def_node);
        }

        let qualified_name =
            build_qualified_name(&name, kind, &def_node, source, language, &namespace);

        let start = def_node.start_position();
        let end = def_node.end_position();

        symbols.push(Symbol {
            name,
            qualified_name,
            kind,
            file_path: file_path.to_string(),
            line_start: start.row as u32 + 1,
            line_end: end.row as u32 + 1,
            col_start: start.column as u32,
            col_end: end.column as u32,
        });
    }

    Ok(symbols)
}

fn find_php_namespace(root: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_definition"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            return Some(name_node.utf8_text(source).ok()?.to_string());
        }
    }
    None
}

fn is_inside_impl_or_class(node: &Node, language: Language) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition" | "class_declaration" if language == Language::Python => {
                return true;
            }
            "impl_item" if language == Language::Rust => return true,
            _ => current = parent.parent(),
        }
    }
    false
}

fn find_parent_class_name<'a>(node: &Node, source: &'a [u8]) -> Option<&'a str> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition"
            | "class_declaration"
            | "trait_declaration"
            | "interface_declaration"
            | "enum_declaration" => {
                let name_node = parent.child_by_field_name("name")?;
                return name_node.utf8_text(source).ok();
            }
            "impl_item" => {
                let type_node = parent.child_by_field_name("type")?;
                return type_node.utf8_text(source).ok();
            }
            _ => current = parent.parent(),
        }
    }
    None
}

fn build_qualified_name(
    name: &str,
    kind: SymbolKind,
    def_node: &Node,
    source: &[u8],
    language: Language,
    namespace: &Option<String>,
) -> String {
    match language {
        Language::Php => {
            let mut parts = Vec::new();
            if let Some(ns) = namespace {
                parts.push(ns.as_str().to_string());
            }
            if (kind == SymbolKind::Method || kind == SymbolKind::Constant)
                && let Some(class_name) = find_parent_class_name(def_node, source)
            {
                parts.push(class_name.to_string());
            }
            parts.push(name.to_string());
            parts.join("\\")
        }
        Language::Python => {
            if kind == SymbolKind::Method
                && let Some(class_name) = find_parent_class_name(def_node, source)
            {
                return format!("{class_name}.{name}");
            }
            name.to_string()
        }
        Language::C => name.to_string(),
        Language::Rust => {
            if kind == SymbolKind::Method
                && let Some(type_name) = find_parent_class_name(def_node, source)
            {
                return format!("{type_name}::{name}");
            }
            name.to_string()
        }
        Language::JavaScript | Language::TypeScript => {
            if kind == SymbolKind::Method
                && let Some(class_name) = find_parent_class_name(def_node, source)
            {
                return format!("{class_name}.{name}");
            }
            name.to_string()
        }
        Language::Go => {
            if kind == SymbolKind::Method
                && let Some(receiver_type) = find_go_receiver_type(def_node, source)
            {
                return format!("{receiver_type}.{name}");
            }
            name.to_string()
        }
    }
}

fn refine_go_type_kind(def_node: &Node) -> SymbolKind {
    let mut cursor = def_node.walk();
    for child in def_node.children(&mut cursor) {
        if child.kind() == "type_spec"
            && let Some(type_child) = child.child_by_field_name("type")
        {
            return match type_child.kind() {
                "struct_type" => SymbolKind::Struct,
                "interface_type" => SymbolKind::Interface,
                _ => SymbolKind::Constant,
            };
        }
    }
    SymbolKind::Constant
}

fn find_go_receiver_type<'a>(node: &Node, source: &'a [u8]) -> Option<&'a str> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            let type_node = child.child_by_field_name("type")?;
            if type_node.kind() == "pointer_type" {
                let mut tc = type_node.walk();
                for inner in type_node.children(&mut tc) {
                    if inner.kind() == "type_identifier" {
                        return inner.utf8_text(source).ok();
                    }
                }
            }
            return type_node.utf8_text(source).ok();
        }
    }
    None
}
