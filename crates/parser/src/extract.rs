use std::sync::LazyLock;

use anyhow::Result;
use codesage_protocol::{Language, Symbol, SymbolKind, Visibility};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

static PHP_QUERY: &str = include_str!("queries/php.scm");
static PYTHON_QUERY: &str = include_str!("queries/python.scm");
static C_QUERY: &str = include_str!("queries/c.scm");
static CPP_QUERY: &str = include_str!("queries/cpp.scm");
static JAVA_QUERY: &str = include_str!("queries/java.scm");
static RUST_QUERY: &str = include_str!("queries/rust.scm");
static JS_QUERY: &str = include_str!("queries/javascript.scm");
static TS_QUERY: &str = include_str!("queries/typescript.scm");
static GO_QUERY: &str = include_str!("queries/go.scm");

/// Cache compiled queries and linear capture-name lookups across files.
pub(crate) struct SymbolQuerySpec {
    pub(crate) query: Query,
    pub(crate) name_idx: u32,
    pub(crate) def_idx: u32,
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
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::Php), PHP_QUERY));
static PY_SYM: LazyLock<SymbolQuerySpec> = LazyLock::new(|| {
    compile_symbol_query(crate::parse::ts_language(Language::Python), PYTHON_QUERY)
});
static C_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::C), C_QUERY));
static CPP_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::Cpp), CPP_QUERY));
static JAVA_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::Java), JAVA_QUERY));
static RUST_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::Rust), RUST_QUERY));
static JS_SYM: LazyLock<SymbolQuerySpec> = LazyLock::new(|| {
    compile_symbol_query(crate::parse::ts_language(Language::JavaScript), JS_QUERY)
});
static TS_SYM: LazyLock<SymbolQuerySpec> = LazyLock::new(|| {
    compile_symbol_query(crate::parse::ts_language(Language::TypeScript), TS_QUERY)
});
static GO_SYM: LazyLock<SymbolQuerySpec> =
    LazyLock::new(|| compile_symbol_query(crate::parse::ts_language(Language::Go), GO_QUERY));

pub(crate) fn symbol_query_for(lang: Language) -> &'static SymbolQuerySpec {
    match lang {
        Language::Php => &PHP_SYM,
        Language::Python => &PY_SYM,
        Language::C => &C_SYM,
        Language::Cpp => &CPP_SYM,
        Language::Java => &JAVA_SYM,
        Language::Rust => &RUST_SYM,
        Language::JavaScript => &JS_SYM,
        Language::TypeScript => &TS_SYM,
        Language::Go => &GO_SYM,
    }
}

/// Shared pattern-kind mapping for symbol extraction and fingerprinting.
pub(crate) fn kind_map_for(language: Language) -> fn(usize) -> Option<SymbolKind> {
    match language {
        Language::Php => php_kind_map,
        Language::Python => python_kind_map,
        Language::C => c_kind_map,
        Language::Cpp => cpp_kind_map,
        Language::Java => java_kind_map,
        Language::Rust => rust_kind_map,
        Language::JavaScript => js_kind_map,
        Language::TypeScript => ts_kind_map,
        Language::Go => go_kind_map,
    }
}

/// Query sources compiled by [`crate::validate`] to catch grammar incompatibility.
pub(crate) const SYMBOL_QUERY_SOURCES: &[(Language, &str)] = &[
    (Language::Php, PHP_QUERY),
    (Language::Python, PYTHON_QUERY),
    (Language::C, C_QUERY),
    (Language::Cpp, CPP_QUERY),
    (Language::Java, JAVA_QUERY),
    (Language::Rust, RUST_QUERY),
    (Language::JavaScript, JS_QUERY),
    (Language::TypeScript, TS_QUERY),
    (Language::Go, GO_QUERY),
];

fn php_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        2 => Some(SymbolKind::Method),
        3 => Some(SymbolKind::Trait),
        4 => Some(SymbolKind::Interface),
        5 => Some(SymbolKind::Enum),
        6 => Some(SymbolKind::Constant),
        7 => Some(SymbolKind::Constant), // enum case (PHP 8.1)
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
        7 => Some(SymbolKind::Function), // double-pointer return
        8 => Some(SymbolKind::Constant), // file-scope const object
        9 => Some(SymbolKind::Function), // identity-macro wrapped name
        10 => Some(SymbolKind::Struct),  // named union body
        _ => None,
    }
}

