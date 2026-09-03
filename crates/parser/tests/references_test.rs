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
    // Grouped / glob / `as` use forms previously emitted zero imports.
    let src = "use std::io::{Read, Write};\nuse a::b::*;\nuse x::y as z;\nuse std::{fmt, cmp::Ordering};\n";
    let refs = refs_from_source(src, Language::Rust);
    assert!(has_ref(&refs, "std::io::Read", ReferenceKind::Import));
    assert!(has_ref(&refs, "std::io::Write", ReferenceKind::Import));
    assert!(has_ref(&refs, "a::b", ReferenceKind::Import)); // glob module
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
    // Same three edges as the JS test, but under the TSX grammar
    // (`extends_clause` form, which plain JS does not emit).
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
    // The module specifier alone left a file that imports a symbol and uses it
    // only as `Foo.staticMethod()` or `x instanceof Foo` with no row naming
    // that symbol, so it dropped out of the symbol's dependents.
    let src = "import Foo from './foo.js';\n\
               import { Bar, Baz as Qux } from './bar.js';\n\
               import * as ns from './ns.js';\n";
    let refs = refs_from_source(src, Language::JavaScript);

    // The modules stay `Import` so file-level dependency listings are unchanged.
    assert!(has_ref(&refs, "./foo.js", ReferenceKind::Import));
    assert!(has_ref(&refs, "./bar.js", ReferenceKind::Import));

    assert!(has_ref(&refs, "Foo", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(has_ref(&refs, "ns", ReferenceKind::ImportBinding));
    // A renamed import binds under the local alias but names the exported
    // symbol, which is what a dependents query is asking about.
    assert!(has_ref(&refs, "Baz", ReferenceKind::ImportBinding));

    // Bindings must not leak into the module list.
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
    // A barrel file forwards symbols; a CommonJS consumer destructures them.
    // Both used to record only the module string, so neither appeared in the
    // forwarded symbol's dependents.
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
    // The shape axios' index.js uses: import the default, unwrap named symbols
    // off it, re-export them. Neither the destructure (RHS is a value, not a
    // `require` call) nor the sourceless `export { ... }` named those symbols,
    // so a barrel every consumer imports through contributed no edges.
    let src = "import axios from './lib/axios.js';\n\
               const { Axios, CancelToken, formToJSON: toJSON } = axios;\n\
               export { axios as default, Axios, CancelToken };\n";
    let refs = refs_from_source(src, Language::JavaScript);

    assert!(
        has_ref(&refs, "Axios", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    assert!(has_ref(&refs, "CancelToken", ReferenceKind::ImportBinding));
    // Aliased destructuring still names the source symbol, not the alias.
    assert!(has_ref(&refs, "formToJSON", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "toJSON", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_destructuring_a_non_module_value_is_ignored() {
    // The old pattern bound ANY bare identifier on the right, so this
    // recorded a bogus ImportBinding for `data`. Value-destructures now
    // require the RHS to be a same-file import binding; `response` is not
    // imported here, so no edge is recorded.
    let src = "const { data } = response;\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}

#[test]
fn javascript_destructuring_an_import_binding_is_captured() {
    // The positive half of the filter: `response` IS imported, so unwrapping
    // `data` off it names the module's export. Aliased keys record the
    // source symbol, not the local alias.
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
    // Regression: `const axios = require('axios')` bound a local the
    // value-destructure filter did not know (patterns 7-9 are ESM-only and
    // pattern 1 keeps only the module string), so `const { Foo } = axios`
    // named no symbol. The require-bound LHS is now an ImportBinding, which
    // admits it to the filter — while an unbound `resp` still drops.
    let src = "const axios = require('axios');\n\
               const { Foo, Bar: B } = axios;\n\
               const { data } = resp;\n";
    let refs = refs_from_source(src, Language::JavaScript);
    assert!(has_ref(&refs, "axios", ReferenceKind::ImportBinding));
    assert!(
        has_ref(&refs, "Foo", ReferenceKind::ImportBinding),
        "{refs:?}"
    );
    // Aliased keys record the source symbol, not the local alias.
    assert!(has_ref(&refs, "Bar", ReferenceKind::ImportBinding));
    assert!(!has_ref(&refs, "B", ReferenceKind::ImportBinding));
    // `resp` binds nothing import-like, so its unpack stays dropped.
    assert!(!has_ref(&refs, "data", ReferenceKind::ImportBinding));
}

#[test]
fn typescript_require_then_destructure_is_captured_but_plain_unpack_is_not() {
    // TS mirror of the JS regression above: same require-then-destructure
    // shape under the TSX grammar, same discrimination against `resp`.
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
    // Parity with Java's annotation patterns: decoration sites surface as
    // Call rows naming the decorator, whether bare, called, dotted, or both.
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
