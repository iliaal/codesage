use std::sync::LazyLock;

use anyhow::Result;
use codesage_protocol::{Language, Symbol, SymbolKind};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

static PHP_QUERY: &str = include_str!("queries/php.scm");
static PYTHON_QUERY: &str = include_str!("queries/python.scm");
static C_QUERY: &str = include_str!("queries/c.scm");
static CPP_QUERY: &str = include_str!("queries/cpp.scm");
static RUST_QUERY: &str = include_str!("queries/rust.scm");
static JS_QUERY: &str = include_str!("queries/javascript.scm");
static TS_QUERY: &str = include_str!("queries/typescript.scm");
static GO_QUERY: &str = include_str!("queries/go.scm");

/// A compiled tree-sitter symbol query plus its capture indices. Compiled once
/// per language on first use, then reused across every file. `capture_index_for_name`
/// is O(n) over captures, so caching it beside the Query matters when the indexer
/// runs through tens of thousands of files.
struct SymbolQuerySpec {
    query: Query,
    name_idx: u32,
    def_idx: u32,
}

fn compile_symbol_query(lang: tree_sitter::Language, src: &str) -> SymbolQuerySpec {
    let query = Query::new(&lang, src).expect("embedded .scm symbol query compiles");
    let name_idx = query
        .capture_index_for_name("name")
        .expect("embedded .scm has @name capture");
    let def_idx = query
        .capture_index_for_name("def")
        .expect("embedded .scm has @def capture");
    SymbolQuerySpec {
        query,
        name_idx,
        def_idx,
    }
}

static PHP_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_php::LANGUAGE_PHP.into(), PHP_QUERY));
static PY_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_python::LANGUAGE.into(), PYTHON_QUERY));
static C_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_c::LANGUAGE.into(), C_QUERY));
static CPP_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_cpp::LANGUAGE.into(), CPP_QUERY));
static RUST_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_rust::LANGUAGE.into(), RUST_QUERY));
static JS_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_javascript::LANGUAGE.into(), JS_QUERY));
static TS_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_typescript::LANGUAGE_TSX.into(), TS_QUERY));
static GO_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(tree_sitter_go::LANGUAGE.into(), GO_QUERY));

fn symbol_query_for(lang: Language) -> &'static SymbolQuerySpec {
    match lang {
        Language::Php => &PHP_SYM,
        Language::Python => &PY_SYM,
        Language::C => &C_SYM,
        Language::Cpp => &CPP_SYM,
        Language::Rust => &RUST_SYM,
        Language::JavaScript => &JS_SYM,
        Language::TypeScript => &TS_SYM,
        Language::Go => &GO_SYM,
    }
}

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

fn cpp_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        // 0..=2 free/in-class function defs (refined to Method when inside a class body).
        0..=2 => Some(SymbolKind::Function),
        // 3 out-of-line method def (Foo::bar). Already a Method.
        3 => Some(SymbolKind::Method),
        // 4 destructor, 5 operator. Refined to Method when inside a class body, else stay Function.
        4 | 5 => Some(SymbolKind::Function),
        6 => Some(SymbolKind::Class),
        7 => Some(SymbolKind::Struct),
        8 => Some(SymbolKind::Struct), // union -> Struct (no Union variant)
        9 => Some(SymbolKind::Enum),
        10 => Some(SymbolKind::Constant), // typedef
        11 => Some(SymbolKind::Constant), // using-alias
        12 => Some(SymbolKind::Constant), // concept (no Concept variant)
        13 => Some(SymbolKind::Macro),
        14..=18 => Some(SymbolKind::Method), // in-class declarations (no body)
        19..=22 => Some(SymbolKind::Function), // in-class definitions (refined to Method)
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
        2 => Some(SymbolKind::Struct), // placeholder; refined by refine_go_type_kind
        3 => Some(SymbolKind::Constant), // type alias (type X = Y)
        4 => Some(SymbolKind::Constant), // const
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
    let kind_map: fn(usize) -> Option<SymbolKind> = match language {
        Language::Php => php_kind_map,
        Language::Python => python_kind_map,
        Language::C => c_kind_map,
        Language::Cpp => cpp_kind_map,
        Language::Rust => rust_kind_map,
        Language::JavaScript => js_kind_map,
        Language::TypeScript => ts_kind_map,
        Language::Go => go_kind_map,
    };

    let spec = symbol_query_for(language);
    let query = &spec.query;
    let name_idx = spec.name_idx;
    let def_idx = spec.def_idx;

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);

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

        let captured_name = name_node.utf8_text(source).unwrap_or("").to_string();
        if captured_name.is_empty() {
            continue;
        }

        if kind == SymbolKind::Namespace {
            continue;
        }

        let mut kind = kind;
        if kind == SymbolKind::Function
            && (language == Language::Python
                || language == Language::Rust
                || language == Language::Cpp)
            && is_inside_impl_or_class(&def_node, language)
        {
            kind = SymbolKind::Method;
        }

        // C++ captures `Foo::bar` for out-of-line methods; the bare `bar` lives
        // in `name`, the full path in `qualified_name`.
        let name = if language == Language::Cpp {
            cpp_bare_name(&captured_name)
        } else {
            captured_name.clone()
        };
        if name.is_empty() {
            continue;
        }

        if language == Language::Go && kind == SymbolKind::Struct {
            kind = refine_go_type_kind(&def_node);
        }

        let qualified_name = if language == Language::Cpp {
            cpp_qualified_name(&name, &captured_name, kind, &def_node, source)
        } else {
            build_qualified_name(&name, kind, &def_node, source, language, &namespace)
        };

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
            "class_specifier" | "struct_specifier" | "union_specifier"
                if language == Language::Cpp =>
            {
                return true;
            }
            _ => current = parent.parent(),
        }
    }
    false
}