fn cpp_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Function),
        1 => Some(SymbolKind::Class),
        2 | 3 => Some(SymbolKind::Struct),
        4 => Some(SymbolKind::Enum),
        5..=7 => Some(SymbolKind::Constant), // typedef, using-alias, concept
        8 => Some(SymbolKind::Macro),
        9 => Some(SymbolKind::Method), // callable fields; const fields are refined
        10 => Some(SymbolKind::Constant),
        _ => None,
    }
}

fn java_kind_map(pattern_index: usize) -> Option<SymbolKind> {
    match pattern_index {
        0 => Some(SymbolKind::Class),
        1 => Some(SymbolKind::Interface),
        2 => Some(SymbolKind::Enum),
        3 => Some(SymbolKind::Class), // record
        4 => Some(SymbolKind::Method),
        5 => Some(SymbolKind::Method),    // constructor
        6 => Some(SymbolKind::Constant),  // field
        7 => Some(SymbolKind::Interface), // @interface (annotation type)
        8 => Some(SymbolKind::Constant),  // enum constant
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
        9 => Some(SymbolKind::Method), // trait method signature
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
        7 => Some(SymbolKind::Function),     // generator function
        8 | 9 => Some(SymbolKind::Constant), // top-level / exported var
        10 | 11 => Some(SymbolKind::Constant), // module.exports = / module.exports.X =
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
        5 => Some(SymbolKind::Constant), // package-level var
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
        9 | 10 => Some(SymbolKind::Class),   // abstract class (+ exported)
        11 => Some(SymbolKind::Function),    // generator function
        12 | 13 => Some(SymbolKind::Constant), // top-level / exported var
        14 => Some(SymbolKind::Method),      // abstract method signature
        _ => None,
    }
}

/// Whether the file's own syntax makes every definition in it test code: a
/// JavaScript/TypeScript program whose top level calls `describe` / `it` /
/// `test`, or a Rust file headed by `#![cfg(test)]`. The path heuristic in
/// [`crate::discover::is_test_like_path`] answers the same question from the
/// name alone; the indexer stores `files.is_test` from either verdict.
pub fn file_is_test_by_syntax(tree: &Tree, source: &[u8], language: Language) -> bool {
    crate::test_marks::FileTestContext::scan(&tree.root_node(), source, language).marks_whole_file()
}

