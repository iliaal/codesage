use codesage_parser::{
    extract::extract_symbols, fingerprint::file_fingerprints, parse::parse_file,
    references::extract_references,
};
use codesage_protocol::{Language, SymbolKind};

const DAMAGED_ARM: &str = "void handle(int id)
{
    if (id == 1) {
        a(id);
    }
#if defined(USE_V)
    else if (id == 2) {
        b(id);
    }
#endif
    else {
        c(id);
    }
}

void latency(int v)
{
#ifdef FAST
    if (v > 3)
#else
    if (v > 5)
#endif
    {
        d(v);
    }
}

void after_one(void) { e(); }
void after_two(void) { after_one(); }
";

const STATEMENT_KEYWORDS: [&str; 10] = [
    "if", "else", "while", "for", "switch", "do", "return", "case", "sizeof", "goto",
];

fn callables(source: &str, language: Language, path: &str) -> Vec<(String, u32, u32)> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, path)
        .unwrap()
        .into_iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|s| (s.name, s.line_start, s.line_end))
        .collect()
}

#[test]
fn partial_statement_in_preprocessor_arm_is_not_a_function() {
    for (language, path) in [(Language::C, "damaged.c"), (Language::Cpp, "damaged.cpp")] {
        assert_eq!(
            callables(DAMAGED_ARM, language, path),
            [
                ("handle".to_string(), 1, 14),
                ("latency".to_string(), 16, 26),
                ("after_one".to_string(), 28, 28),
                ("after_two".to_string(), 29, 29),
            ],
            "{language:?}"
        );

        let tree = parse_file(DAMAGED_ARM.as_bytes(), language).unwrap();
        let fingerprinted: Vec<_> = file_fingerprints(&tree, DAMAGED_ARM.as_bytes(), language)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert!(
            !fingerprinted
                .iter()
                .any(|name| STATEMENT_KEYWORDS.contains(&name.as_str())),
            "{language:?}: {fingerprinted:?}"
        );

        let refs = extract_references(&tree, DAMAGED_ARM.as_bytes(), language, path).unwrap();
        assert!(
            refs.iter().any(|r| r.to_name == "b" && r.line == 8),
            "{language:?}: the call inside the arm must survive: {refs:?}"
        );
    }
}

#[test]
fn keyword_prefixed_function_names_are_still_indexed() {
    let source = "int iff(int a, int b)
{
    return a == b;
}
int if_x(void) { return 1; }
void for_each(int *v, int n)
{
    for (int i = 0; i < n; i++) {
        v[i] = 0;
    }
}
void do_work(void) { do { } while (0); }
int returnValue(void)
{
    return 0;
}
";
    for (language, path) in [(Language::C, "names.c"), (Language::Cpp, "names.cpp")] {
        assert_eq!(
            callables(source, language, path),
            [
                ("iff".to_string(), 1, 4),
                ("if_x".to_string(), 5, 5),
                ("for_each".to_string(), 6, 11),
                ("do_work".to_string(), 12, 12),
                ("returnValue".to_string(), 13, 16),
            ],
            "{language:?}"
        );
    }
}

#[test]
fn cpp_operators_and_methods_with_control_flow_bodies_are_unaffected() {
    let source = "struct S {
    bool operator==(const S &o) const { if (o.v) { return true; } return false; }
    void run();
    int v;
};
void S::run()
{
#if defined(FAST)
    while (v) { v--; }
#endif
    for (;;) { break; }
}
S &operator<<(S &s, int x) { switch (x) { default: return s; } }
";
    assert_eq!(
        callables(source, Language::Cpp, "s.cpp"),
        [
            ("operator==".to_string(), 2, 2),
            ("run".to_string(), 3, 3),
            ("run".to_string(), 6, 12),
            ("operator<<".to_string(), 13, 13),
        ]
    );
}

#[test]
fn cpp_catch_arm_in_preprocessor_group_is_not_a_function() {
    let source = "void ec(int id)
{
    try {
        a14(id);
    }
#if defined(E)
    catch (const std::exception &e) {
        b14(e);
    }
#endif
    catch (...) {
        c14();
    }
}
";
    assert_eq!(
        callables(source, Language::Cpp, "ec.cpp"),
        [("ec".to_string(), 1, 14)]
    );
    let tree = parse_file(source.as_bytes(), Language::Cpp).unwrap();
    let fingerprinted: Vec<_> = file_fingerprints(&tree, source.as_bytes(), Language::Cpp)
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(fingerprinted, ["ec"]);
}

