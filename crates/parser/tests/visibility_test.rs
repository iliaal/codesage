use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_protocol::{Language, Symbol, Visibility};

fn symbols(source: &str, language: Language, path: &str) -> Vec<Symbol> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, path).unwrap()
}

fn visibility_of(symbols: &[Symbol], name: &str) -> Option<Visibility> {
    let matches: Vec<&Symbol> = symbols.iter().filter(|s| s.name == name).collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one symbol named {name}: {matches:?}"
    );
    matches[0].visibility
}

const RUST_SOURCE: &str = r#"
pub fn exported() {}
fn private_fn() {}
pub(crate) fn crate_fn() {}
pub(super) fn super_fn() {}
pub(in crate::a) fn in_fn() {}
pub(in crate) fn in_crate_fn() {}
pub(self) fn self_fn() {}
struct PrivateStruct;
pub struct PublicStruct;
enum PrivateEnum { A }
pub(crate) enum CrateEnum { B }
const PRIVATE_CONST: u8 = 1;
pub const PUBLIC_CONST: u8 = 2;
static PRIVATE_STATIC: u8 = 3;
pub static PUBLIC_STATIC: u8 = 4;
type PrivateAlias = u8;
pub type PublicAlias = u8;
mod private_mod {}
pub mod public_mod {}
pub trait Greets {
    fn greet(&self);
    fn default_greet(&self) {}
}
trait Hidden {
    fn hidden_sig(&self);
}
impl PublicStruct {
    fn inherent_private(&self) {}
    pub fn inherent_public(&self) {}
    pub(crate) fn inherent_crate(&self) {}
}
impl Greets for PublicStruct {
    fn greet(&self) {}
}
impl Hidden for PublicStruct {
    fn hidden_sig(&self) {}
}
macro_rules! make { () => {} }
pub fn outer() {
    fn nested_private() {}
    pub fn nested_pub() {}
}
"#;

#[test]
fn rust_free_items_carry_modifier_visibility() {
    let syms = symbols(RUST_SOURCE, Language::Rust, "src/lib.rs");
    assert_eq!(visibility_of(&syms, "exported"), Some(Visibility::Public));
    assert_eq!(visibility_of(&syms, "private_fn"), Some(Visibility::Module));
    assert_eq!(visibility_of(&syms, "crate_fn"), Some(Visibility::Crate));
    // Ancestor-scoped reach is recorded as Crate: the Module gate covers only
    // the defining module's subtree, which is narrower than `super`.
    assert_eq!(visibility_of(&syms, "super_fn"), Some(Visibility::Crate));
    assert_eq!(visibility_of(&syms, "in_fn"), Some(Visibility::Crate));
    assert_eq!(visibility_of(&syms, "in_crate_fn"), Some(Visibility::Crate));
    assert_eq!(visibility_of(&syms, "self_fn"), Some(Visibility::Module));
    assert_eq!(
        visibility_of(&syms, "PrivateStruct"),
        Some(Visibility::Module)
    );
    assert_eq!(
        visibility_of(&syms, "PublicStruct"),
        Some(Visibility::Public)
    );
    assert_eq!(
        visibility_of(&syms, "PrivateEnum"),
        Some(Visibility::Module)
    );
    assert_eq!(visibility_of(&syms, "CrateEnum"), Some(Visibility::Crate));
    assert_eq!(
        visibility_of(&syms, "PRIVATE_CONST"),
        Some(Visibility::Module)
    );
    assert_eq!(
        visibility_of(&syms, "PUBLIC_CONST"),
        Some(Visibility::Public)
    );
    assert_eq!(
        visibility_of(&syms, "PRIVATE_STATIC"),
        Some(Visibility::Module)
    );
    assert_eq!(
        visibility_of(&syms, "PUBLIC_STATIC"),
        Some(Visibility::Public)
    );
    assert_eq!(
        visibility_of(&syms, "PrivateAlias"),
        Some(Visibility::Module)
    );
    assert_eq!(
        visibility_of(&syms, "PublicAlias"),
        Some(Visibility::Public)
    );
    assert_eq!(
        visibility_of(&syms, "private_mod"),
        Some(Visibility::Module)
    );
    assert_eq!(visibility_of(&syms, "public_mod"), Some(Visibility::Public));
    assert_eq!(visibility_of(&syms, "make"), None);
    assert_eq!(
        visibility_of(&syms, "nested_private"),
        Some(Visibility::Module)
    );
    assert_eq!(visibility_of(&syms, "nested_pub"), Some(Visibility::Public));
}

