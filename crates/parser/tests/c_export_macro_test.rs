use codesage_parser::extract::extract_symbols;
use codesage_parser::fingerprint::file_fingerprints;
use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind, Symbol, SymbolKind, Visibility};

fn symbols(source: &str) -> Vec<Symbol> {
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    extract_symbols(&tree, source.as_bytes(), Language::C, "api.c").unwrap()
}

fn function<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
    symbols
        .iter()
        .find(|s| s.name == name && s.kind == SymbolKind::Function)
        .unwrap_or_else(|| panic!("missing function {name}: {symbols:#?}"))
}

#[test]
fn c_export_macros_preserve_typedef_return_function_names() {
    let source = "typedef int zend_result;\n\
ZEND_API int a1(void) { return 1; }\n\
ZEND_API zend_result a2(void) { return 2; }\n\
zend_result a4(void) { return 4; }\n\
PHPAPI zend_result php_result(void) { return 0; }\n\
UNKNOWN_API zend_result unknown_result(void) { return 0; }\n\
CORE_EXPORT const zend_string *string_result(void) { return 0; }\n";
    let syms = symbols(source);
    let a2 = function(&syms, "a2");
    assert_eq!((a2.line_start, a2.line_end, a2.col_start), (3, 3, 0));
    for name in ["a1", "a4", "php_result", "unknown_result", "string_result"] {
        assert_eq!(function(&syms, name).col_start, 0, "{name}");
    }
    assert_eq!(
        syms.iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .count(),
        6
    );
    assert!(!syms.iter().any(|s| s.name == "void"), "{syms:#?}");
}

#[test]
fn c_zend_modifiers_do_not_hide_typedef_pointer_returns() {
    let source = "ZEND_API zend_string* ZEND_FASTCALL str_weak(zval *arg, uint32_t arg_num) { return 0; }\n\
ZEND_API ZEND_COLD void ZEND_FASTCALL none_error(void) {}\n\
ZEND_NORETURN ZEND_API void ZEND_FASTCALL timeout(void) {}\n";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let syms = symbols(source);
    for (name, line) in [("str_weak", 1), ("none_error", 2), ("timeout", 3)] {
        let s = function(&syms, name);
        assert_eq!((s.line_start, s.col_start), (line, 0));
    }
    assert_eq!(syms.len(), 3);
}

#[test]
fn c_export_macro_multiline_spans_rationale_and_replacement_cover_the_macro() {
    let source = "// WHY: exported for plugins.\r\nPHPAPI\r\nzend_result f(void) { return 0; }\r\n";
    let syms = symbols(source);
    let f = function(&syms, "f");
    assert_eq!((f.line_start, f.line_end, f.col_start), (2, 3, 0));
    assert!(
        f.rationale
            .iter()
            .any(|r| r.text.contains("exported for plugins"))
    );
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let def = tree.root_node().named_child(1).unwrap();
    assert_eq!(def.kind(), "function_definition");
    assert_eq!(
        &source[def.byte_range()],
        "PHPAPI\r\nzend_result f(void) { return 0; }"
    );
    let mut proposed = source.to_string();
    proposed.replace_range(def.byte_range(), "zend_result f(int a) { return a; }");
    assert!(!proposed.contains("PHPAPI"));
    assert!(
        !parse_file(proposed.as_bytes(), Language::C)
            .unwrap()
            .root_node()
            .has_error()
    );
}

#[test]
fn c_export_macro_arguments_keep_newlines_and_unicode_offsets() {
    let source = "// WHY: kept for naïve plugins.\nLIB_DEPRECATED(\"use other\"\n)\nzend_result old(void) { return 0; }\n";
    let syms = symbols(source);
    let old = function(&syms, "old");
    assert_eq!((old.line_start, old.line_end, old.col_start), (2, 4, 0));
    assert!(
        old.rationale
            .iter()
            .any(|r| r.text.contains("naïve plugins"))
    );
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    assert!(!tree.root_node().has_error());
    let def = tree.root_node().named_child(1).unwrap();
    assert_eq!(
        &source[def.byte_range()],
        "LIB_DEPRECATED(\"use other\"\n)\nzend_result old(void) { return 0; }"
    );
}

