use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;
use codesage_protocol::{Language, ReferenceKind};

fn references_for(fixture: &str, language: Language) -> Vec<codesage_protocol::Reference> {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, language).unwrap();
    extract_references(&tree, &source, language, fixture).unwrap()
}

fn refs_from_source(source: &str, language: Language) -> Vec<codesage_protocol::Reference> {
    let bytes = source.as_bytes();
    let tree = parse_file(bytes, language).unwrap();
    extract_references(&tree, bytes, language, "inline").unwrap()
}

fn has_ref(refs: &[codesage_protocol::Reference], name: &str, kind: ReferenceKind) -> bool {
    refs.iter().any(|r| r.to_name == name && r.kind == kind)
}

#[test]
fn rust_grouped_glob_and_renamed_use_emit_prefixed_imports() {
    let src = "use std::io::{Read, Write};\nuse a::b::*;\nuse x::y as z;\nuse std::{fmt, cmp::Ordering};\n";
    let refs = refs_from_source(src, Language::Rust);
    assert!(has_ref(&refs, "std::io::Read", ReferenceKind::Import));
    assert!(has_ref(&refs, "std::io::Write", ReferenceKind::Import));
    assert!(has_ref(&refs, "a::b::*", ReferenceKind::Import)); // glob module
    assert!(has_ref(&refs, "x::y", ReferenceKind::Import)); // renamed source
    assert!(has_ref(&refs, "std::fmt", ReferenceKind::Import)); // grouped bare name
    assert!(has_ref(&refs, "std::cmp::Ordering", ReferenceKind::Import)); // grouped scoped name
}

#[test]
fn php_group_use_emits_prefixed_imports() {
    // `use App\Models\{User, Post};` clauses nest under namespace_use_group.
    let src = "<?php\nuse App\\Models\\{User, Post};\n";
    let refs = refs_from_source(src, Language::Php);
    assert!(has_ref(&refs, "App\\Models\\User", ReferenceKind::Import));
    assert!(has_ref(&refs, "App\\Models\\Post", ReferenceKind::Import));
}

#[test]
fn python_relative_import_module_edge_is_captured() {
    let src = "from .models import User\nfrom . import helpers\n";
    let refs = refs_from_source(src, Language::Python);
    assert!(has_ref(&refs, ".models", ReferenceKind::Import));
    assert!(has_ref(&refs, ".", ReferenceKind::Import));
}