pub fn extract_symbols(
    tree: &Tree,
    source: &[u8],
    language: Language,
    file_path: &str,
) -> Result<Vec<Symbol>> {
    codesage_protocol::work::checkpoint()?;
    let kind_map = kind_map_for(language);

    let spec = symbol_query_for(language);
    let query = &spec.query;
    let name_idx = spec.name_idx;
    let def_idx = spec.def_idx;

    let root = tree.root_node();
    let dead = crate::preproc::DeadRegions::scan(root, source, language);
    let mut cursor = QueryCursor::new();
    let control = codesage_protocol::work::current();
    let mut progress = |_: &tree_sitter::QueryCursorState| {
        if control
            .as_ref()
            .is_some_and(|control| control.check().is_err())
        {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source,
        tree_sitter::QueryCursorOptions::new().progress_callback(&mut progress),
    );

    let file_namespace = match language {
        Language::Php => find_php_namespace(&root, source),
        Language::Java => find_java_package(&root, source),
        _ => None,
    };
    let test_ctx = crate::test_marks::FileTestContext::scan(&root, source, language);

    let mut symbols: Vec<Symbol> = Vec::new();
    let mut seen_defs = std::collections::HashSet::new();
    // Error recovery can emit distinct name nodes with the same storage key,
    // e.g. parameter types in `typedef R (CALLCONV *fn)(T, T, T)`.
    let mut seen_rows = std::collections::HashSet::new();
    let mut definition_names = DefinitionNames::new(root, source, language);

    while let Some(m) = matches.next() {
        codesage_protocol::work::checkpoint()?;
        let Some(kind) = kind_map(m.pattern_index) else {
            continue;
        };

        let name_capture = m.captures().iter().find(|c| c.index == name_idx);
        let def_capture = m.captures().iter().find(|c| c.index == def_idx);

        let (Some(name_cap), Some(def_cap)) = (name_capture, def_capture) else {
            continue;
        };

        let def_node = def_cap.node;
        let Some((name_node, kind)) =
            definition_capture(def_node, name_cap.node, kind, language, source)
        else {
            continue;
        };

        // A definition parked in a dead `#if 0` arm exists in no build, and
        // must not claim a `seen_defs` key either; see `crate::preproc`.
        if dead.covers(&def_node) {
            continue;
        }

        // Multi-declarator fields share a def node; keep each distinct name.
        let def_id = (
            def_node.start_byte(),
            def_node.end_byte(),
            name_node.start_byte(),
        );
        if !seen_defs.insert(def_id) {
            continue;
        }
        let Some((captured_name, kind)) = definition_names.name(name_node, kind) else {
            continue;
        };
        if captured_name.is_empty() {
            continue;
        }

        let kind = refine_definition_kind(kind, &def_node, language);

        // C++ captures `Foo::bar` for out-of-line methods; the bare `bar` lives
        // in `name`, the full path in `qualified_name`.
        let name = if language == Language::Cpp {
            cpp_bare_name(&captured_name)
        } else {
            captured_name.clone()
        };
        if name.is_empty() || is_recovered_statement(&def_node, &name, kind, language, source) {
            continue;
        }

        let qualified_name = if language == Language::Cpp {
            cpp_qualified_name(&name, &captured_name, kind, &def_node, source)
        } else {
            let namespace = match language {
                Language::Php => find_php_namespace_for_node(&def_node, &root, source),
                Language::Java => file_namespace.clone(),
                _ => None,
            };
            build_qualified_name(&name, kind, &def_node, source, language, &namespace)
        };

        let (start_row, col_start) = crate::position::node_start_utf8(&def_node, source);
        let (end_row, col_end) = crate::position::node_end_utf8(&def_node, source);

        let mut rationale = match language {
            Language::Rust => crate::rationale::extract_rust_rationale(&def_node, source),
            Language::Python => crate::rationale::extract_python_rationale(&def_node, source),
            Language::C
            | Language::Cpp
            | Language::Go
            | Language::Php
            | Language::Java
            | Language::JavaScript
            | Language::TypeScript => {
                crate::rationale::extract_clike_symbol_rationale(&def_node, &name_node, source)
            }
        };
        if matches!(language, Language::C | Language::Cpp) && def_node.kind() == "preproc_def" {
            rationale.extend(crate::rationale::extract_macro_trailing_rationale(
                &def_node, source, language,
            ));
        }

        let visibility = match language {
            Language::Rust => rust_visibility(&def_node),
            Language::C | Language::Cpp => c_visibility(&def_node, source, language, file_path),
            Language::Php => php_visibility(&def_node, source),
            Language::Java => java_visibility(&def_node),
            _ => None,
        };

        let is_test = crate::test_marks::symbol_is_test(&def_node, source, language, test_ctx);
        let signature = crate::signature::extract(def_node, source, language, &name);

        let symbol = Symbol {
            name,
            qualified_name,
            kind,
            file_path: file_path.to_string(),
            line_start: start_row + 1,
            line_end: end_row + 1,
            col_start,
            col_end,
            rationale,
            visibility,
            signature,
            is_test,
            overloaded: false,
        };
        if seen_rows.insert(symbol_row_key(&symbol)) {
            symbols.push(symbol);
        }
    }

    codesage_protocol::work::checkpoint()?;
    Ok(symbols)
}

/// A function definition tree-sitter error recovery made out of a statement,
/// typically an `else if (...) {...}` or `catch (...) {...}` arm left alone
/// inside a preprocessor group. Dropping it leaves its span, and the calls in
/// its body, to the real definition that encloses it.
///
/// - A C/C++ statement keyword can never name a function.
/// - A definition whose return type is `else` is a dangling `else` arm,
///   whatever the following statement or macro is.
/// - C++ has no nested functions, so a typeless definition directly inside a
///   function or lambda body is a statement such as `catch (...) {...}` or
///   `Q_FOREACH(x, list) {...}`; C keeps GNU nested functions.
pub(crate) fn is_recovered_statement(
    def: &Node,
    name: &str,
    kind: SymbolKind,
    language: Language,
    source: &[u8],
) -> bool {
    if !matches!(language, Language::C | Language::Cpp)
        || !matches!(kind, SymbolKind::Function | SymbolKind::Method)
    {
        return false;
    }
    if matches!(
        name,
        "if" | "else" | "while" | "for" | "switch" | "do" | "return" | "case" | "sizeof" | "goto"
    ) {
        return true;
    }
    if def.kind() != "function_definition" {
        return false;
    }
    if def
        .child_by_field_name("type")
        .is_some_and(|ty| ty.utf8_text(source) == Ok("else"))
    {
        return true;
    }
    language == Language::Cpp && nested_in_function_body(def, name, source)
}

/// Only a typeless, non-template definition directly owned by a real function
/// or lambda body counts: a macro-opened namespace or macro-annotated class
/// head (`NLOHMANN_JSON_NAMESPACE_BEGIN namespace detail {`,
/// `class GTEST_API_ Impl {`) also parses as a `function_definition`, but its
/// declarator is not a function declarator, and the real definitions inside it
/// carry a return type.
///
/// A constructor or destructor is typeless too, so one that error recovery
/// swallowed into a function body still counts as real: its name starts with
/// `~`, is qualified, or names an enclosing class.
fn nested_in_function_body(def: &Node, name: &str, source: &[u8]) -> bool {
    if def.child_by_field_name("type").is_some() || name.starts_with('~') {
        return false;
    }
    let own_name = unwrap_declarator(*def)
        .filter(|node| node.kind() == "function_declarator")
        .and_then(|node| node.child_by_field_name("declarator"));
    if own_name
        .is_some_and(|node| matches!(node.kind(), "qualified_identifier" | "destructor_name"))
    {
        return false;
    }
    let mut node = *def;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "template_declaration" if node == *def => return false,
            "compound_statement" => match parent.parent() {
                Some(owner)
                    if owner.kind() == "lambda_expression"
                        || (owner.kind() == "function_definition" && declares_function(owner)) =>
                {
                    return !names_enclosing_class(owner, name, source);
                }
                Some(owner) if owner.kind() == "function_definition" => return false,
                _ => node = parent,
            },
            "field_declaration_list" | "declaration_list" | "translation_unit" => return false,
            _ => node = parent,
        }
    }
    false
}

