use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_protocol::{Language, SymbolKind};

#[test]
fn php_explicit_global_namespace_does_not_inherit_preceding_namespace() {
    let source = br#"<?php
namespace Before {
    class NamedClass {
        public const NAMED_VALUE = 1;
        public function namedMethod() {}
    }
    function namedFunction() {}
    const NAMED_TOP_LEVEL = 2;
}
namespace {
    class GlobalClass {
        public const GLOBAL_VALUE = 3;
        public function globalMethod() {}
    }
    function globalFunction() {}
    const GLOBAL_TOP_LEVEL = 4;
}
namespace After {
    class FinalClass {}
}
"#;
    let tree = parse_file(source, Language::Php).unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_symbols(&tree, source, Language::Php, "global.php").unwrap();
    let mut actual: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.kind,
                symbol.qualified_name.as_str(),
            )
        })
        .collect();
    let mut expected = vec![
        ("NamedClass", SymbolKind::Class, "Before\\NamedClass"),
        (
            "NAMED_VALUE",
            SymbolKind::Constant,
            "Before\\NamedClass\\NAMED_VALUE",
        ),
        (
            "namedMethod",
            SymbolKind::Method,
            "Before\\NamedClass\\namedMethod",
        ),
        (
            "namedFunction",
            SymbolKind::Function,
            "Before\\namedFunction",
        ),
        (
            "NAMED_TOP_LEVEL",
            SymbolKind::Constant,
            "Before\\NAMED_TOP_LEVEL",
        ),
        ("GlobalClass", SymbolKind::Class, "GlobalClass"),
        (
            "GLOBAL_VALUE",
            SymbolKind::Constant,
            "GlobalClass\\GLOBAL_VALUE",
        ),
        (
            "globalMethod",
            SymbolKind::Method,
            "GlobalClass\\globalMethod",
        ),
        ("globalFunction", SymbolKind::Function, "globalFunction"),
        ("GLOBAL_TOP_LEVEL", SymbolKind::Constant, "GLOBAL_TOP_LEVEL"),
        ("FinalClass", SymbolKind::Class, "After\\FinalClass"),
    ];
    actual.sort_unstable_by_key(|entry| entry.0);
    expected.sort_unstable_by_key(|entry| entry.0);
    assert_eq!(actual, expected);
}
