use codesage_parser::{
    extract::extract_symbols,
    parse::{parse_file, parse_file_tolerant},
    references::extract_references,
};
use codesage_protocol::{Language, ReferenceKind, SymbolKind, Visibility};

fn definitions(source: &str, language: Language, path: &str) -> Vec<(String, u32, u32)> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, path)
        .unwrap()
        .into_iter()
        .map(|symbol| (symbol.name, symbol.line_start, symbol.line_end))
        .collect()
}

fn calls(source: &str, language: Language, path: &str) -> Vec<(String, u32, u32)> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_references(&tree, source.as_bytes(), language, path)
        .unwrap()
        .into_iter()
        .filter(|reference| reference.kind == ReferenceKind::Call)
        .map(|reference| (reference.to_name, reference.line, reference.col))
        .collect()
}

fn owned(names: &[(&str, u32, u32)]) -> Vec<(String, u32, u32)> {
    names
        .iter()
        .map(|&(name, a, b)| (name.to_string(), a, b))
        .collect()
}

#[test]
fn trailing_annotation_macros_leave_the_declarator_its_name_and_body() {
    let source = "void helper(int a) {}
void locked(int a) ABSL_EXCLUSIVE_LOCKS_REQUIRED(mu) { helper(a); }
class S {
 public:
  void locked2() ABSL_LOCKS_EXCLUDED(mu_) { helper(2); }
  int g_ ABSL_GUARDED_BY(mu_);
  void declared(int a) const ABSL_SHARED_LOCKS_REQUIRED(mu_);
};
void tail() {}
";
    assert_eq!(
        definitions(source, Language::Cpp, "x.cc"),
        owned(&[
            ("helper", 1, 1),
            ("locked", 2, 2),
            ("S", 3, 8),
            ("locked2", 5, 5),
            ("declared", 7, 7),
            ("tail", 9, 9),
        ])
    );
    assert_eq!(
        calls(source, Language::Cpp, "x.cc"),
        owned(&[("helper", 2, 55), ("helper", 5, 44)])
    );
}