fn names_enclosing_class(mut node: Node, name: &str, source: &[u8]) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "class_specifier" | "struct_specifier")
            && let Some(class) = parent.child_by_field_name("name")
        {
            let class = match class.kind() {
                "template_type" => class.child_by_field_name("name").unwrap_or(class),
                _ => class,
            };
            if class.utf8_text(source) == Ok(name) {
                return true;
            }
        }
        node = parent;
    }
    false
}

fn declares_function(definition: Node) -> bool {
    unwrap_declarator(definition).is_some_and(|node| match node.kind() {
        "function_declarator" | "operator_cast" => true,
        "qualified_identifier" => node
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "operator_cast"),
        _ => false,
    })
}

/// The definition's declarator with pointer, reference, parenthesized, and
/// attribute wrappers removed.
fn unwrap_declarator(definition: Node) -> Option<Node> {
    let mut declarator = definition.child_by_field_name("declarator");
    while let Some(node) = declarator {
        declarator = match node.kind() {
            "pointer_declarator" => node.child_by_field_name("declarator"),
            "attributed_declarator" => node.named_child(0),
            "reference_declarator" | "parenthesized_declarator" => node
                .named_children(&mut node.walk())
                .find(|child| child.kind().ends_with("declarator")),
            _ => return Some(node),
        };
    }
    None
}