#[test]
fn rust_methods_follow_impl_and_trait_rules() {
    let syms = symbols(RUST_SOURCE, Language::Rust, "src/lib.rs");
    assert_eq!(
        visibility_of(&syms, "inherent_private"),
        Some(Visibility::Module)
    );
    assert_eq!(
        visibility_of(&syms, "inherent_public"),
        Some(Visibility::Public)
    );
    assert_eq!(
        visibility_of(&syms, "inherent_crate"),
        Some(Visibility::Crate)
    );
    assert_eq!(visibility_of(&syms, "Greets"), Some(Visibility::Public));
    assert_eq!(
        visibility_of(&syms, "default_greet"),
        Some(Visibility::Public)
    );
    assert_eq!(visibility_of(&syms, "Hidden"), Some(Visibility::Module));

    // Trait impl methods are Public regardless of the trait's own modifier:
    // the signature row inside the trait body carries the trait's reach.
    let greet: Vec<&Symbol> = syms.iter().filter(|s| s.name == "greet").collect();
    assert_eq!(greet.len(), 2, "{greet:?}");
    assert!(
        greet
            .iter()
            .all(|s| s.visibility == Some(Visibility::Public))
    );
    let hidden: Vec<&Symbol> = syms.iter().filter(|s| s.name == "hidden_sig").collect();
    assert_eq!(hidden.len(), 2, "{hidden:?}");
    let by_kind = |k| hidden.iter().find(|s| s.kind == k).unwrap().visibility;
    assert_eq!(
        by_kind(codesage_protocol::SymbolKind::Method),
        Some(Visibility::Module)
    );
    let impl_row = hidden
        .iter()
        .find(|s| s.line_start > 30)
        .expect("impl-block method row");
    assert_eq!(impl_row.visibility, Some(Visibility::Public));
}

const C_SOURCE: &str = r#"
#include <stdio.h>
static int file_helper(int x) { return x + 1; }
int external_fn(int x) { return file_helper(x); }
static char *static_ptr_fn(void) { return 0; }
static const int FILE_LIMIT = 5;
const int SHARED_LIMIT = 6;
struct point { int x; };
typedef int handle_t;
#define WIDTH 3
static inline int inline_helper(void) { return 0; }
"#;

#[test]
fn c_static_functions_and_objects_are_file_visible() {
    let syms = symbols(C_SOURCE, Language::C, "src/util.c");
    assert_eq!(visibility_of(&syms, "file_helper"), Some(Visibility::File));
    assert_eq!(
        visibility_of(&syms, "static_ptr_fn"),
        Some(Visibility::File)
    );
    assert_eq!(
        visibility_of(&syms, "inline_helper"),
        Some(Visibility::File)
    );
    assert_eq!(visibility_of(&syms, "FILE_LIMIT"), Some(Visibility::File));
    assert_eq!(visibility_of(&syms, "external_fn"), None);
    assert_eq!(visibility_of(&syms, "SHARED_LIMIT"), None);
    assert_eq!(visibility_of(&syms, "point"), None);
    assert_eq!(visibility_of(&syms, "handle_t"), None);
    assert_eq!(visibility_of(&syms, "WIDTH"), None);
}

#[test]
fn c_static_in_header_stays_unknown() {
    let syms = symbols(C_SOURCE, Language::C, "include/util.h");
    assert_eq!(visibility_of(&syms, "file_helper"), None);
    assert_eq!(visibility_of(&syms, "FILE_LIMIT"), None);
    // Every header extension the language router indexes is exempt.
    for path in ["k.hh", "k.hpp", "k.hxx", "k.h++", "k.cuh", "k.tpp", "k.ipp"] {
        let syms = symbols(C_SOURCE, Language::Cpp, path);
        assert_eq!(visibility_of(&syms, "file_helper"), None, "{path}");
    }
    let syms = symbols(C_SOURCE, Language::Cpp, "k.cu");
    assert_eq!(visibility_of(&syms, "file_helper"), Some(Visibility::File));
}

const CPP_SOURCE: &str = r#"
namespace app {
static int ns_helper() { return 1; }
int ns_public() { return ns_helper(); }
static const int NS_LIMIT = 2;
}
static int free_helper() { return 3; }
class Widget {
public:
    static int make() { return 4; }
    int run();
    static const int CAPACITY = 8;
};
int Widget::run() { return free_helper(); }
"#;

#[test]
fn cpp_static_linkage_excludes_class_members() {
    let syms = symbols(CPP_SOURCE, Language::Cpp, "src/widget.cpp");
    assert_eq!(visibility_of(&syms, "ns_helper"), Some(Visibility::File));
    assert_eq!(visibility_of(&syms, "free_helper"), Some(Visibility::File));
    assert_eq!(visibility_of(&syms, "NS_LIMIT"), Some(Visibility::File));
    assert_eq!(visibility_of(&syms, "ns_public"), None);
    assert_eq!(visibility_of(&syms, "make"), None);
    assert_eq!(visibility_of(&syms, "CAPACITY"), None);
    assert_eq!(visibility_of(&syms, "Widget"), None);
    let run: Vec<&Symbol> = syms.iter().filter(|s| s.name == "run").collect();
    assert!(!run.is_empty());
    assert!(run.iter().all(|s| s.visibility.is_none()), "{run:?}");
}

#[test]
fn other_languages_carry_no_visibility() {
    let py = symbols("def _hidden():\n    pass\n", Language::Python, "a.py");
    assert!(py.iter().all(|s| s.visibility.is_none()));
    let go = symbols("package p\nfunc hidden() {}\n", Language::Go, "a.go");
    assert!(go.iter().all(|s| s.visibility.is_none()));
}