#[test]
fn attribute_macro_between_return_type_and_name_is_not_the_declarator() {
    let source = "#define printflike(a, b) __attribute__((format(printf, a, b)))
void helper(void) {}
void printflike(1, 2) log_debug(const char *fmt, ...) { helper(); return; }
void tail(void) { log_debug(\"x\"); }
";
    assert_eq!(
        definitions(source, Language::C, "x.c"),
        owned(&[("helper", 2, 2), ("log_debug", 3, 3), ("tail", 4, 4)])
    );
    assert_eq!(
        calls(source, Language::C, "x.c"),
        owned(&[("helper", 3, 56), ("log_debug", 4, 18)])
    );
}

#[test]
fn semicolonless_macro_runs_become_calls_of_the_macro() {
    let body = "PHP_FUNCTION(timezone_name_from_abbr)
{
\tzend_long isdst = -1;

\tZEND_PARSE_PARAMETERS_START(1, 3)
\t\tZ_PARAM_STR(abbr)
\t\tZ_PARAM_OPTIONAL
\t\tZ_PARAM_LONG(isdst)
\tZEND_PARSE_PARAMETERS_END();

\ttzid = timelib_timezone_id_from_abbr(ZSTR_VAL(abbr), isdst);
}
";
    assert_eq!(
        definitions(body, Language::C, "date.c"),
        owned(&[("timezone_name_from_abbr", 1, 12)])
    );
    assert_eq!(
        calls(body, Language::C, "date.c"),
        owned(&[
            ("ZEND_PARSE_PARAMETERS_START", 5, 1),
            ("Z_PARAM_STR", 6, 2),
            ("Z_PARAM_LONG", 8, 2),
            ("ZEND_PARSE_PARAMETERS_END", 9, 1),
            ("timelib_timezone_id_from_abbr", 11, 8),
            ("ZSTR_VAL", 11, 38),
        ])
    );

    let file_scope = "EXPORT_FUNC(PyA)
EXPORT_FUNC(PyB)
EXPORT_FUNC(PyC)
int real(void) { return helper(); }
";
    for (language, path) in [(Language::C, "b.c"), (Language::Cpp, "b.cc")] {
        assert_eq!(
            definitions(file_scope, language, path),
            owned(&[("real", 4, 4)]),
            "{language:?}"
        );
        assert_eq!(
            calls(file_scope, language, path),
            owned(&[
                ("EXPORT_FUNC", 1, 0),
                ("EXPORT_FUNC", 2, 0),
                ("EXPORT_FUNC", 3, 0),
                ("helper", 4, 24),
            ]),
            "{language:?}"
        );
    }
}

#[test]
fn brace_initialized_invocation_is_not_a_function_body() {
    let source = "REGISTER_FACTORY(FooFactory, Base){ \"name\" };
FALSE_RUNTIME_GUARD(x);
void helper(int a) {}
void user() { helper(2); }
";
    assert_eq!(
        definitions(source, Language::Cpp, "a.cc"),
        owned(&[("helper", 3, 3), ("user", 4, 4)])
    );
    assert_eq!(
        calls(source, Language::Cpp, "a.cc"),
        owned(&[
            ("REGISTER_FACTORY", 1, 0),
            ("FALSE_RUNTIME_GUARD", 2, 0),
            ("helper", 4, 14),
        ])
    );
}

#[test]
fn function_defining_macros_and_all_caps_functions_keep_their_definitions() {
    let cpp = "#define FNAME(name) name
int helper(int x) { return x; }
int FNAME(wrapped)(int x) { return helper(x); }
TEST_F(Suite, Name) {
  helper(1);
}
WASM_EXPORT(add) {
  return helper(2);
}
static inline long GENERATE_SEED(void) {
  return helper(3);
}
";
    assert_eq!(
        definitions(cpp, Language::Cpp, "n.cc"),
        owned(&[
            ("helper", 2, 2),
            ("wrapped", 3, 3),
            ("TEST_F", 4, 6),
            ("WASM_EXPORT", 7, 9),
            ("GENERATE_SEED", 10, 12),
        ])
    );

    let c = "PharFileFunction(phar_is_dir, FS_IS_DIR, orig_is_dir)

PHP_FUNCTION(phar_is_file)
{
\thelper(1);
}

ZEND_API ZEND_INI_MH(OnUpdateBool)
{
\thelper(2);
}

U_CFUNC PHP_FUNCTION(intlcal_to_date_time)
{
\thelper(3);
}

static inline long GENERATE_SEED(void)
{
\treturn helper(4);
}

static int ZEND_NOP_SPEC_HANDLER(ZEND_OPCODE_HANDLER_ARGS)
{
\treturn helper(5);
}
";
    assert_eq!(
        definitions(c, Language::C, "n.c"),
        owned(&[
            ("phar_is_file", 3, 6),
            ("OnUpdateBool", 8, 11),
            ("intlcal_to_date_time", 13, 16),
            ("GENERATE_SEED", 18, 21),
            ("ZEND_NOP_SPEC_HANDLER", 23, 26),
        ])
    );
    let tree = parse_file(c.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, c.as_bytes(), Language::C, "n.c").unwrap();
    let signature = |name: &str| {
        symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap()
            .signature
            .is_some()
    };
    assert!(!signature("OnUpdateBool"));
    assert!(!signature("phar_is_file"));
    assert!(signature("GENERATE_SEED"));
}

#[test]
fn php_two_argument_macro_definitions_own_their_c_bodies() {
    let source = "PHP_METHOD(DateTime, format)
{
  ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(value)
  ZEND_PARSE_PARAMETERS_END();
  return date_format(value);
}
#ifdef ENABLE_EXAMPLE
ZEND_METHOD(Tiny_Class, BARE) /* retained body */ { return method_helper(); }
#endif
ZEND_FRAMELESS_FUNCTION(
  date_parse, /* arity */ 2
) { return frame_helper(); }
void outer(void) {
  ordinary(a, b); { nested(); }
  PHP_METHOD(Local, namesake); { neighbor(); }
}
int PHP_METHOD(int first, int second) { return numbered(first, second); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "date.c").unwrap();
    let found: Vec<_> = symbols
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.qualified_name.as_str(),
                s.kind,
                s.line_start,
                s.line_end,
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("format", "DateTime::format", SymbolKind::Method, 1, 7),
            ("BARE", "Tiny_Class::BARE", SymbolKind::Method, 9, 9),
            (
                "date_parse",
                "zflf_date_parse_2",
                SymbolKind::Function,
                11,
                13
            ),
            ("outer", "outer", SymbolKind::Function, 14, 17),
            ("PHP_METHOD", "PHP_METHOD", SymbolKind::Function, 18, 18),
        ]
    );
    assert!(symbols[..3].iter().all(|symbol| symbol.signature.is_none()));
    assert_eq!(
        calls(source, Language::C, "date.c"),
        owned(&[
            ("ZEND_PARSE_PARAMETERS_START", 3, 2),
            ("Z_PARAM_STR", 4, 4),
            ("ZEND_PARSE_PARAMETERS_END", 5, 2),
            ("date_format", 6, 9),
            ("method_helper", 9, 59),
            ("frame_helper", 13, 11),
            ("ordinary", 15, 2),
            ("nested", 15, 20),
            ("PHP_METHOD", 16, 2),
            ("neighbor", 16, 33),
            ("numbered", 18, 47),
        ])
    );
}

