use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_protocol::{Language, SymbolKind};

fn symbols_for(fixture: &str, language: Language) -> Vec<codesage_protocol::Symbol> {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, language).unwrap();
    extract_symbols(&tree, &source, language, fixture).unwrap()
}

fn has_symbol(symbols: &[codesage_protocol::Symbol], name: &str, kind: SymbolKind) -> bool {
    symbols.iter().any(|s| s.name == name && s.kind == kind)
}

#[test]
fn php_extracts_all_symbol_types() {
    let syms = symbols_for("sample.php", Language::Php);

    assert!(has_symbol(&syms, "helper", SymbolKind::Function));
    assert!(has_symbol(&syms, "UserController", SymbolKind::Class));
    assert!(has_symbol(&syms, "index", SymbolKind::Method));
    assert!(has_symbol(&syms, "show", SymbolKind::Method));
    assert!(has_symbol(&syms, "Loggable", SymbolKind::Interface));
    assert!(has_symbol(&syms, "Cacheable", SymbolKind::Trait));
    assert!(has_symbol(&syms, "cacheKey", SymbolKind::Method));
    assert!(has_symbol(&syms, "Status", SymbolKind::Enum));
    assert!(has_symbol(&syms, "MAX_USERS", SymbolKind::Constant));
}

#[test]
fn php_multi_namespace_blocks_use_enclosing_namespace() {
    let source = br#"<?php
namespace A;
class First {}

namespace B;
class Second {}
"#;
    let tree = parse_file(source, Language::Php).unwrap();
    let syms = extract_symbols(&tree, source, Language::Php, "multi.php").unwrap();
    let second = syms.iter().find(|s| s.name == "Second").unwrap();
    assert_eq!(second.qualified_name, "B\\Second");
    let first = syms.iter().find(|s| s.name == "First").unwrap();
    assert_eq!(first.qualified_name, "A\\First");
}

#[test]
fn php_qualified_names() {
    let syms = symbols_for("sample.php", Language::Php);

    let index_method = syms.iter().find(|s| s.name == "index").unwrap();
    assert_eq!(
        index_method.qualified_name,
        "App\\Http\\Controllers\\UserController\\index"
    );

    let helper = syms.iter().find(|s| s.name == "helper").unwrap();
    assert_eq!(helper.qualified_name, "App\\Http\\Controllers\\helper");

    let class = syms.iter().find(|s| s.name == "UserController").unwrap();
    assert_eq!(
        class.qualified_name,
        "App\\Http\\Controllers\\UserController"
    );
}

#[test]
fn php_line_numbers_are_positive() {
    let syms = symbols_for("sample.php", Language::Php);
    for s in &syms {
        assert!(s.line_start > 0, "symbol {} has line_start 0", s.name);
        assert!(
            s.line_end >= s.line_start,
            "symbol {} has bad line range",
            s.name
        );
    }
}

#[test]
fn python_extracts_functions_and_classes() {
    let syms = symbols_for("sample.py", Language::Python);

    assert!(has_symbol(&syms, "helper", SymbolKind::Function));
    assert!(has_symbol(&syms, "standalone", SymbolKind::Function));
    assert!(has_symbol(&syms, "UserService", SymbolKind::Class));
    assert!(has_symbol(&syms, "__init__", SymbolKind::Method));
    assert!(has_symbol(&syms, "get_user", SymbolKind::Method));
    assert!(has_symbol(&syms, "delete_user", SymbolKind::Method));
}

#[test]
fn python_qualified_names() {
    let syms = symbols_for("sample.py", Language::Python);

    let get_user = syms.iter().find(|s| s.name == "get_user").unwrap();
    assert_eq!(get_user.qualified_name, "UserService.get_user");

    let helper = syms.iter().find(|s| s.name == "helper").unwrap();
    assert_eq!(helper.qualified_name, "helper");
}

#[test]
fn c_extracts_all_symbol_types() {
    let syms = symbols_for("sample.c", Language::C);

    assert!(has_symbol(&syms, "add", SymbolKind::Function));
    assert!(has_symbol(&syms, "config", SymbolKind::Struct));
    assert!(has_symbol(&syms, "log_level", SymbolKind::Enum));
    assert!(has_symbol(&syms, "MAX_BUFFER", SymbolKind::Macro));
    assert!(has_symbol(&syms, "VERSION", SymbolKind::Macro));
    assert!(has_symbol(&syms, "ulong", SymbolKind::Constant)); // typedef
    assert!(has_symbol(&syms, "parse_url", SymbolKind::Function)); // macro-wrapped
}

