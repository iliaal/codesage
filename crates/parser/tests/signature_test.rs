use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{CallableSignature, Language};

fn signature(source: &str, language: Language, path: &str, name: &str) -> CallableSignature {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let symbols = extract_symbols(&tree, source.as_bytes(), language, path).unwrap();
    symbols
        .into_iter()
        .find(|symbol| symbol.name == name)
        .unwrap()
        .signature
        .unwrap()
}

#[test]
fn callable_signatures_cover_all_supported_languages() {
    for (language, path, source, name, returns, names) in [
        (
            Language::C,
            "api.c",
            "int *allocate(unsigned count, const char *label) { return 0; }",
            "allocate",
            "int *",
            vec!["count", "label"],
        ),
        (
            Language::Cpp,
            "api.cpp",
            "auto collect(int count = 2) -> const char * { return nullptr; }",
            "collect",
            "const char *",
            vec!["count"],
        ),
        (
            Language::Rust,
            "api.rs",
            "pub fn fetch(count: usize) -> Option<String> { None }",
            "fetch",
            "Option<String>",
            vec!["count"],
        ),
        (
            Language::Php,
            "api.php",
            "<?php function fetch(string $key, int $limit = 2): ?array { return null; }",
            "fetch",
            "?array",
            vec!["$key", "$limit"],
        ),
        (
            Language::Python,
            "api.py",
            "def fetch(key: str, limit: int = 2) -> list[str]:\n    return []\n",
            "fetch",
            "list[str]",
            vec!["key", "limit"],
        ),
        (
            Language::Go,
            "api.go",
            "package api\nfunc Fetch(left, right int) (string, error) { return \"\", nil }",
            "Fetch",
            "(string, error)",
            vec!["left", "right"],
        ),
        (
            Language::Java,
            "Api.java",
            "class Api { public String[] fetch(int limit) { return null; } }",
            "fetch",
            "String[]",
            vec!["limit"],
        ),
        (
            Language::TypeScript,
            "api.ts",
            "export function fetch(key: string, limit?: number): string[] { return []; }",
            "fetch",
            "string[]",
            vec!["key", "limit"],
        ),
    ] {
        let signature = signature(source, language, path, name);
        assert_eq!(
            signature.returns.as_deref(),
            Some(returns),
            "{language:?}: {signature:?}"
        );
        assert_eq!(
            signature
                .parameters
                .iter()
                .map(|p| p.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            names,
            "{language:?}: {signature:?}"
        );
        assert!(
            !signature.declaration.contains("return "),
            "{language:?}: {signature:?}"
        );
        let arity = signature.arity.as_ref().unwrap();
        let (minimum, maximum) = match language {
            Language::C | Language::Go => (2, Some(2)),
            Language::Cpp => (0, Some(1)),
            Language::Php => (1, None),
            Language::Python | Language::TypeScript => (1, Some(2)),
            Language::Rust | Language::Java => (1, Some(1)),
            Language::JavaScript => unreachable!(),
        };
        assert_eq!(
            (arity.minimum, arity.maximum),
            (minimum, maximum),
            "{language:?}"
        );
        assert!(
            signature
                .parameters
                .iter()
                .all(|parameter| parameter.type_name.is_some()),
            "{language:?}: {signature:?}"
        );
    }
    let js = signature(
        "function fetch(key, limit = 2) { return key; }",
        Language::JavaScript,
        "api.js",
        "fetch",
    );
    assert_eq!(js.parameters[0].name.as_deref(), Some("key"));
    assert_eq!(js.parameters[1].default_value.as_deref(), Some("2"));
    assert!(js.arity.is_none());
    assert!(js.returns.is_none());
}

#[test]
fn parameter_types_preserve_pointer_and_function_declarators() {
    let sig = signature(
        "int run(const char *key, void (*callback)(int)) { return 0; }",
        Language::C,
        "api.c",
        "run",
    );
    assert_eq!(sig.parameters[0].type_name.as_deref(), Some("const char *"));
    assert_eq!(sig.parameters[1].name.as_deref(), Some("callback"));
    assert_eq!(
        sig.parameters[1].type_name.as_deref(),
        Some("void (*)(int)")
    );
}

#[test]
fn qualified_returns_and_unnamed_parameters_preserve_types() {
    for (language, path) in [(Language::C, "api.c"), (Language::Cpp, "api.cpp")] {
        let sig = signature(
            "const char *lookup(const char *) { return 0; }",
            language,
            path,
            "lookup",
        );
        assert_eq!(sig.returns.as_deref(), Some("const char *"));
        assert_eq!(sig.parameters[0].type_name.as_deref(), Some("const char *"));
        assert!(sig.parameters[0].name.is_none());
    }
}

#[test]
fn typed_python_variadics_keep_names_and_unknown_positional_arity() {
    let sig = signature(
        "def run(*args: int, **kwargs: str) -> None:\n    pass\n",
        Language::Python,
        "api.py",
        "run",
    );
    assert!(sig.arity.is_none());
    assert_eq!(sig.parameters[0].name.as_deref(), Some("args"));
    assert_eq!(sig.parameters[0].type_name.as_deref(), Some("int"));
    assert_eq!(sig.parameters[1].name.as_deref(), Some("kwargs"));
    assert_eq!(sig.parameters[1].type_name.as_deref(), Some("str"));
    assert!(sig.parameters.iter().all(|p| p.variadic));
}

#[test]
fn java_array_and_variadic_parameters_keep_names_and_types() {
    let sig = signature(
        "class Api { public void run(String items[], String... rest) {} }",
        Language::Java,
        "Api.java",
        "run",
    );
    assert_eq!(sig.parameters[0].name.as_deref(), Some("items"));
    assert_eq!(sig.parameters[0].type_name.as_deref(), Some("String[]"));
    assert_eq!(sig.parameters[1].name.as_deref(), Some("rest"));
    assert_eq!(sig.parameters[1].type_name.as_deref(), Some("String"));
    assert!(sig.parameters[1].variadic);
    assert_eq!(sig.arity.unwrap().minimum, 1);
}

#[test]
fn defaults_variadics_and_unknown_counts_remain_distinct() {
    let cpp = signature(
        "int sum(int start = 2, ...) { return start; }",
        Language::Cpp,
        "api.cpp",
        "sum",
    );
    assert_eq!(cpp.arity.unwrap().minimum, 0);
    assert_eq!(cpp.parameters[0].default_value.as_deref(), Some("2"));
    let php = signature(
        "<?php function sum(int $start, int ...$rest): int { return $start; }",
        Language::Php,
        "api.php",
        "sum",
    );
    let arity = php.arity.unwrap();
    assert_eq!(arity.minimum, 1);
    assert_eq!(arity.maximum, None);
    assert!(php.parameters[1].variadic);
    let py = signature(
        "def run(key, /, *, flag=True):\n    pass\n",
        Language::Python,
        "api.py",
        "run",
    );
    assert!(py.arity.is_none());
    let c = signature(
        "int unknown() { return 0; }",
        Language::C,
        "api.c",
        "unknown",
    );
    assert!(c.arity.is_none());
    let c = signature("int none(void) { return 0; }", Language::C, "api.c", "none");
    assert!(c.parameters.is_empty());
    assert_eq!(c.arity.unwrap().maximum, Some(0));
    let rust = signature(
        "struct Api; impl Api { fn run(&self, n: usize) {} }",
        Language::Rust,
        "api.rs",
        "run",
    );
    assert!(rust.arity.is_none());
}

#[test]
fn arrow_signatures_preserve_literal_defaults_without_bodies() {
    let sig = signature(
        "export const fetch = (key: string = 'a  b'): string => key;",
        Language::TypeScript,
        "api.ts",
        "fetch",
    );
    assert!(sig.declaration.contains("'a  b'"));
    assert!(!sig.declaration.ends_with("key;"));
    assert_eq!(sig.returns.as_deref(), Some("string"));
    assert_eq!(sig.parameters[0].default_value.as_deref(), Some("'a  b'"));
    let first = signature(
        "const first = (a) => a, second = (b, c) => b + c;",
        Language::JavaScript,
        "api.js",
        "first",
    );
    let second = signature(
        "const first = (a) => a, second = (b, c) => b + c;",
        Language::JavaScript,
        "api.js",
        "second",
    );
    assert_eq!(first.parameters.len(), 1);
    assert_eq!(second.parameters.len(), 2);
    assert_eq!(second.parameters[0].name.as_deref(), Some("b"));
}

#[test]
fn noncallable_symbols_do_not_invent_signatures() {
    let source = b"pub const VALUE: usize = 4; pub struct Api;";
    let tree = parse_file(source, Language::Rust).unwrap();
    let symbols = extract_symbols(&tree, source, Language::Rust, "api.rs").unwrap();
    assert!(symbols.iter().all(|symbol| symbol.signature.is_none()));
}

#[test]
fn php_method_visibility_includes_implicit_public_and_excludes_restricted_methods() {
    use codesage_protocol::Visibility;
    let source = b"<?php class Api { function visible(): int { return 1; } private function hidden() {} protected function inherited() {} public static function factory(): self { return new self; } }";
    let tree = parse_file(source, Language::Php).unwrap();
    let symbols = extract_symbols(&tree, source, Language::Php, "api.php").unwrap();
    for (name, visibility) in [
        ("visible", Visibility::Public),
        ("factory", Visibility::Public),
        ("hidden", Visibility::Private),
        ("inherited", Visibility::Protected),
    ] {
        let symbol = symbols.iter().find(|symbol| symbol.name == name).unwrap();
        assert_eq!(symbol.visibility, Some(visibility), "{name}");
        assert!(symbol.signature.is_some(), "{name}");
    }
}

#[test]
fn java_annotations_cannot_supply_access_modifiers() {
    use codesage_protocol::Visibility;
    let source = br#"class Api {
        @Label(value = " public ") private int hidden(int x) { return x; }
        @Label(value = " private ") public int visible(int x) { return x; }
    }"#;
    let tree = parse_file(source, Language::Java).unwrap();
    let symbols = extract_symbols(&tree, source, Language::Java, "Api.java").unwrap();
    for (name, visibility) in [
        ("hidden", Visibility::Private),
        ("visible", Visibility::Public),
    ] {
        assert_eq!(
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap()
                .visibility,
            Some(visibility)
        );
    }
}

