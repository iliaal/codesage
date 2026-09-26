use codesage_parser::extract::extract_symbols;
use codesage_parser::fingerprint::file_fingerprints;
use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind, Symbol, SymbolKind};

fn symbols(source: &str, language: Language) -> Vec<Symbol> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, "inline.hpp").unwrap()
}

fn find<'a>(syms: &'a [Symbol], qualified: &str, kind: SymbolKind) -> &'a Symbol {
    syms.iter()
        .find(|s| s.qualified_name == qualified && s.kind == kind)
        .unwrap_or_else(|| panic!("missing {kind:?} {qualified}; got {syms:#?}"))
}

type Span = (String, String, u32, u32, u32, u32);

fn spans(syms: &[Symbol]) -> Vec<Span> {
    let mut out: Vec<_> = syms
        .iter()
        .map(|s| {
            (
                s.qualified_name.clone(),
                s.kind.as_str().to_string(),
                s.line_start,
                s.line_end,
                s.col_start,
                s.col_end,
            )
        })
        .collect();
    out.sort();
    out
}

const EXPORTED: &str = r#"#define MYLIB_API __attribute__((visibility("default")))
class MYLIB_API Widget {
public:
    int size() const { return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }
    void resize(int n);
    MYLIB_API static Widget make();
};
struct MYLIB_API Point { int x; };
class Q_DECL_EXPORT Alpha {
public:
    void run();
};
class Beta {
public:
    void go();
};
void Alpha::run() {}
MYLIB_API int free_fn(int a) { return a; }
class MYLIB_DEPRECATED_EXPORT Legacy : public Beta {
    void old();
};
class MYLIB_DEPRECATED("use Widget")
    Gadget {
    void tick();
};
namespace ns {
template <typename T> class CORE_API Box final {
    T get();
};
}
"#;

#[test]
fn cpp_export_macro_classes_structs_functions_and_members_are_indexed() {
    let syms = symbols(EXPORTED, Language::Cpp);

    find(&syms, "Widget", SymbolKind::Class);
    find(&syms, "Widget::size", SymbolKind::Method);
    find(&syms, "Widget::resize", SymbolKind::Method);
    find(&syms, "Widget::make", SymbolKind::Method);
    find(&syms, "Point", SymbolKind::Struct);
    find(&syms, "Alpha", SymbolKind::Class);
    find(&syms, "Alpha::run", SymbolKind::Method);
    find(&syms, "Beta", SymbolKind::Class);
    find(&syms, "Beta::go", SymbolKind::Method);
    find(&syms, "free_fn", SymbolKind::Function);
    find(&syms, "Legacy", SymbolKind::Class);
    find(&syms, "Legacy::old", SymbolKind::Method);
    find(&syms, "Gadget", SymbolKind::Class);
    find(&syms, "Gadget::tick", SymbolKind::Method);
    find(&syms, "ns::Box", SymbolKind::Class);
    find(&syms, "ns::Box::get", SymbolKind::Method);
    assert!(
        !syms.iter().any(|s| s.kind != SymbolKind::Macro
            && (s.name.ends_with("_API") || s.name.ends_with("_EXPORT"))),
        "an export macro was indexed as a symbol name: {syms:#?}"
    );

    let widget = find(&syms, "Widget", SymbolKind::Class);
    assert_eq!(
        (
            widget.line_start,
            widget.line_end,
            widget.col_start,
            widget.col_end
        ),
        (2, 7, 0, 1)
    );
    let gadget = find(&syms, "Gadget", SymbolKind::Class);
    assert_eq!((gadget.line_start, gadget.line_end), (22, 25));
}

/// Class-head macros become spaces; a macro opening a declaration becomes a
/// same-length `[[a ...]]` attribute, so the definition still starts at the
/// macro's own byte.
#[test]
fn cpp_export_macro_spans_equal_a_hand_rewritten_source() {
    let rewritten = EXPORTED
        .replacen("    MYLIB_API static", "    [[a    ]] static", 1)
        .replacen("MYLIB_API int free_fn", "[[a    ]] int free_fn", 1)
        .replacen("class MYLIB_API Widget", "class           Widget", 1)
        .replacen("struct MYLIB_API Point", "struct           Point", 1)
        .replacen("class Q_DECL_EXPORT", "class              ", 1)
        .replacen(
            "class MYLIB_DEPRECATED_EXPORT",
            "class                        ",
            1,
        )
        .replacen(
            "class MYLIB_DEPRECATED(\"use Widget\")",
            &format!("class {}", " ".repeat(30)),
            1,
        )
        .replacen("class CORE_API", "class         ", 1);
    assert_eq!(rewritten.len(), EXPORTED.len());
    assert!(rewritten.starts_with("#define MYLIB_API "));
    assert_eq!(rewritten.matches("_API").count(), 1, "{rewritten}");
    assert!(!rewritten.contains("_EXPORT") && !rewritten.contains("_DEPRECATED"));

    let with_macros = symbols(EXPORTED, Language::Cpp);
    assert_eq!(
        spans(&with_macros),
        spans(&symbols(&rewritten, Language::Cpp))
    );

    let free_fn = find(&with_macros, "free_fn", SymbolKind::Function);
    assert_eq!((free_fn.line_start, free_fn.col_start), (18, 0));
    let make = find(&with_macros, "Widget::make", SymbolKind::Method);
    assert_eq!((make.line_start, make.col_start), (6, 4));
}

