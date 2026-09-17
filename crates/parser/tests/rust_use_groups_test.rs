use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind};

fn import_targets(source: &str) -> Vec<String> {
    let tree = parse_file(source.as_bytes(), Language::Rust).unwrap();
    assert!(!tree.root_node().has_error());
    let references =
        extract_references(&tree, source.as_bytes(), Language::Rust, "inline.rs").unwrap();
    let mut targets: Vec<_> = references
        .into_iter()
        .filter(|reference| reference.kind == ReferenceKind::Import)
        .map(|reference| reference.to_name)
        .collect();
    targets.sort();
    targets
}

#[test]
fn rust_grouped_aliases_preserve_source_paths() {
    let grouped = import_targets("use a::{b as c, d::E as F, G};");
    assert_eq!(grouped, ["a::G", "a::b", "a::d::E"]);
    assert_eq!(
        grouped,
        import_targets("use a::b as c; use a::d::E as F; use a::G;")
    );
}

#[test]
fn rust_nested_grouped_aliases_preserve_all_prefixes() {
    let grouped = import_targets("use crate::a::{b::{C as D, e::F as G}, H as I};");
    assert_eq!(
        grouped,
        ["crate::a::H", "crate::a::b::C", "crate::a::b::e::F"]
    );
    assert_eq!(
        grouped,
        import_targets(
            "use crate::a::b::C as D; use crate::a::b::e::F as G; use crate::a::H as I;"
        )
    );
}

#[test]
fn rust_top_level_braced_aliases_preserve_source_paths() {
    let grouped = import_targets("use {a as b, c::D as E, f::{G as H}, I};");
    assert_eq!(grouped, ["I", "a", "c::D", "f::G"]);
    assert_eq!(
        grouped,
        import_targets("use a as b; use c::D as E; use f::G as H; use I;")
    );
}
