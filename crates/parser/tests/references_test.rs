use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind};

fn references_for(fixture: &str, language: Language) -> Vec<codesage_protocol::Reference> {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, language).unwrap();
    extract_references(&tree, &source, language, fixture).unwrap()
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