pub(crate) fn definition_capture<'tree>(
    definition: Node<'tree>,
    name: Node<'tree>,
    kind: SymbolKind,
    language: Language,
    source: &[u8],
) -> Option<(Node<'tree>, SymbolKind)> {
    if language != Language::Cpp {
        return Some((name, kind));
    }
    let has_const_qualifier = || {
        definition
            .named_children(&mut definition.walk())
            .any(|node| {
                node.kind() == "type_qualifier"
                    && node.end_byte() <= name.start_byte()
                    && matches!(
                        node.utf8_text(source),
                        Ok("const" | "constexpr" | "constinit")
                    )
            })
    };
    match definition.kind() {
        "function_definition" => cpp_callable_capture(name, true),
        "field_declaration" if name.kind() == "field_identifier" => {
            let has_later_initializer = definition
                .children_by_field_name("default_value", &mut definition.walk())
                .any(|value| value.start_byte() >= name.end_byte());
            (has_later_initializer && has_const_qualifier()).then_some((name, SymbolKind::Constant))
        }
        "field_declaration" => cpp_callable_capture(name, false),
        "class_specifier" | "struct_specifier" | "union_specifier" => definition
            .child_by_field_name("body")
            .filter(|body| {
                body.kind() == "field_declaration_list" && body.start_byte() >= name.end_byte()
            })
            .map(|_| (name, kind)),
        "enum_specifier" => definition
            .child_by_field_name("body")
            .filter(|body| body.kind() == "enumerator_list" && body.start_byte() >= name.end_byte())
            .map(|_| (name, kind)),
        "declaration" => (definition.parent().is_some_and(|parent| {
            matches!(parent.kind(), "translation_unit" | "declaration_list")
        }) && has_const_qualifier())
        .then_some((name, kind)),
        _ => Some((name, kind)),
    }
}

fn cpp_callable_capture(declarator: Node<'_>, has_body: bool) -> Option<(Node<'_>, SymbolKind)> {
    let function = match declarator.kind() {
        "function_declarator" => declarator,
        "pointer_declarator" => declarator.child_by_field_name("declarator")?,
        "reference_declarator" => declarator
            .named_children(&mut declarator.walk())
            .find(|child| child.kind() == "function_declarator")?,
        _ => return None,
    };
    if function.kind() != "function_declarator" {
        return None;
    }
    let name = function.child_by_field_name("declarator")?;
    let admitted = match (declarator.kind(), has_body) {
        ("function_declarator", true) => matches!(
            name.kind(),
            "identifier"
                | "field_identifier"
                | "destructor_name"
                | "operator_name"
                | "qualified_identifier"
                | "function_declarator"
        ),
        ("pointer_declarator", true) => matches!(name.kind(), "identifier" | "field_identifier"),
        ("reference_declarator", true) => matches!(
            name.kind(),
            "identifier" | "field_identifier" | "operator_name"
        ),
        ("function_declarator" | "reference_declarator", false) => {
            matches!(name.kind(), "field_identifier" | "operator_name")
        }
        ("pointer_declarator", false) => name.kind() == "field_identifier",
        _ => false,
    };
    let kind = if !has_body || name.kind() == "qualified_identifier" {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    admitted.then_some((name, kind))
}

pub(crate) struct DefinitionNames<'tree, 'source> {
    root: Node<'tree>,
    source: &'source [u8],
    language: Language,
    wrappers: Option<std::collections::HashMap<String, Vec<(usize, bool)>>>,
    barriers: Vec<usize>,
}

impl<'tree, 'source> DefinitionNames<'tree, 'source> {
    pub(crate) fn new(root: Node<'tree>, source: &'source [u8], language: Language) -> Self {
        Self {
            root,
            source,
            language,
            wrappers: None,
            barriers: Vec::new(),
        }
    }

    pub(crate) fn name(&mut self, node: Node, kind: SymbolKind) -> Option<(String, SymbolKind)> {
        if !matches!(self.language, Language::C | Language::Cpp)
            || node.kind() != "function_declarator"
        {
            return Some((crate::parse::node_text_lossy(&node, self.source), kind));
        }
        if node.has_error() {
            return None;
        }
        let wrapper = node.child_by_field_name("declarator")?;
        if !matches!(
            wrapper.kind(),
            "identifier" | "field_identifier" | "qualified_identifier"
        ) {
            return None;
        }
        let parameters = node.child_by_field_name("parameters")?;
        if parameters.named_child_count() != 1 {
            return None;
        }
        let parameter = parameters.named_child(0)?;
        if parameter.kind() != "parameter_declaration" || parameter.named_child_count() != 1 {
            return None;
        }
        let name = parameter.child_by_field_name("type")?;
        if name.kind() != "type_identifier" {
            return None;
        }
        let mut scopes = Vec::new();
        let mut wrapper_name = wrapper;
        while wrapper_name.kind() == "qualified_identifier" {
            scopes.push(match wrapper_name.child_by_field_name("scope") {
                Some(scope) => scope.utf8_text(self.source).ok()?,
                None => "",
            });
            wrapper_name = wrapper_name.child_by_field_name("name")?;
        }
        if !matches!(wrapper_name.kind(), "identifier" | "field_identifier") {
            return None;
        }
        let wrapper_name = wrapper_name.utf8_text(self.source).ok()?;
        if self.wrappers.is_none() {
            self.scan_wrappers();
        }
        let states = self.wrappers.as_ref()?.get(wrapper_name)?;
        let state_index = states.partition_point(|(byte, _)| *byte < node.start_byte());
        let &(defined_at, identity) = states.get(state_index.checked_sub(1)?)?;
        let barrier_index = self
            .barriers
            .partition_point(|byte| *byte < node.start_byte());
        if !identity || barrier_index > 0 && self.barriers[barrier_index - 1] > defined_at {
            return None;
        }
        let name = name.utf8_text(self.source).ok()?;
        if scopes.is_empty() {
            Some((name.to_string(), kind))
        } else {
            scopes.push(name);
            Some((scopes.join("::"), SymbolKind::Method))
        }
    }

    fn scan_wrappers(&mut self) {
        let mut wrappers: std::collections::HashMap<String, Vec<(usize, bool)>> =
            std::collections::HashMap::new();
        let mut cursor = self.root.walk();
        loop {
            let node = cursor.node();
            match node.kind() {
                "preproc_function_def" | "preproc_def" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let identity = node.kind() == "preproc_function_def"
                            && node
                                .parent()
                                .is_some_and(|parent| parent.kind() == "translation_unit")
                            && identity_macro(node, self.source);
                        wrappers
                            .entry(crate::parse::node_text_lossy(&name, self.source))
                            .or_default()
                            .push((node.start_byte(), identity));
                    }
                }
                // Included files and unknown directives can change macro state.
                "preproc_include" | "preproc_call" => self.barriers.push(node.start_byte()),
                _ => {}
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    self.wrappers = Some(wrappers);
                    return;
                }
            }
        }
    }
}