#[test]
fn frameless_definitions_keep_display_names_and_distinct_native_abi_names() {
    let source = "ZEND_FRAMELESS_FUNCTION(leaf, 0) { helper_zero(); }
ZEND_FRAMELESS_FUNCTION(leaf, 1) { helper_one(); }
void caller(void) {
  leaf();
  zflf_leaf_0(0);
  zflf_leaf_1(0, 0);
}
void leaf(void) { ordinary_helper(); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "frameless.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (
                s.name.as_str(),
                s.qualified_name.as_str(),
                s.kind,
                s.line_start
            ))
            .collect::<Vec<_>>(),
        [
            ("leaf", "zflf_leaf_0", SymbolKind::Function, 1),
            ("leaf", "zflf_leaf_1", SymbolKind::Function, 2),
            ("caller", "caller", SymbolKind::Function, 3),
            ("leaf", "leaf", SymbolKind::Function, 8),
        ]
    );
    assert_eq!(
        calls(source, Language::C, "frameless.c"),
        owned(&[
            ("helper_zero", 1, 35),
            ("helper_one", 2, 35),
            ("leaf", 4, 2),
            ("zflf_leaf_0", 5, 2),
            ("zflf_leaf_1", 6, 2),
            ("ordinary_helper", 8, 18),
        ])
    );
}

#[test]
fn prefixed_php_heads_and_keyword_method_names_are_c_only() {
    let c = "static ZEND_METHOD(A, sizeof) { helper(); }
ZEND_COLD ZEND_METHOD(B, BARE) { helper(); }
ZEND_FRAMELESS_FUNCTION(sizeof, 1) { helper(); }
";
    let tree = parse_file(c.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, c.as_bytes(), Language::C, "ffi.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.kind, s.col_start))
            .collect::<Vec<_>>(),
        [
            ("A::sizeof", SymbolKind::Method, 0),
            ("B::BARE", SymbolKind::Method, 0),
            ("zflf_sizeof_1", SymbolKind::Function, 0)
        ]
    );
    assert_eq!(
        definitions(
            "PHP_METHOD(A, Size) { helper(); }",
            Language::Cpp,
            "macro.cc"
        ),
        owned(&[("PHP_METHOD", 1, 1)]),
    );
}

#[test]
fn combining_static_and_zend_cold_keeps_the_definition_file_local() {
    let source = "static ZEND_COLD PHP_METHOD(A, decorated) { first(); }
  ZEND_COLD static ZEND_METHOD(B, reversed) { second(); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "prefixes.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.col_start, s.visibility))
            .collect::<Vec<_>>(),
        [
            ("A::decorated", 0, Some(Visibility::File)),
            ("B::reversed", 2, Some(Visibility::File)),
        ]
    );
    assert_eq!(
        calls(source, Language::C, "prefixes.c"),
        owned(&[("first", 1, 44), ("second", 2, 46)])
    );
}

