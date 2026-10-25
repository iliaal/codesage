use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{Language, SymbolKind};

#[test]
fn cpp_callable_shapes_keep_exact_kinds_names_and_spans() {
    let source = br#"namespace demo {
struct Widget {
    Widget() {}
    ~Widget() {}
    int plain() { return 0; }
    int* pointer() { return nullptr; }
    int& reference() { static int value = 0; return value; }
    Widget& operator=(const Widget&) { return *this; }
    int declared();
    int* declared_pointer();
    int& declared_reference();
    bool operator==(const Widget&) const;
    Widget& operator+=(const Widget&);
    int (*callback)(int);
    static const int constant = 3;
    int value;
};
int free_value() { return 1; }
int* free_pointer() { return nullptr; }
int& free_reference() { static int value = 1; return value; }
bool operator!=(const Widget&, const Widget&) { return false; }
Widget& operator+(Widget& left, const Widget&) { return left; }
int Widget::declared() { return 2; }
const int global = 4;
int outer() { const int local = 2; return local; }
}
#define IDENTITY(x) x
int IDENTITY(wrapped)(void) { return 3; }
"#;
    let tree = parse_file(source, Language::Cpp).unwrap();
    let symbols = extract_symbols(&tree, source, Language::Cpp, "shapes.cpp").unwrap();
    let mut actual: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.kind,
                symbol.line_start,
                symbol.line_end,
            )
        })
        .collect();
    actual.sort_by_key(|row| (row.2, row.0));
    use SymbolKind::{Constant, Function, Method, Struct};
    assert_eq!(
        actual,
        [
            ("demo::Widget", Struct, 2, 17),
            ("demo::Widget::Widget", Method, 3, 3),
            ("demo::Widget::~Widget", Method, 4, 4),
            ("demo::Widget::plain", Method, 5, 5),
            ("demo::Widget::pointer", Method, 6, 6),
            ("demo::Widget::reference", Method, 7, 7),
            ("demo::Widget::operator=", Method, 8, 8),
            ("demo::Widget::declared", Method, 9, 9),
            ("demo::Widget::declared_pointer", Method, 10, 10),
            ("demo::Widget::declared_reference", Method, 11, 11),
            ("demo::Widget::operator==", Method, 12, 12),
            ("demo::Widget::operator+=", Method, 13, 13),
            ("demo::Widget::constant", Constant, 15, 15),
            ("demo::free_value", Function, 18, 18),
            ("demo::free_pointer", Function, 19, 19),
            ("demo::free_reference", Function, 20, 20),
            ("demo::operator!=", Function, 21, 21),
            ("demo::operator+", Function, 22, 22),
            ("demo::Widget::declared", Method, 23, 23),
            ("demo::global", Constant, 24, 24),
            ("demo::outer", Function, 25, 25),
            ("wrapped", Function, 28, 28),
        ]
    );
}

#[test]
fn cpp_constant_fields_do_not_acquire_function_fingerprints() {
    let source = br#"struct Widget {
    static constexpr int value = 1+2+3+4+5+6+7+8+9+10+11+12+13+14+15+16+17+18+19+20;
    int run(int count) {
        int sum = 0;
        for (int index = 0; index < count; ++index) {
            sum += index * count;
        }
        return sum + count;
    }
};
"#;
    let tree = parse_file(source, Language::Cpp).unwrap();
    let symbols = extract_symbols(&tree, source, Language::Cpp, "shapes.cpp").unwrap();
    assert!(
        symbols
            .iter()
            .any(|symbol| { symbol.name == "value" && symbol.kind == SymbolKind::Constant })
    );
    let fingerprints =
        codesage_parser::fingerprint::file_fingerprints(&tree, source, Language::Cpp);
    assert_eq!(
        fingerprints
            .iter()
            .map(|row| (row.name.as_str(), row.kind))
            .collect::<Vec<_>>(),
        [("run", SymbolKind::Method)]
    );
}

#[test]
fn cpp_member_constants_preserve_initializer_capture_order() {
    for (source, expected) in [
        (
            "struct A { const int initialized = 1, uninitialized; };",
            vec!["initialized"],
        ),
        (
            "struct A { const int one = 1, two = 2, uninitialized; };",
            vec!["one", "two"],
        ),
        (
            "struct A { const int uninitialized, initialized = 1; };",
            vec!["initialized", "uninitialized"],
        ),
        (
            "struct A { const int before, initialized = 1, after; };",
            vec!["before", "initialized"],
        ),
    ] {
        let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
        let symbols =
            extract_symbols(&tree, source.as_bytes(), Language::Cpp, "shapes.cpp").unwrap();
        let mut constants: Vec<_> = symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Constant)
            .map(|symbol| symbol.name.as_str())
            .collect();
        constants.sort_unstable();
        assert_eq!(constants, expected, "{source}");
    }
}