#[test]
fn cpp_export_macro_on_its_own_line_keeps_leading_rationale_and_start() {
    let source = "// WHY: exported for plugins.\nMYLIB_API\nint g(int a) { return a; }\n";
    let syms = symbols(source, Language::Cpp);
    let g = find(&syms, "g", SymbolKind::Function);
    assert_eq!((g.line_start, g.line_end, g.col_start), (2, 3, 0));
    assert!(
        g.rationale
            .iter()
            .any(|r| r.text.contains("exported for plugins")),
        "leading rationale lost: {:?}",
        g.rationale
    );
}

/// `edit_check` swaps a definition's byte range for a replacement and
/// reparses; the range must cover the export macro so none is left behind.
#[test]
fn cpp_export_macro_definition_byte_range_covers_the_macro() {
    let source = "extern \"C\" CORE_API int g(int a) { return a; }\n\
template <typename T> ENGINE_API T get() { return T(); }\n";
    let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let mut defs = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_definition" {
            defs.push(node.byte_range());
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    defs.sort_by_key(|r| r.start);
    let texts: Vec<_> = defs.iter().map(|r| &source[r.clone()]).collect();
    assert_eq!(
        texts,
        [
            "CORE_API int g(int a) { return a; }",
            "ENGINE_API T get() { return T(); }"
        ]
    );
    let mut proposed = source.to_string();
    proposed.replace_range(defs[0].clone(), "int g(int a, int b) { return a; }");
    assert!(!proposed.contains("CORE_API"));
    let reparsed = parse_file(proposed.as_bytes(), Language::Cpp).unwrap();
    assert!(!reparsed.root_node().has_error());
}

#[test]
fn cpp_macro_shaped_class_names_and_types_keep_their_base_parse() {
    let syms = symbols("class RENDER_API final { void m(); };\n", Language::Cpp);
    find(&syms, "RENDER_API", SymbolKind::Class);
    find(&syms, "RENDER_API::m", SymbolKind::Method);

    for source in ["MY_TYPE_API const x;\n", "MY_TYPE_API const x = 1;\n"] {
        let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{source:?}: {}",
            tree.root_node().to_sexp()
        );
    }
}

#[test]
fn cpp_export_macro_references_and_fingerprints_share_the_neutralized_tree() {
    let bytes = EXPORTED.as_bytes();
    let tree = parse_file(bytes, Language::Cpp).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "export macros left a degraded parse: {}",
        tree.root_node().to_sexp()
    );

    let refs = extract_references(&tree, bytes, Language::Cpp, "inline.hpp").unwrap();
    let helper_cols: Vec<_> = refs
        .iter()
        .filter(|r| r.to_name == "helper" && r.kind == ReferenceKind::Call)
        .map(|r| (r.line, r.col))
        .collect();
    assert_eq!(helper_cols, [(4, 30), (4, 42), (4, 54), (4, 66), (4, 78)]);

    let fps = file_fingerprints(&tree, bytes, Language::Cpp);
    let size = fps
        .iter()
        .find(|f| f.name == "size")
        .expect("in-class method of an exported class is fingerprinted");
    assert_eq!(
        (size.kind, size.line_start, size.line_end),
        (SymbolKind::Method, 4, 4)
    );
}

#[test]
fn cpp_macro_shaped_identifiers_outside_declaration_heads_are_untouched() {
    let source = r#"#define LIB_EXPORT __declspec(dllexport)
#define PICK_API(x) (x + 1)
struct MY_API { int x; };
struct NET_EXPORT *global_handle;
enum Flags { FOO_API, BAR_EXPORT = 2 };
// class MYLIB_API CommentWidget {};
/* struct MYLIB_API BlockPoint {}; */
const char *text = "class MYLIB_API StringWidget {};";
int run() {
    int value = LIB_EXPORT;
    CONFIG_DEPRECATED("message");
    if (value > FOO_API) { return PICK_API(value); }
    return LIB_EXPORT;
}
"#;
    let syms = symbols(source, Language::Cpp);
    find(&syms, "MY_API", SymbolKind::Struct);
    find(&syms, "Flags", SymbolKind::Enum);
    find(&syms, "LIB_EXPORT", SymbolKind::Macro);
    find(&syms, "run", SymbolKind::Function);
    for absent in ["CommentWidget", "BlockPoint", "StringWidget"] {
        assert!(
            !syms.iter().any(|s| s.name == absent),
            "{absent} came from a comment or string: {syms:#?}"
        );
    }

    let bytes = source.as_bytes();
    let tree = parse_file(bytes, Language::Cpp).unwrap();
    let refs = extract_references(&tree, bytes, Language::Cpp, "inline.cpp").unwrap();
    let calls: Vec<_> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .map(|r| (r.to_name.as_str(), r.line))
        .collect();
    assert!(calls.contains(&("CONFIG_DEPRECATED", 11)), "{calls:?}");
    assert!(calls.contains(&("PICK_API", 12)), "{calls:?}");
}

#[test]
fn c_export_macro_function_definitions_keep_extracting() {
    let source = "ZEND_API int zend_startup(void) { return 0; }\n\
PHPAPI int php_request_startup(void) { return 0; }\n\
MYLIB_API int c_free_fn(int a) { return a; }\n\
ZEND_API zend_result zend_post_startup(int flags) { return 0; }\n";
    let syms = symbols(source, Language::C);
    // Column 0 proves C sources are parsed as written: a blanked macro would
    // move each definition's start to its return type.
    for name in [
        "zend_startup",
        "php_request_startup",
        "c_free_fn",
        "zend_post_startup",
    ] {
        let s = find(&syms, name, SymbolKind::Function);
        assert_eq!(s.col_start, 0, "{name}");
    }
}