#[test]
fn known_c_decorators_keep_the_native_php_abi_name_and_span() {
    let source = "PHPAPI PHP_METHOD(A, exported) { exported_helper(); }
  ZEND_NORETURN ZEND_COLD ZEND_FASTCALL PHPAPI ZEND_METHOD(A, decorated) { marked_helper(); }
  ZEND_FASTCALL static PHPAPI ZEND_FRAMELESS_FUNCTION(local, 0) { local_helper(); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "markers.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.col_start, s.visibility))
            .collect::<Vec<_>>(),
        [
            ("A::exported", 0, Some(Visibility::Public)),
            ("A::decorated", 2, Some(Visibility::Public)),
            ("zflf_local_0", 2, Some(Visibility::File)),
        ]
    );
    assert_eq!(
        calls(source, Language::C, "markers.c"),
        owned(&[
            ("exported_helper", 1, 33),
            ("marked_helper", 2, 75),
            ("local_helper", 3, 66),
        ])
    );
}

#[test]
fn unknown_decorators_do_not_turn_invocations_into_php_abi_definitions() {
    let source = "OTHER_ATTRIBUTE PHPAPI PHP_METHOD(A, rejected) { first(); }
void ordinary_owner(void) {
    ordinary(a, b);
    { second(); }
}
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "unknown.c").unwrap();
    assert!(symbols.iter().all(|s| s.qualified_name != "A::rejected"));
    let calls = calls(source, Language::C, "unknown.c");
    assert!(calls.contains(&("ordinary".to_owned(), 3, 4)), "{calls:?}");
    assert!(calls.contains(&("second".to_owned(), 4, 6)), "{calls:?}");
}