fn identity_macro(node: Node, source: &[u8]) -> bool {
    let (Some(parameters), Some(value)) = (
        node.child_by_field_name("parameters"),
        node.child_by_field_name("value"),
    ) else {
        return false;
    };
    if node.has_error() || parameters.child_count() != 3 || parameters.named_child_count() != 1 {
        return false;
    }
    let Some(parameter) = parameters.named_child(0) else {
        return false;
    };
    parameter.kind() == "identifier"
        && parameter
            .utf8_text(source)
            .ok()
            .zip(value.utf8_text(source).ok())
            .is_some_and(|(parameter, value)| parameter == value.trim())
}

/// The columns of the `symbols` UNIQUE index, minus `file_id` (one file per
/// call). Two emitted symbols with equal keys would be one stored row.
fn symbol_row_key(s: &Symbol) -> (String, String, SymbolKind, u32, u32, u32, u32) {
    (
        s.name.clone(),
        s.qualified_name.clone(),
        s.kind,
        s.line_start,
        s.line_end,
        s.col_start,
        s.col_end,
    )
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

/// Resolve the PHP namespace for a symbol: innermost braced `namespace { }`
/// block first, otherwise the most recent file-level `namespace Name;`
/// declaration before the symbol.
fn find_php_namespace_for_node(node: &Node, root: &Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "namespace_definition" {
            // An unnamed enclosing namespace is explicitly global; do not fall back.
            return parent
                .child_by_field_name("name")
                .and_then(|name_node| name_node.utf8_text(source).ok())
                .map(str::to_string);
        }
        current = parent.parent();
    }
    find_php_namespace_before_offset(root, source, node.start_byte())
}

fn find_php_namespace_before_offset(root: &Node, source: &[u8], offset: usize) -> Option<String> {
    let mut cursor = root.walk();
    let mut current: Option<String> = None;
    for child in root.children(&mut cursor) {
        if child.start_byte() > offset {
            break;
        }
        if child.kind() == "namespace_definition"
            && let Some(name_node) = child.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source)
        {
            current = Some(name.to_string());
        }
    }
    current
}