#[test]
fn c_pointer_return_function() {
    let syms = symbols_for("sample.c", Language::C);
    assert!(has_symbol(&syms, "get_name", SymbolKind::Function));
}

#[test]
fn c_qualified_names_are_plain() {
    let syms = symbols_for("sample.c", Language::C);
    let add = syms.iter().find(|s| s.name == "add").unwrap();
    assert_eq!(add.qualified_name, "add");
}

#[test]
fn cpp_extracts_all_symbol_types() {
    let syms = symbols_for("sample.cpp", Language::Cpp);

    assert!(has_symbol(&syms, "free_function", SymbolKind::Function));
    assert!(has_symbol(&syms, "Connection", SymbolKind::Class));
    assert!(has_symbol(&syms, "Endpoint", SymbolKind::Struct));
    assert!(has_symbol(&syms, "Tag", SymbolKind::Struct)); // union -> Struct
    assert!(has_symbol(&syms, "State", SymbolKind::Enum));
    assert!(has_symbol(&syms, "ulong", SymbolKind::Constant)); // typedef
    assert!(has_symbol(&syms, "Bytes", SymbolKind::Constant)); // using-alias
    assert!(has_symbol(&syms, "Hashable", SymbolKind::Constant)); // concept
    assert!(has_symbol(&syms, "CPP_MAX", SymbolKind::Macro));
    assert!(has_symbol(&syms, "Buffer", SymbolKind::Class)); // template-wrapped class
}

#[test]
fn cpp_methods_inside_class() {
    let syms = symbols_for("sample.cpp", Language::Cpp);

    // In-class method definitions refined to Method via parent walk.
    assert!(has_symbol(&syms, "send", SymbolKind::Method));
    assert!(has_symbol(&syms, "push", SymbolKind::Method));
    // Out-of-line method defs (`void Foo::bar() {}`) -> Method directly.
    assert!(has_symbol(&syms, "open", SymbolKind::Method));
    assert!(has_symbol(&syms, "close", SymbolKind::Method));
    // Out-of-line template method (`T Buffer<T>::pop() {}`) -> Method.
    assert!(has_symbol(&syms, "pop", SymbolKind::Method));
}

#[test]
fn cpp_constructor_destructor_operator() {
    let syms = symbols_for("sample.cpp", Language::Cpp);

    // Out-of-line ctor/dtor (Method) -- the in-class declarations have no body
    // so they don't surface as function_definitions.
    let ctors: Vec<_> = syms.iter().filter(|s| s.name == "Connection").collect();
    // One Class + one Method (ctor). Destructor name is `~Connection`.
    assert!(ctors.iter().any(|s| s.kind == SymbolKind::Method));
    assert!(has_symbol(&syms, "~Connection", SymbolKind::Method));
    // Operator overload defined in-class -> Method.
    let op = syms
        .iter()
        .find(|s| s.name == "operator=")
        .expect("operator= should be captured");
    assert_eq!(op.kind, SymbolKind::Method);
}

#[test]
fn cpp_qualified_names_with_namespace() {
    let syms = symbols_for("sample.cpp", Language::Cpp);

    // Free function inside `namespace app { namespace net { ... } }`.
    let ff = syms
        .iter()
        .find(|s| s.name == "free_function")
        .expect("free_function symbol");
    assert_eq!(ff.qualified_name, "app::net::free_function");

    // In-class method definition.
    let send = syms.iter().find(|s| s.name == "send").expect("send symbol");
    assert_eq!(send.qualified_name, "app::net::Connection::send");

    // Class symbol carries namespace prefix.
    let conn_class = syms
        .iter()
        .find(|s| s.name == "Connection" && s.kind == SymbolKind::Class)
        .expect("Connection class");
    assert_eq!(conn_class.qualified_name, "app::net::Connection");

    // Out-of-line method: captured name `app::net::Connection::open` is used
    // as-is for qualified_name.
    let open = syms.iter().find(|s| s.name == "open").expect("open method");
    assert_eq!(open.qualified_name, "app::net::Connection::open");
}

#[test]
fn cpp_line_numbers_are_positive() {
    let syms = symbols_for("sample.cpp", Language::Cpp);
    for s in &syms {
        assert!(s.line_start > 0, "symbol {} has line_start 0", s.name);
        assert!(
            s.line_end >= s.line_start,
            "symbol {} has bad line range",
            s.name
        );
    }
}