#[test]
fn a_complete_conditional_can_choose_either_bare_opening_brace() {
    let source = "PHP_METHOD(A, alternative)
#if FLAG
{
#else
{
#endif
    first();
}
ZEND_FRAMELESS_FUNCTION(frame, 0)
#if 0
{
#else
{
#endif
    second();
}
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "split.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.line_start, s.line_end))
            .collect::<Vec<_>>(),
        [("A::alternative", 1, 8), ("zflf_frame_0", 9, 16)]
    );
    assert_eq!(
        calls(source, Language::C, "split.c"),
        owned(&[("first", 7, 4), ("second", 15, 4)])
    );
}

#[test]
fn conditional_braces_do_not_admit_incomplete_or_nonempty_arms_as_php_heads() {
    for source in [
        "PHP_METHOD(A, rejected)
#if FLAG
{
#endif
    untouched();
}
",
        "PHP_METHOD(A, rejected)
#if FLAG
{
    one_arm_only();
#else
{
#endif
    untouched();
}
",
    ] {
        let tree = parse_file(source.as_bytes(), Language::C).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "gap.c").unwrap();
        assert!(
            symbols.iter().all(|s| s.qualified_name != "A::rejected"),
            "{source}"
        );
    }
}

#[test]
fn a_semicolonless_module_macro_and_zend_try_leave_the_method_body_intact() {
    let c = "#ifdef SHARED
ZEND_GET_MODULE(example)
#endif
PHP_METHOD(Example, attempt)
{
    zend_try {
        try_helper();
    } zend_catch {
        catch_helper();
    } zend_end_try();
    return after_try();
}
void next(void) { after_method(); }
";
    let tree = parse_file(c.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, c.as_bytes(), Language::C, "module.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.line_start, s.line_end))
            .collect::<Vec<_>>(),
        [("Example::attempt", 4, 12), ("next", 13, 13)]
    );
    assert_eq!(
        calls(c, Language::C, "module.c"),
        owned(&[
            ("ZEND_GET_MODULE", 2, 0),
            ("try_helper", 7, 8),
            ("catch_helper", 9, 8),
            ("zend_end_try", 10, 6),
            ("after_try", 11, 11),
            ("after_method", 13, 18),
        ])
    );
}

#[test]
fn zend_hash_fill_body_stays_inside_the_native_php_method() {
    let source = "PHP_METHOD(SplFixedArray, toArray)
{
  int nested(void) { return real_nested(); }
  if (ready()) {
    ZEND_HASH_FILL_PACKED(ht) {
      for (int i = 0; i < count; i++) {
        ZEND_HASH_FILL_ADD(i);
        Z_TRY_ADDREF(i);
      }
    } ZEND_HASH_FILL_END();
  }
  nested();
}
";
    assert_eq!(
        definitions(source, Language::C, "spl_fixedarray.c"),
        owned(&[("toArray", 1, 13), ("nested", 3, 3)])
    );
    let calls = calls(source, Language::C, "spl_fixedarray.c");
    assert!(
        calls.contains(&("ZEND_HASH_FILL_ADD".to_string(), 7, 8)),
        "{calls:?}"
    );
    assert!(
        calls.contains(&("Z_TRY_ADDREF".to_string(), 8, 8)),
        "{calls:?}"
    );
}

#[test]
fn zend_hash_fill_under_a_native_abi_preserves_gnu_nested_functions() {
    let source = "PHP_METHOD(A, method)
{
    int method_nested(void) {
        ZEND_HASH_FILL_PACKED(ht) { method_call(); } ZEND_HASH_FILL_END();
        return 0;
    }
    method_nested();
}
ZEND_FRAMELESS_FUNCTION(frame, 0)
{
    ZEND_HASH_FILL_PACKED(ht) { frame_call(); } ZEND_HASH_FILL_END();
    int frame_nested(void) {
        ZEND_HASH_FILL_PACKED(ht) { nested_call(); } ZEND_HASH_FILL_END();
        return 0;
    }
    frame_nested();
}
";
    assert_eq!(
        definitions(source, Language::C, "nested.c"),
        owned(&[
            ("method", 1, 8),
            ("method_nested", 3, 6),
            ("frame", 9, 17),
            ("frame_nested", 12, 15),
        ])
    );
    let calls = calls(source, Language::C, "nested.c");
    for expected in owned(&[
        ("method_call", 4, 36),
        ("frame_call", 11, 32),
        ("nested_call", 13, 36),
    ]) {
        assert!(calls.contains(&expected), "{calls:?}");
    }

    let source = "typedef int ZEND_HASH_FILL_PACKED;
PHP_METHOD(A, typed)
{
    ZEND_HASH_FILL_PACKED real(void) { return helper(); }
    ZEND_HASH_FILL_END();
    real();
}
";
    let symbols = definitions(source, Language::C, "typed.c");
    assert!(symbols.contains(&("real".to_owned(), 4, 4)), "{symbols:?}");
}

#[test]
fn zend_hash_fill_end_can_start_an_always_live_conditional_arm() {
    let source = "PHP_METHOD(A, guarded)
{
    ZEND_HASH_FILL_PACKED(ht) { inside(); }
#if 1
    ZEND_HASH_FILL_END();
#endif
    after_wrapper();
}
";
    assert_eq!(
        definitions(source, Language::C, "guarded_wrapper.c"),
        owned(&[("guarded", 1, 8)])
    );
    let calls = calls(source, Language::C, "guarded_wrapper.c");
    assert!(calls.contains(&("inside".to_owned(), 3, 32)), "{calls:?}");
    assert!(
        calls.contains(&("after_wrapper".to_owned(), 7, 4)),
        "{calls:?}"
    );
}

#[test]
fn php_macro_heads_ignore_continued_line_comment_contents() {
    let source = r"PHP_METHOD(Commented, // trailing comment \
  also_part_of_the_comment)
  actual) { method_helper(); }
ZEND_FRAMELESS_FUNCTION(frame, // arity comment \
  99)
  0) { frame_helper(); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "commented.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.line_start, s.line_end))
            .collect::<Vec<_>>(),
        [("Commented::actual", 1, 3), ("zflf_frame_0", 4, 6)]
    );
    assert_eq!(
        calls(source, Language::C, "commented.c"),
        owned(&[("method_helper", 3, 12), ("frame_helper", 6, 7)])
    );
}