#[test]
fn c_export_macros_keep_definitions_after_a_multiline_macro_comment_tail() {
    let source = "#define WRAP(type) do { \\\n+    /* must still recover declarations below */ \\\n+    helper(type); \\\n+} while (0)\n\n\
ZEND_NORETURN ZEND_API ZEND_COLD void preserved(int type) { helper(type); }\n\
ZEND_NORETURN ZEND_API ZEND_COLD void also_preserved(int type) { helper(type); }\n";
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .unwrap();
    let original = parser.parse(source, None).unwrap();
    let original_syms =
        extract_symbols(&original, source.as_bytes(), Language::C, "api.c").unwrap();
    let syms = symbols(source);
    for (name, col_start) in [("preserved", 33), ("also_preserved", 0)] {
        let before = function(&original_syms, name);
        let after = function(&syms, name);
        assert_eq!(
            (after.line_start, after.line_end, after.col_end),
            (before.line_start, before.line_end, before.col_end)
        );
        assert_eq!(after.col_start, col_start);
    }
}

#[test]
fn c_export_macros_keep_static_visibility_qualifiers_and_macro_named_declarators() {
    let source = "typedef int zend_result;\n\
static CORE_API const zend_result *local_before(void) { return 0; }\n\
CORE_API static zend_result local_after(void) { return 0; }\n\
extern CORE_API volatile zend_result external(void) { return 0; }\n\
CORE_API zend_result ZEND_FASTCALL(void) { return 0; }\n";
    let syms = symbols(source);
    for name in ["local_before", "local_after"] {
        let s = function(&syms, name);
        assert_eq!(s.visibility, Some(Visibility::File));
        assert_eq!(s.col_start, 0);
    }
    assert_eq!(function(&syms, "external").visibility, None);
    function(&syms, "ZEND_FASTCALL");
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
}

#[test]
fn c_export_macros_share_offsets_and_dead_arms_with_references_and_fingerprints() {
    let body = "{ return helper(1) + helper(2) + helper(3) + helper(4) + helper(5); }";
    let source = format!(
        "#if 0\nZEND_API zend_result dead(void) {body}\n#else\nPHPAPI zend_result live(void) {body}\n#endif\n#if 1\nCORE_API zend_result taken(void) {body}\n#else\nCORE_API zend_result skipped(void) {body}\n#endif\n"
    );
    let syms = symbols(&source);
    let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["live", "taken"]);
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let refs = extract_references(&tree, source.as_bytes(), Language::C, "api.c").unwrap();
    let calls: Vec<_> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .map(|r| (r.to_name.as_str(), r.line, r.col))
        .collect();
    assert_eq!(calls.len(), 10);
    assert!(
        calls
            .iter()
            .all(|(name, line, _)| *name == "helper" && [4, 7].contains(line))
    );
    assert_eq!(calls[0], ("helper", 4, 39));
    let fps = file_fingerprints(&tree, source.as_bytes(), Language::C);
    assert_eq!(
        fps.iter()
            .map(|f| (f.name.as_str(), f.line_start))
            .collect::<Vec<_>>(),
        [("live", 4), ("taken", 7)]
    );
}

#[test]
fn c_macro_shaped_types_names_and_unknown_second_types_keep_the_base_tree() {
    for source in [
        "MY_TYPE_API const *f(void) { return 0; }",
        "MY_TYPE_API f(void) { return 0; }",
        "MY_TYPE_API const x;",
        "MY_TYPE_API volatile y;",
        "struct MY_TYPE_API { int x; };",
        "typedef MY_TYPE_API Result;",
        "int FUNCTION_API(void) { return 0; }",
        "int ZEND_FASTCALL(void) { return 0; }",
        "ZEND_API void UNKNOWN_CALL f(void) {}",
        "ZEND_API UNKNOWN_MODIFIER zend_result f(void) {}",
        "ZEND_COLD f(void) { return 0; }",
        "#define WRAP(x) \\\n+ ZEND_API zend_result x(void) { return 0; }\n",
        "// PHPAPI zend_result hidden(void) {}\n",
        "/* PHPAPI zend_result hidden(void) {} */\n",
        "const char *s = \"PHPAPI zend_result hidden(void) {}\";",
        "int f(void) { return ZEND_API; }",
    ] {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .unwrap();
        let original = parser.parse(source, None).unwrap();
        let parsed = parse_file(source.as_bytes(), Language::C).unwrap();
        assert_eq!(
            parsed.root_node().to_sexp(),
            original.root_node().to_sexp(),
            "{source}"
        );
    }
}