#[test]
fn rust_extracts_all_symbol_types() {
    let syms = symbols_for("sample.rs", Language::Rust);

    assert!(has_symbol(&syms, "process", SymbolKind::Function));
    assert!(has_symbol(&syms, "helper", SymbolKind::Function));
    assert!(has_symbol(&syms, "Config", SymbolKind::Struct));
    assert!(has_symbol(&syms, "LogLevel", SymbolKind::Enum));
    assert!(has_symbol(&syms, "Serializable", SymbolKind::Trait));
    assert!(has_symbol(&syms, "MAX_SIZE", SymbolKind::Constant));
    assert!(has_symbol(&syms, "GLOBAL_NAME", SymbolKind::Constant));
    assert!(has_symbol(&syms, "Result", SymbolKind::Constant)); // type alias
    assert!(has_symbol(&syms, "utils", SymbolKind::Module));
    assert!(has_symbol(&syms, "log_msg", SymbolKind::Macro));
}

#[test]
fn rust_methods_inside_impl() {
    let syms = symbols_for("sample.rs", Language::Rust);

    assert!(has_symbol(&syms, "new", SymbolKind::Method));
    assert!(has_symbol(&syms, "with_debug", SymbolKind::Method));
    assert!(has_symbol(&syms, "serialize", SymbolKind::Method));
}

#[test]
fn rust_qualified_names() {
    let syms = symbols_for("sample.rs", Language::Rust);

    let new_method = syms.iter().find(|s| s.name == "new").unwrap();
    assert_eq!(new_method.qualified_name, "Config::new");

    // Two `serialize` symbols now exist: the trait method signature
    // (`Serializable::serialize`) and the impl (`Config::serialize`).
    let impl_serialize = syms
        .iter()
        .find(|s| s.name == "serialize" && s.qualified_name == "Config::serialize")
        .expect("impl serialize resolves to Config::serialize");
    assert_eq!(impl_serialize.kind, SymbolKind::Method);
    let trait_serialize = syms
        .iter()
        .find(|s| s.name == "serialize" && s.qualified_name == "Serializable::serialize")
        .expect("trait signature serialize resolves to Serializable::serialize");
    assert_eq!(trait_serialize.kind, SymbolKind::Method);

    let process = syms.iter().find(|s| s.name == "process").unwrap();
    assert_eq!(process.qualified_name, "process");
}

#[test]
fn typescript_extracts_all_symbol_types() {
    let syms = symbols_for("sample.ts", Language::TypeScript);

    assert!(has_symbol(&syms, "createLogger", SymbolKind::Function));
    assert!(has_symbol(&syms, "UserService", SymbolKind::Class));
    assert!(has_symbol(&syms, "constructor", SymbolKind::Method));
    assert!(has_symbol(&syms, "findAll", SymbolKind::Method));
    assert!(has_symbol(&syms, "findById", SymbolKind::Method));
    assert!(has_symbol(&syms, "delete", SymbolKind::Method));
    assert!(has_symbol(&syms, "Identifiable", SymbolKind::Interface));
    assert!(has_symbol(&syms, "UserRole", SymbolKind::Constant)); // type alias
    assert!(has_symbol(&syms, "Status", SymbolKind::Enum));
    assert!(has_symbol(&syms, "DEFAULT_TIMEOUT", SymbolKind::Constant)); // exported const
}

#[test]
fn typescript_qualified_names() {
    let syms = symbols_for("sample.ts", Language::TypeScript);

    let find_all = syms.iter().find(|s| s.name == "findAll").unwrap();
    assert_eq!(find_all.qualified_name, "UserService.findAll");

    let create_logger = syms.iter().find(|s| s.name == "createLogger").unwrap();
    assert_eq!(create_logger.qualified_name, "createLogger");
}

#[test]
fn javascript_extracts_all_symbol_types() {
    let syms = symbols_for("sample.js", Language::JavaScript);

    assert!(has_symbol(&syms, "createApp", SymbolKind::Function));
    assert!(has_symbol(&syms, "middleware", SymbolKind::Function));
    assert!(has_symbol(&syms, "Router", SymbolKind::Class));
    assert!(has_symbol(&syms, "constructor", SymbolKind::Method));
    assert!(has_symbol(&syms, "get", SymbolKind::Method));
    assert!(has_symbol(&syms, "post", SymbolKind::Method));
    assert!(has_symbol(&syms, "express", SymbolKind::Constant)); // top-level const
    assert!(has_symbol(&syms, "DEFAULT_PORT", SymbolKind::Constant)); // top-level const
}

#[test]
fn javascript_qualified_names() {
    let syms = symbols_for("sample.js", Language::JavaScript);

    let get_method = syms.iter().find(|s| s.name == "get").unwrap();
    assert_eq!(get_method.qualified_name, "Router.get");

    let create_app = syms.iter().find(|s| s.name == "createApp").unwrap();
    assert_eq!(create_app.qualified_name, "createApp");
}