fn find_java_package(root: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "package_declaration" {
            continue;
        }
        let mut package_cursor = child.walk();
        for package_child in child.named_children(&mut package_cursor) {
            if matches!(package_child.kind(), "identifier" | "scoped_identifier") {
                return package_child.utf8_text(source).ok().map(str::to_string);
            }
        }
    }
    None
}

/// Refine query kinds using the enclosing declaration, shared by symbols and
/// fingerprints so both expose the same definition identity.
pub(crate) fn refine_definition_kind(
    kind: SymbolKind,
    node: &Node,
    language: Language,
) -> SymbolKind {
    if kind == SymbolKind::Function
        && matches!(language, Language::Python | Language::Rust | Language::Cpp)
        && is_inside_impl_or_class(node, language)
    {
        SymbolKind::Method
    } else if language == Language::Go && kind == SymbolKind::Struct {
        refine_go_type_kind(node)
    } else {
        kind
    }
}

fn is_inside_impl_or_class(node: &Node, language: Language) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            // An intervening function makes this a local function, not a method.
            "function_definition" if language == Language::Python || language == Language::Cpp => {
                return false;
            }
            "function_item" if language == Language::Rust => return false,
            "class_definition" | "class_declaration" if language == Language::Python => {
                return true;
            }
            "impl_item" | "trait_item" if language == Language::Rust => return true,
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

/// Rust reach of an item from its `visibility_modifier`. Items inside a trait
/// body take the trait's modifier; methods of `impl Trait for Type` are
/// Public whatever they are written with, since the trait decides who may
/// call them. `macro_rules!` scoping is textual, so macros stay unknown.
fn rust_visibility(def_node: &Node) -> Option<Visibility> {
    if def_node.kind() == "macro_definition" {
        return None;
    }
    let mut current = def_node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "impl_item" => {
                if parent.child_by_field_name("trait").is_some() {
                    return Some(Visibility::Public);
                }
                break;
            }
            "trait_item" => return Some(rust_modifier_visibility(&parent)),
            // A nested item is scoped by its own modifier, not the outer fn's.
            "function_item" => break,
            _ => {}
        }
        current = parent.parent();
    }
    Some(rust_modifier_visibility(def_node))
}

fn rust_modifier_visibility(item: &Node) -> Visibility {
    let mut cursor = item.walk();
    let Some(modifier) = item
        .children(&mut cursor)
        .find(|c| c.kind() == "visibility_modifier")
    else {
        return Visibility::Module;
    };
    // Comments are extras and can sit anywhere inside the parentheses, so
    // look for the scope keyword rather than the token after `(`.
    let mut inner = modifier.walk();
    let Some(scope) = modifier
        .children(&mut inner)
        .find(|c| matches!(c.kind(), "crate" | "self" | "super" | "in"))
    else {
        return Visibility::Public;
    };
    match scope.kind() {
        "crate" => Visibility::Crate,
        "self" => Visibility::Module,
        // `pub(super)` and `pub(in path)` reach an ancestor module, which the
        // Module gate (defining module's subtree) would under-approximate.
        // Crate is the nearest level that never rejects a legal edge.
        "super" | "in" => Visibility::Crate,
        _ => Visibility::Public,
    }
}

fn php_visibility(node: &Node<'_>, source: &[u8]) -> Option<Visibility> {
    if node.kind() == "function_definition" {
        return Some(Visibility::Public);
    }
    if node.kind() != "method_declaration" {
        return None;
    }
    let modifier = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "visibility_modifier");
    match modifier
        .and_then(|modifier| modifier.utf8_text(source).ok())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("private") => Some(Visibility::Private),
        Some("protected") => Some(Visibility::Protected),
        Some("public") | None => Some(Visibility::Public),
        _ => None,
    }
}

fn java_visibility(node: &Node<'_>) -> Option<Visibility> {
    if !matches!(
        node.kind(),
        "method_declaration" | "constructor_declaration"
    ) {
        return None;
    }
    let modifier = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "modifiers");
    let modifier = modifier?;
    modifier
        .children(&mut modifier.walk())
        .find_map(|child| match child.kind() {
            "public" => Some(Visibility::Public),
            "private" => Some(Visibility::Private),
            "protected" => Some(Visibility::Protected),
            _ => None,
        })
}

