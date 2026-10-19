use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{Language, SymbolKind};

#[test]
fn c_struct_and_enum_definitions_require_bodies_but_keep_typedef_aliases() {
    let source = "struct Record {\n\
    struct Nested { int count; } nested;\n\
    enum State { Ready, Done } state;\n\
    struct Opaque *opaque;\n\
};\n\
enum Mode { Read, Write };\n\
struct Forward;\n\
enum Future;\n\
struct Record value;\n\
enum Mode mode;\n\
struct Record *consume(struct Record *record, enum Mode mode) { struct Record local; return record; }\n\
typedef struct Record RecordAlias;\n\
typedef enum Mode ModeAlias;\n\
typedef struct Opaque OpaqueAlias;\n\
typedef enum Future FutureAlias;\n\
typedef struct { int value; } AnonymousStructAlias;\n\
typedef enum { AnonymousValue } AnonymousEnumAlias;\n";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "records.h").unwrap();
    let tags: Vec<_> = symbols
        .iter()
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Struct | SymbolKind::Enum))
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.kind,
                symbol.line_start,
                symbol.line_end,
            )
        })
        .collect();
    assert_eq!(
        tags,
        [
            ("Record", SymbolKind::Struct, 1, 5),
            ("Nested", SymbolKind::Struct, 2, 2),
            ("State", SymbolKind::Enum, 3, 3),
            ("Mode", SymbolKind::Enum, 6, 6),
        ]
    );
    let aliases: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Constant)
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(
        aliases,
        [
            "RecordAlias",
            "ModeAlias",
            "OpaqueAlias",
            "FutureAlias",
            "AnonymousStructAlias",
            "AnonymousEnumAlias"
        ]
    );
    let functions: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function)
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(functions, ["consume"]);
}
