use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{Language, SymbolKind};

#[test]
fn c_and_cpp_keep_named_union_bodies_without_anonymous_or_forward_symbols() {
    let source = "union Record {\n    int tag;\n    double payload;\n};\n\
struct Container {\n    union Nested { int nested_value; } member;\n    union { int anonymous_value; } anonymous;\n};\n\
union Forward;\nunion Record;\n";
    for language in [Language::C, Language::Cpp] {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        assert!(!tree.root_node().has_error());
        let symbols = extract_symbols(&tree, source.as_bytes(), language, "unions.h").unwrap();
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (
                    symbol.name.as_str(),
                    symbol.kind,
                    symbol.line_start,
                    symbol.line_end
                ))
                .collect::<Vec<_>>(),
            [
                ("Record", SymbolKind::Struct, 1, 4),
                ("Container", SymbolKind::Struct, 5, 8),
                ("Nested", SymbolKind::Struct, 6, 6)
            ],
            "{language:?}"
        );
    }
}