#[test]
fn javascript_does_not_capture_local_consts() {
    let syms = symbols_for("sample.js", Language::JavaScript);

    // 'app' is a const inside createApp(), should NOT be captured
    let apps: Vec<_> = syms.iter().filter(|s| s.name == "app").collect();
    assert!(apps.is_empty(), "local const 'app' should not be extracted");
}

#[test]
fn go_extracts_all_symbol_types() {
    let syms = symbols_for("sample.go", Language::Go);

    assert!(has_symbol(&syms, "NewConfig", SymbolKind::Function));
    assert!(has_symbol(&syms, "process", SymbolKind::Function));
    assert!(has_symbol(&syms, "Config", SymbolKind::Struct));
    assert!(has_symbol(&syms, "Server", SymbolKind::Struct));
    assert!(has_symbol(&syms, "Handler", SymbolKind::Interface));
    assert!(has_symbol(&syms, "Duration", SymbolKind::Constant)); // type alias
    assert!(has_symbol(&syms, "MaxRetries", SymbolKind::Constant));
    assert!(has_symbol(&syms, "DefaultPort", SymbolKind::Constant));
    assert!(has_symbol(&syms, "DefaultHost", SymbolKind::Constant));
}

#[test]
fn go_extracts_methods() {
    let syms = symbols_for("sample.go", Language::Go);

    assert!(has_symbol(&syms, "String", SymbolKind::Method));
    assert!(has_symbol(&syms, "WithDebug", SymbolKind::Method));
    assert!(has_symbol(&syms, "Start", SymbolKind::Method));
}

#[test]
fn go_qualified_names_pointer_receiver() {
    let syms = symbols_for("sample.go", Language::Go);

    let string_method = syms.iter().find(|s| s.name == "String").unwrap();
    assert_eq!(string_method.qualified_name, "Config.String");

    let with_debug = syms.iter().find(|s| s.name == "WithDebug").unwrap();
    assert_eq!(with_debug.qualified_name, "Config.WithDebug");
}

#[test]
fn go_qualified_names_value_receiver() {
    let syms = symbols_for("sample.go", Language::Go);

    let start = syms.iter().find(|s| s.name == "Start").unwrap();
    assert_eq!(start.qualified_name, "Server.Start");
}

#[test]
fn go_qualified_names_functions_are_plain() {
    let syms = symbols_for("sample.go", Language::Go);

    let new_config = syms.iter().find(|s| s.name == "NewConfig").unwrap();
    assert_eq!(new_config.qualified_name, "NewConfig");

    let process_fn = syms.iter().find(|s| s.name == "process").unwrap();
    assert_eq!(process_fn.qualified_name, "process");
}

#[test]
fn go_line_numbers_are_positive() {
    let syms = symbols_for("sample.go", Language::Go);
    for s in &syms {
        assert!(s.line_start > 0, "symbol {} has line_start 0", s.name);
        assert!(
            s.line_end >= s.line_start,
            "symbol {} has bad line range",
            s.name
        );
    }
}

fn symbols_from_source(source: &str, language: Language) -> Vec<codesage_protocol::Symbol> {
    let bytes = source.as_bytes();
    let tree = parse_file(bytes, language).unwrap();
    extract_symbols(&tree, bytes, language, "inline").unwrap()
}

#[test]
fn nested_function_in_python_method_is_not_a_method() {
    // A local helper defined inside a method must stay a Function with an
    // unprefixed qualified name — not Method `A.helper`.
    let src = "class A:\n    def m(self):\n        def helper():\n            return 1\n        return helper()\n";
    let syms = symbols_from_source(src, Language::Python);
    let helper = syms
        .iter()
        .find(|s| s.name == "helper")
        .expect("helper extracted");
    assert_eq!(
        helper.kind,
        SymbolKind::Function,
        "nested def must stay Function"
    );
    assert_eq!(
        helper.qualified_name, "helper",
        "nested def must not get a class prefix"
    );
    // The real method is unaffected.
    let m = syms
        .iter()
        .find(|s| s.name == "m")
        .expect("method m extracted");
    assert_eq!(m.kind, SymbolKind::Method);
    assert_eq!(m.qualified_name, "A.m");
}