/// Extract the bare identifier from a captured C++ name.
///
/// Most captures already are the bare identifier (e.g. `bar` for an in-class
/// method, `Foo` for a class). Patterns that capture `qualified_identifier`
/// (out-of-line `void Foo::bar() {}`) yield `Foo::bar`; this helper returns
/// `bar` in that case so symbol-name search hits the same way it does for
/// in-class definitions. Destructors (`~Foo`) and operators (`operator+`) are
/// returned as-is because the leading marker is part of the conventional
/// search term.
fn cpp_bare_name(captured: &str) -> String {
    captured.rsplit("::").next().unwrap_or(captured).to_string()
}

/// Walk up from a definition collecting every enclosing C++ namespace name.
/// Outermost first, joined with `::`. Returns `None` when the def is at file
/// scope. Handles both `namespace ns { ... }` (single name) and the C++17
/// `namespace ns1::ns2 { ... }` form (nested_namespace_specifier).
fn find_cpp_namespace(node: &Node, source: &[u8]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "namespace_definition"
            && let Some(name_node) = parent.child_by_field_name("name")
        {
            match name_node.kind() {
                "namespace_identifier" => {
                    if let Ok(text) = name_node.utf8_text(source) {
                        parts.push(text.to_string());
                    }
                }
                "nested_namespace_specifier" => {
                    let mut walker = name_node.walk();
                    let mut nested_parts: Vec<String> = Vec::new();
                    for child in name_node.named_children(&mut walker) {
                        if child.kind() == "namespace_identifier"
                            && let Ok(text) = child.utf8_text(source)
                        {
                            nested_parts.push(text.to_string());
                        }
                    }
                    // nested specifier is left-to-right; we'll reverse the whole
                    // chain at the end so push as-is.
                    for part in nested_parts.into_iter().rev() {
                        parts.push(part);
                    }
                }
                _ => {}
            }
        }
        current = parent.parent();
    }
    if parts.is_empty() {
        None
    } else {
        parts.reverse();
        Some(parts.join("::"))
    }
}

/// Build the qualified name for a C++ symbol: `ns1::ns2::Class::member`.
/// Out-of-line method captures (`Foo::bar`) carry their own scope prefix;
/// in-class definitions get their class via `find_parent_class_name`.
fn cpp_qualified_name(
    bare_name: &str,
    captured_name: &str,
    kind: SymbolKind,
    def_node: &Node,
    source: &[u8],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ns) = find_cpp_namespace(def_node, source) {
        parts.push(ns);
    }
    if captured_name.contains("::") {
        // Out-of-line definition: the captured text already contains the class
        // scope (e.g. `Foo::bar`). Use it verbatim instead of walking parents,
        // since these defs live at namespace scope, not inside the class body.
        parts.push(captured_name.to_string());
    } else {
        if (kind == SymbolKind::Method || kind == SymbolKind::Constant)
            && let Some(class_name) = find_parent_class_name(def_node, source)
        {
            parts.push(class_name.to_string());
        }
        parts.push(bare_name.to_string());
    }
    parts.join("::")
}

fn find_parent_class_name<'a>(node: &Node, source: &'a [u8]) -> Option<&'a str> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition"
            | "class_declaration"
            | "trait_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "class_specifier"
            | "struct_specifier"
            | "union_specifier" => {
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
        // Cpp goes through `cpp_qualified_name` in extract_symbols. This arm
        // exists only to keep the match exhaustive; the dispatcher never
        // reaches here for Cpp.
        Language::Cpp => name.to_string(),
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