#[test]
fn bound_receiver_arity_remains_unknown() {
    let python = signature(
        "class Api:\n    def run(self, key: str) -> str:\n        return key\n",
        Language::Python,
        "api.py",
        "run",
    );
    assert!(python.arity.is_none());
    assert_eq!(python.parameters.len(), 2);
    let rust = signature(
        "struct Api; impl Api { pub fn run(self: Box<Self>, x: i32) -> i32 { x } }",
        Language::Rust,
        "api.rs",
        "run",
    );
    assert!(rust.arity.is_none());
    assert_eq!(rust.parameters.len(), 2);
    let free = signature(
        "def run(self, key: str) -> str:\n    return key\n",
        Language::Python,
        "api.py",
        "run",
    );
    assert_eq!(free.arity.unwrap().minimum, 2);
}

#[test]
fn typescript_rest_tuples_and_aliases_do_not_claim_unbounded_arity() {
    for source in [
        "function run(...args: [string, number]): void {}",
        "type Args = [string, number]; function run(...args: Args): void {}",
    ] {
        let sig = signature(source, Language::TypeScript, "api.ts", "run");
        assert!(sig.arity.is_none(), "{sig:?}");
        assert!(sig.parameters[0].variadic);
        assert!(sig.parameters[0].type_name.is_some());
    }
    let array = signature(
        "function run(...args: string[]): void {}",
        Language::TypeScript,
        "api.ts",
        "run",
    );
    assert_eq!(
        array.arity,
        Some(codesage_protocol::Arity {
            minimum: 0,
            maximum: None
        })
    );
}
