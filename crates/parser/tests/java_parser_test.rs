use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind, SymbolKind};

fn parse_fixture(fixture: &str) -> (Vec<u8>, tree_sitter::Tree) {
    let path = format!(
        "{}/tests/fixtures/java/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, Language::Java).unwrap();
    (source, tree)
}

fn symbols_for(fixture: &str) -> Vec<codesage_protocol::Symbol> {
    let (source, tree) = parse_fixture(fixture);
    extract_symbols(&tree, &source, Language::Java, fixture).unwrap()
}

fn references_for(fixture: &str) -> Vec<codesage_protocol::Reference> {
    let (source, tree) = parse_fixture(fixture);
    extract_references(&tree, &source, Language::Java, fixture).unwrap()
}

fn has_symbol(symbols: &[codesage_protocol::Symbol], name: &str, kind: SymbolKind) -> bool {
    symbols.iter().any(|s| s.name == name && s.kind == kind)
}

fn has_ref(refs: &[codesage_protocol::Reference], to_name: &str, kind: ReferenceKind) -> bool {
    refs.iter().any(|r| r.to_name == to_name && r.kind == kind)
}

#[test]
fn java_simple_class_symbols_and_references() {
    let syms = symbols_for("simple_class.java");
    assert_eq!(syms.len(), 5);
    assert!(has_symbol(&syms, "SimpleClass", SymbolKind::Class));
    assert!(has_symbol(&syms, "names", SymbolKind::Constant));
    assert!(has_symbol(&syms, "SimpleClass", SymbolKind::Method));
    assert!(has_symbol(&syms, "addName", SymbolKind::Method));
    assert!(has_symbol(&syms, "log", SymbolKind::Method));

    let refs = references_for("simple_class.java");
    assert_eq!(refs.len(), 4);
    assert!(has_ref(&refs, "java.util.List", ReferenceKind::Import));
    assert!(has_ref(&refs, "add", ReferenceKind::Call));
    assert!(has_ref(&refs, "log", ReferenceKind::Call));
    assert!(has_ref(&refs, "println", ReferenceKind::Call));
}

#[test]
fn java_interface_and_impl_symbols_and_references() {
    let syms = symbols_for("interface_and_impl.java");
    assert_eq!(syms.len(), 7);
    assert!(has_symbol(&syms, "Processor", SymbolKind::Interface));
    assert!(has_symbol(&syms, "process", SymbolKind::Method));
    assert!(has_symbol(&syms, "StringProcessor", SymbolKind::Class));
    assert!(has_symbol(&syms, "lastValue", SymbolKind::Constant));
    assert!(has_symbol(&syms, "StringProcessor", SymbolKind::Method));
    assert!(has_symbol(&syms, "emit", SymbolKind::Method));

    let refs = references_for("interface_and_impl.java");
    assert_eq!(refs.len(), 9);
    assert!(has_ref(
        &refs,
        "com.acme.support.BaseProcessor",
        ReferenceKind::Import
    ));
    assert!(has_ref(&refs, "java.io.Closeable", ReferenceKind::Import));
    assert!(has_ref(&refs, "BaseProcessor", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Processor", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Closeable", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "AutoCloseable", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "trim", ReferenceKind::Call));
    assert!(has_ref(&refs, "emit", ReferenceKind::Call));
    assert!(has_ref(&refs, "println", ReferenceKind::Call));
}

#[test]
fn java_enum_and_record_symbols_and_references() {
    let syms = symbols_for("enum_and_record.java");
    assert_eq!(syms.len(), 5);
    assert!(has_symbol(&syms, "Status", SymbolKind::Enum));
    assert!(has_symbol(&syms, "active", SymbolKind::Method));
    assert!(has_symbol(&syms, "AuditEvent", SymbolKind::Class));
    assert!(has_symbol(&syms, "label", SymbolKind::Method));
    assert!(has_symbol(&syms, "DomainEvent", SymbolKind::Interface));

    let refs = references_for("enum_and_record.java");
    assert_eq!(refs.len(), 6);
    assert!(has_ref(&refs, "java.time.Instant", ReferenceKind::Import));
    assert!(has_ref(&refs, "DomainEvent", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "trim", ReferenceKind::Call));
    assert!(has_ref(
        &refs,
        "StringBuilder",
        ReferenceKind::Instantiation
    ));
    assert!(has_ref(&refs, "append", ReferenceKind::Call));
    assert!(has_ref(&refs, "toString", ReferenceKind::Call));
}

#[test]
fn java_nested_types_are_captured() {
    let syms = symbols_for("nested_types.java");
    assert_eq!(syms.len(), 11);
    assert!(has_symbol(&syms, "Outer", SymbolKind::Class));
    assert!(has_symbol(&syms, "helper", SymbolKind::Constant));
    assert!(has_symbol(&syms, "Outer", SymbolKind::Method));
    assert!(has_symbol(&syms, "run", SymbolKind::Method));
    assert!(has_symbol(&syms, "Inner", SymbolKind::Class));
    assert!(has_symbol(&syms, "visit", SymbolKind::Method));
    assert!(has_symbol(&syms, "Marker", SymbolKind::Interface));
    assert!(has_symbol(&syms, "mark", SymbolKind::Method));
    assert!(has_symbol(&syms, "Mode", SymbolKind::Enum));
    assert!(has_symbol(&syms, "Helper", SymbolKind::Class));
    assert!(has_symbol(&syms, "work", SymbolKind::Method));

    let refs = references_for("nested_types.java");
    assert_eq!(refs.len(), 4);
    assert!(has_ref(&refs, "Helper", ReferenceKind::Instantiation));
    assert_eq!(
        refs.iter()
            .filter(|r| r.to_name == "work" && r.kind == ReferenceKind::Call)
            .count(),
        2
    );
}
