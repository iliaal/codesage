use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind};

fn references_for(fixture: &str, language: Language) -> Vec<codesage_protocol::Reference> {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, language).unwrap();
    extract_references(&tree, &source, language, fixture).unwrap()
}

fn refs_from_source(source: &str, language: Language) -> Vec<codesage_protocol::Reference> {
    let bytes = source.as_bytes();
    let tree = parse_file(bytes, language).unwrap();
    extract_references(&tree, bytes, language, "inline").unwrap()
}

fn has_ref(refs: &[codesage_protocol::Reference], name: &str, kind: ReferenceKind) -> bool {
    refs.iter().any(|r| r.to_name == name && r.kind == kind)
}

#[test]
fn rust_grouped_glob_and_renamed_use_emit_prefixed_imports() {
    // CR-011: grouped / glob / `as` use forms previously emitted zero imports.
    let src = "use std::io::{Read, Write};\nuse a::b::*;\nuse x::y as z;\nuse std::{fmt, cmp::Ordering};\n";
    let refs = refs_from_source(src, Language::Rust);
    assert!(has_ref(&refs, "std::io::Read", ReferenceKind::Import));
    assert!(has_ref(&refs, "std::io::Write", ReferenceKind::Import));
    assert!(has_ref(&refs, "a::b", ReferenceKind::Import)); // glob module
    assert!(has_ref(&refs, "x::y", ReferenceKind::Import)); // renamed source
    assert!(has_ref(&refs, "std::fmt", ReferenceKind::Import)); // grouped bare name
    assert!(has_ref(&refs, "std::cmp::Ordering", ReferenceKind::Import)); // grouped scoped name
}

#[test]
fn php_group_use_emits_prefixed_imports() {
    // CR-016: `use App\Models\{User, Post};` clauses nest under namespace_use_group.
    let src = "<?php\nuse App\\Models\\{User, Post};\n";
    let refs = refs_from_source(src, Language::Php);
    assert!(has_ref(&refs, "App\\Models\\User", ReferenceKind::Import));
    assert!(has_ref(&refs, "App\\Models\\Post", ReferenceKind::Import));
}

#[test]
fn python_relative_import_module_edge_is_captured() {
    // CR-017: `from .models import User` / `from . import helpers`.
    let src = "from .models import User\nfrom . import helpers\n";
    let refs = refs_from_source(src, Language::Python);
    assert!(has_ref(&refs, ".models", ReferenceKind::Import));
    assert!(has_ref(&refs, ".", ReferenceKind::Import));
}

#[test]
fn javascript_reexport_inheritance_and_instantiation() {
    // CR-013: re-export source, class heritage, and `new` in plain JS.
    let src = "export { a } from \"./m\";\nclass Foo extends Bar {}\nconst x = new Baz();\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(has_ref(&refs, "./m", ReferenceKind::Import));
    assert!(has_ref(&refs, "Bar", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Baz", ReferenceKind::Instantiation));
}

#[test]
fn typescript_reexport_inheritance_and_instantiation() {
    // CR-013: same three edges under the TSX grammar (extends_clause form).
    let src = "export { a } from \"./m\";\nclass Foo extends Bar {}\nconst x = new Baz();\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(has_ref(&refs, "./m", ReferenceKind::Import));
    assert!(has_ref(&refs, "Bar", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Baz", ReferenceKind::Instantiation));
}

#[test]
fn python_extracts_attribute_method_calls() {
    let refs = references_for("sample.py", Language::Python);

    assert!(
        refs.iter()
            .any(|r| r.to_name == "find" && r.kind == ReferenceKind::Call && r.line == 11)
    );
    assert!(
        refs.iter()
            .any(|r| r.to_name == "delete" && r.kind == ReferenceKind::Call && r.line == 14)
    );
}

#[test]
fn go_keeps_selector_call_references() {
    let refs = references_for("sample.go", Language::Go);

    let println_refs: Vec<_> = refs
        .iter()
        .filter(|r| r.to_name == "fmt.Println" && r.kind == ReferenceKind::Call)
        .collect();
    assert_eq!(println_refs.len(), 2);
    assert!(println_refs.iter().any(|r| r.line == 49));
    assert!(println_refs.iter().any(|r| r.line == 54));
}

#[test]
fn php_extracts_instance_nullsafe_and_static_method_calls() {
    let refs = references_for("sample.php", Language::Php);

    assert!(
        refs.iter()
            .any(|r| r.to_name == "show" && r.kind == ReferenceKind::Call && r.line == 36)
    );
    assert!(
        refs.iter()
            .any(|r| r.to_name == "index" && r.kind == ReferenceKind::Call && r.line == 37)
    );
    assert!(
        refs.iter()
            .any(|r| r.to_name == "show" && r.kind == ReferenceKind::Call && r.line == 38)
    );
}