#[test]
fn cpp_macro_loop_in_function_body_is_not_a_function() {
    let source = "void qf(QList<int> list)
{
    int t = 0;
    Q_FOREACH(int x, list) {
        use(x);
    }
}
";
    assert_eq!(
        callables(source, Language::Cpp, "qf.cpp"),
        [("qf".to_string(), 1, 7)]
    );
}

#[test]
fn dangling_else_before_macro_loop_is_not_a_function() {
    let source = "void k4(int x)
{
    if (x) {
        a(x);
    }
#if defined(A)
    else ZEND_HASH_FOREACH_VAL(ht, v) {
        b(x);
    } ZEND_HASH_FOREACH_END();
#endif
}
";
    for (language, path) in [(Language::C, "k4.c"), (Language::Cpp, "k4.cpp")] {
        assert_eq!(
            callables(source, language, path),
            [("k4".to_string(), 1, 11)],
            "{language:?}"
        );
    }
}

#[test]
fn real_nested_and_keyword_like_definitions_stay_indexed() {
    let gnu_nested = "void k6(int x)
{
    void inner(int y) { b(y); }
    inner(x);
}
";
    assert_eq!(
        callables(gnu_nested, Language::C, "nested.c"),
        [("k6".to_string(), 1, 5), ("inner".to_string(), 3, 3)]
    );

    let local_class = "void outer(int x)
{
    struct Local {
        void method(int y) { b(y); }
    };
    Local().method(x);
}
";
    assert_eq!(
        callables(local_class, Language::Cpp, "local.cpp"),
        [("outer".to_string(), 1, 7), ("method".to_string(), 4, 4)]
    );

    let c_names = "void catch(int e) { a16(e); }
void throw(int e) { b16(e); }
void caller(void) { catch(1); }
";
    assert_eq!(
        callables(c_names, Language::C, "catch_c.c"),
        [
            ("catch".to_string(), 1, 1),
            ("throw".to_string(), 2, 2),
            ("caller".to_string(), 3, 3),
        ]
    );
}

#[test]
fn cpp_definitions_under_macro_scope_heads_stay_indexed() {
    let macro_namespace = "NLOHMANN_JSON_NAMESPACE_BEGIN
namespace detail
{
template<typename T>
void to_json(T& j, bool b) { construct(j, b); }
int helper(int x) { return hx(x); }
}
NLOHMANN_JSON_NAMESPACE_END
";
    assert_eq!(
        callables(macro_namespace, Language::Cpp, "macrons.cpp"),
        [("to_json".to_string(), 5, 5), ("helper".to_string(), 6, 6)]
    );

    let macro_class = "namespace testing {
class GTEST_API_ UnitTestImpl {
 public:
  explicit UnitTestImpl(UnitTest* parent);
  TestSuite* GetMutableSuiteCase(int i) {
    return lookup(i);
  }
  int count() const { return total(); }
  TestEventListeners* listeners() { return &listeners_; }
};
}
";
    assert_eq!(
        callables(macro_class, Language::Cpp, "impl.h"),
        [
            ("GetMutableSuiteCase".to_string(), 5, 7),
            ("count".to_string(), 8, 8),
            ("listeners".to_string(), 9, 9),
        ]
    );
}

#[test]
fn cpp_statement_in_conversion_operator_body_is_not_a_function() {
    let source = "struct S {
    operator bool() const
    {
        try { a1(); }
#ifdef X
        catch (int e) { b1(e); }
#endif
        catch (...) { c1(); }
        return true;
    }
};
S::operator int() const
{
    Q_FOREACH(int x, list) { d6(x); }
    return 0;
}
";
    assert_eq!(callables(source, Language::Cpp, "owners.cpp"), []);
}

#[test]
fn constructor_swallowed_into_function_body_stays_indexed() {
    let source = "class binary_reader
{
    static void byte_swap(NumberType& number)
    {
#ifdef __cpp_lib_byteswap
        if constexpr (sz == 1)
        {
    }
    /*
    */
    binary_reader(int z, int w) : zz(z) { ctor_init(w); }
    ~binary_reader() { dtor_fini(); }
    }
};
";
    assert_eq!(
        callables(source, Language::Cpp, "binary_reader.hpp"),
        [
            ("byte_swap".to_string(), 3, 13),
            ("binary_reader".to_string(), 11, 11),
            ("~binary_reader".to_string(), 12, 12),
        ]
    );
}