#[test]
fn javascript_reexport_inheritance_and_instantiation() {
    let src = "export { a } from \"./m\";\nclass Foo extends Bar {}\nconst x = new Baz();\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(has_ref(&refs, "./m", ReferenceKind::Import));
    assert!(has_ref(&refs, "Bar", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Baz", ReferenceKind::Instantiation));
}

#[test]
fn typescript_reexport_inheritance_and_instantiation() {
    let src = "export { a } from \"./m\";\nclass Foo extends Bar {}\nconst x = new Baz();\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(has_ref(&refs, "./m", ReferenceKind::Import));
    assert!(has_ref(&refs, "Bar", ReferenceKind::Inheritance));
    assert!(has_ref(&refs, "Baz", ReferenceKind::Instantiation));
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

#[test]
fn javascript_import_bindings_are_captured_separately_from_the_module() {
    let src = "import Foo from './foo.js';\n\
               import { Bar, Baz as Qux } from './bar.js';\n\
               import * as ns from './ns.js';\n";
    let refs = refs_from_source(src, Language::JavaScript);

    assert!(has_ref(&refs, "./foo.js", ReferenceKind::Import));
    assert!(has_ref(&refs, "./bar.js", ReferenceKind::Import));

    assert!(has_ref(&refs, "Foo", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "ns", ReferenceKind::ImportBinding));
    // A renamed import binds under the local alias but names the exported
    // symbol, which is what a dependents query is asking about.
    assert!(has_ref(&refs, "Baz", ReferenceKind::ImportBinding));

    assert!(!has_ref(&refs, "Foo", ReferenceKind::Import));
}

#[test]
fn typescript_import_bindings_are_captured_separately_from_the_module() {
    let src = "import Foo from './foo.js';\n\
               import { Bar } from './bar.js';\n\
               import * as ns from './ns.js';\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(has_ref(&refs, "./foo.js", ReferenceKind::Import));
    assert!(has_ref(&refs, "Foo", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "ns", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "Foo", ReferenceKind::Import));
}

#[test]
fn javascript_reexport_and_commonjs_destructuring_name_their_bindings() {
    let src = "export { x, y as z } from './m.js';\n\
               const { a, b } = require('./n.js');\n\
               a.staticMethod();\n";
    let refs = refs_from_source(src, Language::JavaScript);

    assert!(has_ref(&refs, "./m.js", ReferenceKind::Import));
    assert!(has_ref(&refs, "./n.js", ReferenceKind::Import));
    assert!(has_ref(&refs, "x", ReferenceKind::ImportBinding));
    // A renamed re-export names the source symbol, matching the import case.
    assert!(has_ref(&refs, "y", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "a", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "b", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_aliased_commonjs_destructuring_names_the_source_symbol() {
    // `{ a: localA }` is a pair_pattern, not the shorthand form, so it needs
    // its own pattern. The KEY is the exported symbol a dependents query asks
    // about; the local alias is not.
    let src = "const { a: localA, b } = require('./m.js');\nlocalA();\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(
        has_ref(&refs, "a", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "b", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "localA", ReferenceKind::ImportBinding));
}

#[test]
fn typescript_reexport_and_commonjs_destructuring_name_their_bindings() {
    let src = "export { x } from './m.js';\n\
               const { a } = require('./n.js');\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(has_ref(&refs, "x", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "a", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_barrel_destructure_and_local_reexport_name_their_symbols() {
    let src = "import axios from './lib/axios.js';\n\
               const { Axios, CancelToken, formToJSON: toJSON } = axios;\n\
               export { axios as default, Axios, CancelToken };\n";
    let refs = refs_from_source(src, Language::JavaScript);

    assert!(
        has_ref(&refs, "Axios", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "CancelToken", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "formToJSON", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "toJSON", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_destructuring_a_non_module_value_is_ignored() {
    let src = "const { data } = response;\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_destructuring_an_import_binding_is_captured() {
    let src = "import response from './r.js';\n\
               const { data, meta: m } = response;\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(has_ref(&refs, "data", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "meta", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "m", ReferenceKind::ImportBinding));
}

#[test]
fn typescript_destructuring_a_non_module_value_is_ignored() {
    let src = "const { data } = response;\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}
#[test]
fn typescript_barrel_destructure_and_local_reexport_name_their_symbols() {
    let src = "import axios from './axios.js';\n\
               const { Axios, CancelToken } = axios;\n\
               export { Axios, CancelToken };\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(
        has_ref(&refs, "Axios", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "CancelToken", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_require_then_destructure_is_captured_but_plain_unpack_is_not() {
    let src = "const axios = require('axios');\n\
               const { Foo, Bar: B } = axios;\n\
               const { data } = resp;\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(has_ref(&refs, "axios", ReferenceKind::ImportBinding));
    assert!(
        has_ref(&refs, "Foo", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "B", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}

#[test]
fn typescript_require_then_destructure_is_captured_but_plain_unpack_is_not() {
    let src = "const axios = require('axios');\n\
               const { Foo, Bar: B } = axios;\n\
               const { data } = resp;\n";
    let refs = refs_from_source(src, Language::TypeScript);
    assert!(has_ref(&refs, "axios", ReferenceKind::ImportBinding));
    assert!(
        has_ref(&refs, "Foo", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "B", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}

#[test]
fn python_decorators_name_the_applied_symbol() {
    let src = "@property\n\
               def name(self):\n    return 1\n\
               @retry(tries=3)\n\
               def flaky():\n    pass\n\
               @app.route(\"/x\")\n\
               def view():\n    pass\n";
    let refs = refs_from_source(src, Language::Python);
    assert!(has_ref(&refs, "property", ReferenceKind::Call));
    assert!(has_ref(&refs, "retry", ReferenceKind::Call));
    assert!(has_ref(&refs, "route", ReferenceKind::Call));
}

fn ref_count(refs: &[codesage_protocol::Reference], name: &str) -> usize {
    refs.iter().filter(|r| r.to_name == name).count()
}

#[test]
fn c_call_in_an_if_zero_arm_is_not_a_reference() {
    // The bead cs-j0p reproducer: the dead row inflated `find_references`,
    // breaking the `counts_floor` promise that true >= reported, and named
    // `dead_caller` as a caller in every configuration.
    let src = "void real_target(void) {}\n\
               void live_caller(void) { real_target(); }\n\
               void dead_caller(void) {\n\
               #if 0\n\
                   real_target();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    assert_eq!(ref_count(&refs, "real_target"), 1, "{refs:?}");
    assert_eq!(
        refs.iter()
            .find(|r| r.to_name == "real_target")
            .map(|r| r.line),
        Some(2)
    );
}

#[test]
fn c_call_in_an_if_one_arm_stays_live() {
    let src = "void d(void){\n#if 1\nlive();\n#endif\n}\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn c_if_zero_else_keeps_the_else_arm_only() {
    let src = "void d(void){\n#if 0\ndead();\n#else\nlive();\n#endif\n}\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead"), 0, "{refs:?}");
}

#[test]
fn c_if_one_else_keeps_the_if_arm_only() {
    let src = "void d(void){\n#if 1\nlive();\n#else\ndead();\n#endif\n}\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead"), 0, "{refs:?}");
}

#[test]
fn c_nested_conditional_inside_if_zero_is_fully_masked() {
    let src = "void d(void){\n\
               #if 0\n\
               #ifdef Q\n\
               dead_a();\n\
               #else\n\
               dead_b();\n\
               #endif\n\
               dead_c();\n\
               #endif\n\
               live();\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    assert_eq!(ref_count(&refs, "dead_a"), 0, "{refs:?}");
    assert_eq!(ref_count(&refs, "dead_b"), 0, "{refs:?}");
    assert_eq!(ref_count(&refs, "dead_c"), 0, "{refs:?}");
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn c_ifdef_ifndef_and_expression_guards_all_stay_live() {
    // Configuration, not dead code: CodeSage indexes a file once with no
    // configuration, so masking these would trade an over-count for a much
    // larger under-count.
    let src = "void d(void){\n\
               #ifdef X\n\
               live_ifdef();\n\
               #else\n\
               live_ifdef_else();\n\
               #endif\n\
               #ifndef Y\n\
               live_ifndef();\n\
               #endif\n\
               #if defined(Z) && N > 1\n\
               live_expr();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    for name in ["live_ifdef", "live_ifdef_else", "live_ifndef", "live_expr"] {
        assert!(
            has_ref(&refs, name, ReferenceKind::Call),
            "{name}: {refs:?}"
        );
    }
}

#[test]
fn c_elif_and_else_arms_of_an_if_zero_stay_live() {
    // `#if 0` proves only its own arm dead; which of the arms below runs is a
    // build fact, exactly like a bare `#if EXPR`.
    let src = "void d(void){\n\
               #if 0\n\
               dead();\n\
               #elif FOO\n\
               live_elif();\n\
               #else\n\
               live_else();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    assert_eq!(ref_count(&refs, "dead"), 0, "{refs:?}");
    assert!(has_ref(&refs, "live_elif", ReferenceKind::Call), "{refs:?}");
    assert!(has_ref(&refs, "live_else", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn c_elif_and_else_arms_after_an_if_one_are_dead() {
    // Once a group is taken, the preprocessor skips every later group of the
    // conditional in every configuration.
    let src = "void d(void){\n\
               #if 1\n\
               live();\n\
               #elif FOO\n\
               dead_elif();\n\
               #else\n\
               dead_else();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead_elif"), 0, "{refs:?}");
    assert_eq!(ref_count(&refs, "dead_else"), 0, "{refs:?}");
}

#[test]
fn c_elif_zero_arm_is_dead_and_the_chain_below_it_stays_live() {
    let src = "void d(void){\n\
               #if FOO\n\
               live_if();\n\
               #elif 0\n\
               dead_elif();\n\
               #else\n\
               live_else();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live_if", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead_elif"), 0, "{refs:?}");
    assert!(has_ref(&refs, "live_else", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn c_include_inside_if_zero_is_not_a_reference() {
    // Same decision as the dead call: the header is included in no build.
    let src = "#if 0\n#include \"dead.h\"\n#endif\n#include \"live.h\"\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live.h", ReferenceKind::Include), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead.h"), 0, "{refs:?}");
}

#[test]
fn cpp_if_zero_masks_calls_includes_bases_instantiations_and_usings() {
    // cpp_refs.scm is a separate query with eleven more patterns than C's.
    let src = "#if 0\n\
               #include \"dead.h\"\n\
               using ns::dead_using;\n\
               class DeadC : public DeadBase { void m() { dead_call(); } };\n\
               #endif\n\
               #include \"live.h\"\n\
               using ns::live_using;\n\
               class LiveC : public LiveBase { void n() { live_call(); } };\n\
               void f(){\n\
               #if 0\n\
                 auto *p = new DeadT();\n\
               #endif\n\
                 auto *q = new LiveT();\n\
               }\n";
    let refs = refs_from_source(src, Language::Cpp);
    for name in ["dead.h", "ns::dead_using", "DeadBase", "dead_call", "DeadT"] {
        assert_eq!(ref_count(&refs, name), 0, "{name}: {refs:?}");
    }
    assert!(has_ref(&refs, "live.h", ReferenceKind::Include), "{refs:?}");
    assert!(
        has_ref(&refs, "ns::live_using", ReferenceKind::Import),
        "{refs:?}"
    );
    assert!(
        has_ref(&refs, "LiveBase", ReferenceKind::Inheritance),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "live_call", ReferenceKind::Call), "{refs:?}");
    assert!(
        has_ref(&refs, "LiveT", ReferenceKind::Instantiation),
        "{refs:?}"
    );
}

#[test]
fn cpp_if_one_else_keeps_the_if_arm_only() {
    let src = "void d(){\n#if 1\nlive();\n#else\ndead();\n#endif\n}\n";
    let refs = refs_from_source(src, Language::Cpp);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead"), 0, "{refs:?}");
}

#[test]
fn cpp_if_zero_else_keeps_the_else_arm_only() {
    let src = "void d(){\n#if 0\ndead();\n#else\nlive();\n#endif\n}\n";
    let refs = refs_from_source(src, Language::Cpp);
    assert!(has_ref(&refs, "live", ReferenceKind::Call), "{refs:?}");
    assert_eq!(ref_count(&refs, "dead"), 0, "{refs:?}");
}

#[test]
fn cpp_ifdef_and_expression_guards_all_stay_live() {
    let src = "void d(){\n\
               #ifdef X\n\
               live_ifdef();\n\
               #else\n\
               live_else();\n\
               #endif\n\
               #ifndef Y\n\
               live_ifndef();\n\
               #endif\n\
               #if N > 1\n\
               live_expr();\n\
               #endif\n\
               }\n";
    let refs = refs_from_source(src, Language::Cpp);
    for name in ["live_ifdef", "live_else", "live_ifndef", "live_expr"] {
        assert!(
            has_ref(&refs, name, ReferenceKind::Call),
            "{name}: {refs:?}"
        );
    }
}

#[test]
fn c_if_zero_left_unterminated_by_a_brace_masks_nothing() {
    // The `#endif` is swallowed by the unclosed `struct S {`, so tree-sitter
    // inserts a zero-width MISSING one at EOF and the group node spans the
    // rest of the file. Masking that span would delete real references.
    let src = "#if 0\n\
               struct S {\n\
               #endif\n\
                 int x;\n\
               };\n\
               void live(void){ live_t(); }\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live_t", ReferenceKind::Call), "{refs:?}");
    assert_eq!(
        refs.iter().find(|r| r.to_name == "live_t").map(|r| r.line),
        Some(6)
    );
}

#[test]
fn c_if_zero_with_no_endif_masks_nothing() {
    let src = "#if 0\n\
               struct Kept { int x; };\n\
               void kept(void){ kept_t(); }\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "kept_t", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn c_elif_zero_left_unterminated_masks_nothing() {
    let src = "#if FOO\n\
               void a(void){}\n\
               #elif 0\n\
               struct S {\n\
               #endif\n\
                 int x; };\n\
               void live(void){ live_t(); }\n";
    let refs = refs_from_source(src, Language::C);
    assert!(has_ref(&refs, "live_t", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn cpp_if_zero_left_unterminated_by_a_brace_masks_nothing() {
    let src = "#if 0\n\
               class C {\n\
               #endif\n\
                 int x;\n\
               };\n\
               void live(){ live_t(); }\n";
    let refs = refs_from_source(src, Language::Cpp);
    assert!(has_ref(&refs, "live_t", ReferenceKind::Call), "{refs:?}");
}

#[test]
fn cpp_if_zero_with_no_endif_masks_nothing() {
    let src = "#if 0\n\
               class Kept { int x; };\n\
               void kept(){ kept_t(); }\n";
    let refs = refs_from_source(src, Language::Cpp);
    assert!(has_ref(&refs, "kept_t", ReferenceKind::Call), "{refs:?}");
}