#[test]
fn php_method_heads_keep_names_across_line_splices_and_directive_gaps() {
    let source = r"PHP_METHOD(A, \
  spliced) { splice_helper(); }
PHP_METHOD(A, guarded)
#if FLAG
#endif
{ guard_helper(); }
";
    let tree = parse_file(source.as_bytes(), Language::C).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::C, "trivia.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.line_start, s.line_end))
            .collect::<Vec<_>>(),
        [("A::spliced", 1, 2), ("A::guarded", 3, 6)]
    );
    assert_eq!(
        calls(source, Language::C, "trivia.c"),
        owned(&[("splice_helper", 2, 13), ("guard_helper", 6, 2)])
    );
}

#[test]
fn php_method_heads_do_not_skip_code_in_conditional_directive_gaps() {
    for source in [
        "PHP_METHOD(A, rejected)
#if FLAG
ordinary_call();
#endif
{ standalone(); }
",
        "PHP_METHOD(A, rejected)
#if FLAG
#include \"generated_body.h\"
#endif
{ standalone(); }
",
    ] {
        let tree = parse_file(source.as_bytes(), Language::C).unwrap();
        let symbols =
            extract_symbols(&tree, source.as_bytes(), Language::C, "conditional.c").unwrap();
        assert!(
            symbols.iter().all(|s| s.qualified_name != "A::rejected"),
            "{source}"
        );
    }
}

#[test]
fn zend_try_can_be_the_unbraced_body_of_an_if_in_a_native_php_method() {
    let source = "PHP_METHOD(A, unbraced)
{
    if (0) zend_try {
        try_helper();
    } zend_catch {
        catch_helper();
    } zend_end_try();
    return after_try();
}
void outside(void) { after_method(); }
";
    let parsed = parse_file_tolerant(source.as_bytes(), Language::C).unwrap();
    assert!(!parsed.degraded, "{}", parsed.tree.root_node().to_sexp());
    let symbols =
        extract_symbols(&parsed.tree, source.as_bytes(), Language::C, "if_body.c").unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.line_start, s.line_end))
            .collect::<Vec<_>>(),
        [("A::unbraced", 1, 9), ("outside", 10, 10)]
    );
    assert_eq!(
        calls(source, Language::C, "if_body.c"),
        owned(&[
            ("try_helper", 4, 8),
            ("catch_helper", 6, 8),
            ("zend_end_try", 7, 6),
            ("after_try", 8, 11),
            ("after_method", 10, 21),
        ])
    );
}

#[test]
fn trailing_annotation_after_function_qualifiers_and_specifiers_is_blanked() {
    let source = "void helper();
class S {
 public:
  int get() const ABSL_SHARED_LOCKS_REQUIRED(mu_) { helper(); return 1; }
  void over() override ABSL_LOCKS_EXCLUDED(mu_) { helper(); }
  void nx() noexcept ABSL_LOCKS_EXCLUDED(mu_) { helper(); }
  void fin() final ABSL_LOCKS_EXCLUDED(mu_) { helper(); }
  void lref() & ABSL_LOCKS_EXCLUDED(mu_) { helper(); }
  void rref() && ABSL_LOCKS_EXCLUDED(mu_) { helper(); }
};
int S2::get2() const ABSL_SHARED_LOCKS_REQUIRED(mu_) { helper(); return 2; }
int S2::get3() const noexcept ABSL_SHARED_LOCKS_REQUIRED(mu_) { helper(); return 3; }
";
    assert_eq!(
        definitions(source, Language::Cpp, "s.cc"),
        owned(&[
            ("S", 2, 10),
            ("get", 4, 4),
            ("over", 5, 5),
            ("nx", 6, 6),
            ("fin", 7, 7),
            ("lref", 8, 8),
            ("rref", 9, 9),
            ("get2", 11, 11),
            ("get3", 12, 12),
        ])
    );
    let lines: Vec<u32> = calls(source, Language::Cpp, "s.cc")
        .into_iter()
        .filter(|(name, _, _)| name == "helper")
        .map(|(_, line, _)| line)
        .collect();
    assert_eq!(lines, [4, 5, 6, 7, 8, 9, 11, 12]);
}

#[test]
fn gnu_nested_functions_with_typeof_return_types_stay_definitions() {
    let source = "int helper(void);
void outer(void) {
  typeof(int) inner(void) { return helper(); }
  __typeof__(int) inner3(void) { return helper(); }
  inner();
}
";
    assert_eq!(
        definitions(source, Language::C, "n.c"),
        owned(&[("outer", 2, 6), ("inner", 3, 3), ("inner3", 4, 4)])
    );
}

