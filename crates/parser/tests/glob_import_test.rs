use codesage_parser::{parse::parse_file, references::extract_references};
use codesage_protocol::{Language, ReferenceKind};

fn imports(source: &str, language: Language) -> Vec<String> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    assert!(!tree.root_node().has_error());
    extract_references(&tree, source.as_bytes(), language, "inline")
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == ReferenceKind::Import)
        .map(|r| r.to_name)
        .collect()
}

#[test]
fn rust_globs_preserve_module_and_star_in_nested_groups() {
    assert_eq!(
        imports(
            "use super::*; use crate::*; use self::*; use x::*; use a::b::*; use super::{nested::{deep::*, *}, *};",
            Language::Rust,
        ),
        [
            "super::*",
            "crate::*",
            "self::*",
            "x::*",
            "a::b::*",
            "super::nested::deep::*",
            "super::nested::*",
            "super::*"
        ]
    );
}

#[test]
fn python_globs_preserve_absolute_and_relative_modules() {
    assert_eq!(
        imports(
            "from pkg.mod import *\nfrom .mod import *\nfrom .. import *\nfrom . import *\n",
            Language::Python
        ),
        ["pkg.mod.*", ".mod.*", "..*", ".*"]
    );
}

#[test]
fn ordinary_imports_remain_distinct_from_globs() {
    assert_eq!(
        imports("use a::b; use super::item;", Language::Rust),
        ["a::b", "super::item"]
    );
    assert_eq!(
        imports("import pkg.mod\nfrom pkg import item\n", Language::Python),
        ["pkg.mod", "pkg", "pkg.item"]
    );
}