/// C/C++ internal linkage: a `static` function or file-scope object is
/// nameable only from its own translation unit. A `static` in a header is
/// meant to be textually included by several units, so it stays unknown.
/// Class-member `static` is storage, not linkage, and is skipped.
fn c_visibility(
    def_node: &Node,
    source: &[u8],
    language: Language,
    file_path: &str,
) -> Option<Visibility> {
    if !matches!(def_node.kind(), "function_definition" | "declaration") {
        return None;
    }
    if is_c_header_path(file_path) {
        return None;
    }
    if language == Language::Cpp && is_inside_impl_or_class(def_node, language) {
        return None;
    }
    let mut cursor = def_node.walk();
    let is_static = def_node.children(&mut cursor).any(|c| {
        c.kind() == "storage_class_specifier" && c.utf8_text(source).ok() == Some("static")
    });
    if is_static {
        Some(Visibility::File)
    } else if language == Language::C && def_node.kind() == "function_definition" {
        let mut parent = def_node.parent();
        while let Some(node) = parent {
            if node.kind() == "function_definition" {
                return None;
            }
            parent = node.parent();
        }
        Some(Visibility::Public)
    } else {
        None
    }
}

fn is_c_header_path(file_path: &str) -> bool {
    let ext = file_path.rsplit_once('.').map_or("", |(_, ext)| ext);
    crate::detect::is_c_header_extension(ext)
}

/// Remove C++ scope prefixes, preserving destructor and operator names.
pub(crate) fn cpp_bare_name(captured: &str) -> String {
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
                    // Reverse each nested specifier before reversing the full chain.
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
        // Out-of-line definitions carry class scope in the name, not the AST ancestry.
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

/// Collect enclosing Java type names from outermost to innermost (e.g.
/// `["Outer", "Inner"]` for a method inside `Outer.Inner`).
fn find_java_enclosing_type_names(node: &Node, source: &[u8]) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name")
                    && let Ok(text) = name_node.utf8_text(source)
                {
                    parts.push(text.to_string());
                }
            }
            _ => {}
        }
        current = parent.parent();
    }
    parts.reverse();
    parts
}

fn java_qualified_name(
    name: &str,
    _kind: SymbolKind,
    def_node: &Node,
    source: &[u8],
    package: &Option<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(pkg) = package {
        parts.push(pkg.clone());
    }
    let enclosing = find_java_enclosing_type_names(def_node, source);
    parts.extend(enclosing);
    parts.push(name.to_string());
    parts.join(".")
}

fn find_parent_class_name<'a>(node: &Node, source: &'a [u8]) -> Option<&'a str> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition"
            | "class_declaration"
            | "abstract_class_declaration"
            | "trait_declaration"
            | "trait_item"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
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

fn join_qualified(
    name: &str,
    kind: SymbolKind,
    def_node: &Node,
    source: &[u8],
    namespace: &Option<String>,
    sep: &str,
) -> String {
    let mut parts = Vec::new();
    if let Some(ns) = namespace {
        parts.push(ns.clone());
    }
    if (kind == SymbolKind::Method || kind == SymbolKind::Constant)
        && let Some(class_name) = find_parent_class_name(def_node, source)
    {
        parts.push(class_name.to_string());
    }
    parts.push(name.to_string());
    parts.join(sep)
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
        Language::Php => join_qualified(name, kind, def_node, source, namespace, "\\"),
        Language::Python => {
            if kind == SymbolKind::Method
                && let Some(class_name) = find_parent_class_name(def_node, source)
            {
                return format!("{class_name}.{name}");
            }
            name.to_string()
        }
        Language::C => name.to_string(),
        // C++ qualification is handled by `cpp_qualified_name` before dispatch.
        Language::Cpp => name.to_string(),
        Language::Java => java_qualified_name(name, kind, def_node, source, namespace),
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
    match def_node.child_by_field_name("type").map(|node| node.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::Constant,
    }
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
                    if matches!(inner.kind(), "type_identifier" | "generic_type") {
                        return inner.utf8_text(source).ok();
                    }
                }
            }
            return type_node.utf8_text(source).ok();
        }
    }
    None
}