#[test]
fn typed_all_caps_c_definitions_keep_their_own_name() {
    let source = "int COUNT(Foo) { return helper(); }
int SQUARE(x) { return x * x; }
U_CFUNC PHP_FUNCTION(named_by_argument) { helper(); }
";
    assert_eq!(
        definitions(source, Language::C, "c23.c"),
        owned(&[
            ("COUNT", 1, 1),
            ("SQUARE", 2, 2),
            ("named_by_argument", 3, 3),
        ])
    );
}

#[test]
fn conditional_arms_that_each_open_a_function_count_one_brace() {
    let source = "#ifdef A
int f(void) {
#else
int f(int x) {
#endif
  Z_PARAM_LONG(x)
  Z_PARAM_LONG(y)
  return helper();
}
EXPORT_FUNC(PyA)
EXPORT_FUNC(PyB)
int real(void) { return helper2(); }
";
    let defined = definitions(source, Language::C, "arms.c");
    assert!(
        defined.contains(&("real".to_string(), 12, 12)),
        "{defined:?}"
    );
    let exported: Vec<u32> = calls(source, Language::C, "arms.c")
        .into_iter()
        .filter(|(name, _, _)| name == "EXPORT_FUNC")
        .map(|(_, line, _)| line)
        .collect();
    assert_eq!(exported, [10, 11]);
}

#[test]
fn conditional_arms_at_different_depths_stop_repairs_in_their_scope() {
    let source = "int helper(void) { return 0; }
#ifndef ZTS
#else
static void g(int a) {
#endif
  if (a) {
    helper();
  } else if (a > 1) ZEND_ASSERT(a);
  helper();
}
";
    let found = calls(source, Language::C, "dead.c");
    assert!(
        found.contains(&("ZEND_ASSERT".to_string(), 8, 20)),
        "{found:?}"
    );
    assert_eq!(
        found
            .iter()
            .filter(|(name, _, _)| name == "helper")
            .map(|(_, line, _)| *line)
            .collect::<Vec<_>>(),
        [7, 9]
    );
}

#[test]
fn macro_run_followed_by_a_lowercase_call_keeps_the_call() {
    let source = "int helper(void) { return 0; }
int f(void)
{
\tFOO_SUB(a)
\tFOO_SUB(b)

\t/* c */
\tzval_ptr_dtor(&x);
\treturn 0;
}
PHP_MINFO_FUNCTION(basic)
{
\tphp_info_print_table_start();
\tBASIC_MINFO_SUBMODULE(dl)
\tBASIC_MINFO_SUBMODULE(mail)
\tphp_info_print_table_end();
\tBASIC_MINFO_SUBMODULE(assert)
}
";
    assert_eq!(
        definitions(source, Language::C, "basic.c"),
        owned(&[("helper", 1, 1), ("f", 2, 10), ("basic", 11, 18)])
    );
    assert_eq!(
        calls(source, Language::C, "basic.c"),
        owned(&[
            ("FOO_SUB", 4, 1),
            ("FOO_SUB", 5, 1),
            ("zval_ptr_dtor", 8, 1),
            ("php_info_print_table_start", 13, 1),
            ("BASIC_MINFO_SUBMODULE", 14, 1),
            ("BASIC_MINFO_SUBMODULE", 15, 1),
            ("php_info_print_table_end", 16, 1),
            ("BASIC_MINFO_SUBMODULE", 17, 1),
        ])
    );
}

#[test]
fn a_dead_if_zero_arm_without_a_live_sibling_leaves_brace_tracking_unchanged() {
    let source = "void before(void) {}
#if 0
void x(void) {
#endif
EXPORT_FUNC(PyA)
int real(void) { return 0; }
";
    assert_eq!(
        definitions(source, Language::C, "a.c"),
        owned(&[("before", 1, 1), ("real", 6, 6)])
    );
    assert!(
        calls(source, Language::C, "a.c")
            .iter()
            .any(|call| call.0 == "EXPORT_FUNC" && call.1 == 5)
    );
}
