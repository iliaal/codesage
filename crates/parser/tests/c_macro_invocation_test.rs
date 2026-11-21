use codesage_parser::{
    extract::extract_symbols, parse::parse_file, references::extract_references,
};
use codesage_protocol::{Language, ReferenceKind};

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
";
    assert_eq!(
        definitions(c, Language::C, "n.c"),
        owned(&[
            ("phar_is_file", 3, 6),
            ("OnUpdateBool", 8, 11),
            ("intlcal_to_date_time", 13, 16),
            ("GENERATE_SEED", 18, 21),
        ])
    );
}
