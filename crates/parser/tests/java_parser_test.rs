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
fn java_nested_type_qualified_names() {
    let syms = symbols_for("nested_types.java");
    let visit = syms.iter().find(|s| s.name == "visit").unwrap();
    assert_eq!(visit.qualified_name, "com.acme.nested.Outer.Inner.visit");
    let inner = syms
        .iter()
        .find(|s| s.name == "Inner" && s.kind == SymbolKind::Class)
        .unwrap();
    assert_eq!(inner.qualified_name, "com.acme.nested.Outer.Inner");
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
    // 5 prior symbols + 2 enum constants (ACTIVE, DISABLED) now captured (CR-018).
    assert_eq!(syms.len(), 7);
    assert!(has_symbol(&syms, "Status", SymbolKind::Enum));
    assert!(has_symbol(&syms, "ACTIVE", SymbolKind::Constant));
    assert!(has_symbol(&syms, "DISABLED", SymbolKind::Constant));
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
fn java_annotations_and_annotation_type_and_multi_declarator_fields() {
    // Three gaps the first slice missed:
    //   1. Annotation usages (`@Override`, `@Test`, `@Component(...)`) carry
    //      type references that `find_references` should surface — Spring /
    //      JUnit / JPA routing depends on this.
    //   2. `@interface` declarations (`annotation_type_declaration`) should
    //      produce an Interface symbol, same as a plain `interface`.
    //   3. Multi-declarator fields (`String x, y, z;`) should emit one symbol
    //      per declarator, not just the first.
    let syms = symbols_for("annotations_and_fields.java");

    // @interface MyMarker -> Interface
    assert!(has_symbol(&syms, "MyMarker", SymbolKind::Interface));
    // Annotation-type elements (`String value() default "x";`) are a distinct
    // grammar node (`annotation_type_element_declaration`) and intentionally
    // not captured — they're rarely the target of a cross-file lookup. Follow-
    // on PRs can add the pattern if real usage shows a need.
    // The class itself
    assert!(has_symbol(&syms, "AnnotatedService", SymbolKind::Class));
    // All three declarators of `String firstName, lastName, email;`
    assert!(has_symbol(&syms, "firstName", SymbolKind::Constant));
    assert!(has_symbol(&syms, "lastName", SymbolKind::Constant));
    assert!(has_symbol(&syms, "email", SymbolKind::Constant));
    // Method on the class
    assert!(has_symbol(&syms, "run", SymbolKind::Method));

    let refs = references_for("annotations_and_fields.java");

    // Annotation usages — both `marker_annotation` (no args) and `annotation`
    // (with args) shapes should surface as Call-kind references.
    assert!(has_ref(&refs, "FunctionalInterface", ReferenceKind::Call));
    assert!(has_ref(&refs, "Component", ReferenceKind::Call));
    assert!(has_ref(&refs, "Deprecated", ReferenceKind::Call));
    assert!(has_ref(&refs, "Override", ReferenceKind::Call));
    assert!(has_ref(&refs, "Test", ReferenceKind::Call));
}

#[test]
fn java_nested_types_are_captured() {
    let syms = symbols_for("nested_types.java");
    // 11 prior symbols + 2 Mode enum constants (FAST, SLOW) now captured (CR-018).
    assert_eq!(syms.len(), 13);
    assert!(has_symbol(&syms, "Outer", SymbolKind::Class));
    assert!(has_symbol(&syms, "helper", SymbolKind::Constant));
    assert!(has_symbol(&syms, "Outer", SymbolKind::Method));
    assert!(has_symbol(&syms, "run", SymbolKind::Method));
    assert!(has_symbol(&syms, "Inner", SymbolKind::Class));
    assert!(has_symbol(&syms, "visit", SymbolKind::Method));
    assert!(has_symbol(&syms, "Marker", SymbolKind::Interface));
    assert!(has_symbol(&syms, "mark", SymbolKind::Method));
    assert!(has_symbol(&syms, "Mode", SymbolKind::Enum));
    assert!(has_symbol(&syms, "FAST", SymbolKind::Constant));
    assert!(has_symbol(&syms, "SLOW", SymbolKind::Constant));
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