#[test]
fn rust_trait_method_signature_is_captured() {
    // `function_signature_item` (a trait method without a body) must
    // surface as a Method qualified by the trait name.
    let src = "trait Store {\n    fn get(&self, k: &str) -> u8;\n}\n";
    let syms = symbols_from_source(src, Language::Rust);
    let get = syms
        .iter()
        .find(|s| s.name == "get")
        .expect("trait signature get extracted");
    assert_eq!(get.kind, SymbolKind::Method);
    assert_eq!(get.qualified_name, "Store::get");
}

#[test]
fn rust_default_trait_method_is_a_method() {
    // With `trait_item` added to is_inside_impl_or_class, a default method
    // (a `function_item` with a body inside a trait) refines Function -> Method.
    let src = "trait Store {\n    fn touch(&self) { let _ = self; }\n}\n";
    let syms = symbols_from_source(src, Language::Rust);
    let touch = syms.iter().find(|s| s.name == "touch").unwrap();
    assert_eq!(touch.kind, SymbolKind::Method);
    assert_eq!(touch.qualified_name, "Store::touch");
}

#[test]
fn typescript_abstract_class_and_methods() {
    let src = "abstract class Repo {\n  abstract find(id: number): string;\n  save(): void {}\n}\nexport default abstract class Base {}\n";
    let syms = symbols_from_source(src, Language::TypeScript);
    assert!(has_symbol(&syms, "Repo", SymbolKind::Class));
    assert!(has_symbol(&syms, "Base", SymbolKind::Class));
    // Abstract method signature -> Method, qualified by the abstract class.
    let find = syms.iter().find(|s| s.name == "find").unwrap();
    assert_eq!(find.kind, SymbolKind::Method);
    assert_eq!(find.qualified_name, "Repo.find");
    // Concrete method resolves its parent class name (abstract_class_declaration
    // added to find_parent_class_name).
    let save = syms.iter().find(|s| s.name == "save").unwrap();
    assert_eq!(save.qualified_name, "Repo.save");
}

#[test]
fn go_package_level_var_is_captured_but_not_locals() {
    // Package-level `var` -> Constant; locals must stay uncaptured.
    let src =
        "package main\nvar Registry = 1\nvar A, B int\nfunc f() { var local = 2; _ = local }\n";
    let syms = symbols_from_source(src, Language::Go);
    assert!(has_symbol(&syms, "Registry", SymbolKind::Constant));
    assert!(has_symbol(&syms, "A", SymbolKind::Constant));
    assert!(has_symbol(&syms, "B", SymbolKind::Constant));
    assert!(
        !syms.iter().any(|s| s.name == "local"),
        "function-local var must not be captured as a package symbol"
    );
}

#[test]
fn generator_functions_are_captured() {
    // `generator_function_declaration` -> Function in JS and TS.
    let js = symbols_from_source("function* gen() { yield 1; }\n", Language::JavaScript);
    assert!(has_symbol(&js, "gen", SymbolKind::Function));
    let ts = symbols_from_source("function* gen() { yield 1; }\n", Language::TypeScript);
    assert!(has_symbol(&ts, "gen", SymbolKind::Function));
}

#[test]
fn top_level_var_is_captured_js_and_ts() {
    // Cheap gap: top-level `var x` (variable_declaration, not lexical_declaration).
    let js = symbols_from_source("var legacy = 1;\n", Language::JavaScript);
    assert!(has_symbol(&js, "legacy", SymbolKind::Constant));
    let ts = symbols_from_source("var legacy = 1;\n", Language::TypeScript);
    assert!(has_symbol(&ts, "legacy", SymbolKind::Constant));
}

#[test]
fn c_double_pointer_return_function_is_captured() {
    // Cheap gap: `char **f()` nests two pointer_declarator levels.
    let syms = symbols_from_source("char **get_names(void) { return 0; }\n", Language::C);
    assert!(has_symbol(&syms, "get_names", SymbolKind::Function));
}

#[test]
fn nested_function_in_rust_method_is_not_a_method() {
    let src = "struct Foo;\nimpl Foo {\n    fn method(&self) {\n        fn local() {}\n        local();\n    }\n}\n";
    let syms = symbols_from_source(src, Language::Rust);
    let local = syms
        .iter()
        .find(|s| s.name == "local")
        .expect("local extracted");
    assert_eq!(
        local.kind,
        SymbolKind::Function,
        "nested fn must stay Function"
    );
    assert_eq!(
        local.qualified_name, "local",
        "nested fn must not get an impl prefix"
    );
    let method = syms
        .iter()
        .find(|s| s.name == "method")
        .expect("method extracted");
    assert_eq!(method.kind, SymbolKind::Method);
    assert_eq!(method.qualified_name, "Foo::method");
}
