use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{Language, SymbolKind};

#[test]
fn go_grouped_types_have_individual_kinds_and_spans() {
    let source = "package main\ntype (\n    Record struct {\n        Value int\n    }\n    Reader interface {\n        Read() int\n    }\n    Alias = Record\n    Count int\n)\n";
    let tree = parse_file(source.as_bytes(), Language::Go).unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Go, "groups.go").unwrap();
    let members: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.kind,
                symbol.line_start,
                symbol.col_start,
                symbol.line_end,
                symbol.col_end,
            )
        })
        .collect();
    assert_eq!(
        members,
        [
            ("Record", SymbolKind::Struct, 3, 4, 5, 5),
            ("Reader", SymbolKind::Interface, 6, 4, 8, 5),
            ("Alias", SymbolKind::Constant, 9, 4, 9, 18),
            ("Count", SymbolKind::Constant, 10, 4, 10, 13),
        ]
    );
}

#[test]
fn go_pointer_receivers_share_value_receiver_qualification() {
    let source = r#"package receiver
type Box[T any] struct { Value T }
func (b Box[T]) ValueGet() T { return b.Value }
func (b *Box[T]) PointerGet() T { return b.Value }
type Pair[K comparable, V any] struct { Key K; Value V }
func (p *Pair[K, V]) PairGet() V { return p.Value }
type Plain struct { Value int }
func (p *Plain) PlainGet() int { return p.Value }
"#;
    let tree = parse_file(source.as_bytes(), Language::Go).unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Go, "receiver.go").unwrap();
    let methods: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| (symbol.name.as_str(), symbol.qualified_name.as_str()))
        .collect();
    assert_eq!(
        methods,
        [
            ("ValueGet", "Box[T].ValueGet"),
            ("PointerGet", "Box[T].PointerGet"),
            ("PairGet", "Pair[K, V].PairGet"),
            ("PlainGet", "Plain.PlainGet"),
        ]
    );
}
