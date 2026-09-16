use codesage_parser::{extract::extract_symbols, parse::parse_file};
use codesage_protocol::{Language, RationaleKind};

fn check_rationale(language: Language, source: &str) {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
    let annotated = symbols.iter().find(|s| s.name == "annotated").unwrap();
    let rationale: Vec<_> = annotated
        .rationale
        .iter()
        .map(|r| (r.kind, r.text.as_str()))
        .collect();
    assert_eq!(
        rationale,
        [
            (RationaleKind::Why, "leading owner"),
            (RationaleKind::Note, "trailing owner")
        ],
        "{language:?}: {}",
        tree.root_node().to_sexp()
    );
    for name in ["before", "after"] {
        let symbol = symbols.iter().find(|s| s.name == name).unwrap();
        assert!(
            symbol.rationale.is_empty(),
            "{language:?}: {name}: {:?}",
            symbol.rationale
        );
    }
}

#[test]
fn rust_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Rust,
        "fn before() {}\n/**\n * WHY: leading owner\n */\n#[inline]\nfn annotated() {} // NOTE: trailing owner\nfn after() {}\n",
    );
}

#[test]
fn python_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Python,
        "def before(): pass\n# WHY: leading owner\n@decorator\ndef annotated(): # NOTE: trailing owner\n    pass\ndef after(): pass\n",
    );
}

#[test]
fn python_single_line_definition_keeps_trailing_rationale() {
    check_rationale(
        Language::Python,
        "def before(): pass\n# WHY: leading owner\ndef annotated(): pass # NOTE: trailing owner\ndef after(): pass\n",
    );
}

#[test]
fn rationale_attaches_to_multiline_declaration_headers() {
    for (language, source) in [
        (
            Language::Python,
            "def before(): pass\n# WHY: leading owner\ndef annotated(\n): # NOTE: trailing owner\n    pass\ndef after(): pass\n",
        ),
        (
            Language::Rust,
            "fn before() {}\n/* WHY: leading owner */\nfn annotated(\n) { // NOTE: trailing owner\n}\nfn after() {}\n",
        ),
        (
            Language::JavaScript,
            "function before() {}\n/* WHY: leading owner */\nexport function annotated(\n) { // NOTE: trailing owner\n}\nfunction after() {}\n",
        ),
    ] {
        check_rationale(language, source);
    }
}

#[test]
fn c_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::C,
        "void before() {}\n/**\n * WHY: leading owner\n */\nvoid annotated() {} // NOTE: trailing owner\nvoid after() {}\n",
    );
}

#[test]
fn cpp_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Cpp,
        "void before() {}\n/**\n * WHY: leading owner\n */\nvoid annotated() {} // NOTE: trailing owner\nvoid after() {}\n",
    );
}

#[test]
fn php_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Php,
        "<?php\nfunction before() {}\n/**\n * WHY: leading owner\n */\n#[Attribute]\nfunction annotated() {} # NOTE: trailing owner\nfunction after() {}\n",
    );
}

#[test]
fn go_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Go,
        "package main\nfunc before() {}\n/**\n * WHY: leading owner\n */\nfunc annotated() {} // NOTE: trailing owner\nfunc after() {}\n",
    );
}

#[test]
fn java_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::Java,
        "class Example {\nvoid before() {}\n/**\n * WHY: leading owner\n */\n@Deprecated\nvoid annotated() {} // NOTE: trailing owner\nvoid after() {}\n}\n",
    );
}

#[test]
fn javascript_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::JavaScript,
        "function before() {}\n/**\n * WHY: leading owner\n */\nexport function annotated() {} // NOTE: trailing owner\nfunction after() {}\n",
    );
}

#[test]
fn typescript_rationale_attaches_to_its_owner() {
    check_rationale(
        Language::TypeScript,
        "function before() {}\n/**\n * WHY: leading owner\n */\nexport function annotated(): void {} // NOTE: trailing owner\nfunction after() {}\n",
    );
}

#[test]
fn typescript_class_keeps_closing_brace_rationale() {
    for export in ["", "export "] {
        check_rationale(
            Language::TypeScript,
            &format!(
                "class before {{}}\n// WHY: leading owner\n{export}class annotated {{}} // NOTE: trailing owner\nclass after {{}}\n"
            ),
        );
    }
}

#[test]
fn rationale_crosses_template_and_export_metadata() {
    for (language, source) in [
        (
            Language::Cpp,
            "void before() {}\n// WHY: leading owner\ntemplate<typename T> void annotated() {} // NOTE: trailing owner\nvoid after() {}\n",
        ),
        (
            Language::Cpp,
            "struct before {};\n// WHY: leading owner\ntemplate<typename T> struct annotated {}; // NOTE: trailing owner\nstruct after {};\n",
        ),
        (
            Language::JavaScript,
            "class before {}\n// WHY: leading owner\n@dec\nexport class annotated {} // NOTE: trailing owner\nclass after {}\n",
        ),
        (
            Language::TypeScript,
            "class before {}\n// WHY: leading owner\n@dec\nexport class annotated {} // NOTE: trailing owner\nclass after {}\n",
        ),
        (
            Language::JavaScript,
            "function before() {}\n// WHY: leading owner\nexport /* ordinary */ function annotated() {} // NOTE: trailing owner\nfunction after() {}\n",
        ),
    ] {
        check_rationale(language, source);
    }
}

#[test]
fn exported_arrow_keeps_opening_brace_rationale() {
    for language in [Language::JavaScript, Language::TypeScript] {
        check_rationale(
            language,
            "const before = 0;\n// WHY: leading owner\nexport const annotated = () => { // NOTE: trailing owner\nreturn 1;\n};\nconst after = 2;\n",
        );
    }
}

#[test]
fn go_grouped_constant_keeps_its_internal_comments() {
    for other in ["", "other = 3\n"] {
        check_rationale(
            Language::Go,
            &format!(
                "package main\nconst before = 0\nconst (\n// WHY: leading owner\nannotated = 1 // NOTE: trailing owner\n{other})\nconst after = 2\n"
            ),
        );
    }
}

#[test]
fn go_grouped_types_keep_their_own_comments() {
    for definition in ["struct {}", "interface {}", "= int"] {
        check_rationale(
            Language::Go,
            &format!(
                "package main\ntype (\nbefore struct {{}}\n// WHY: leading owner\nannotated {definition} // NOTE: trailing owner\nafter struct {{}}\n)\n"
            ),
        );
        check_rationale(
            Language::Go,
            &format!(
                "package main\ntype before struct {{}}\n// WHY: leading owner\ntype annotated {definition} // NOTE: trailing owner\ntype after struct {{}}\n"
            ),
        );
    }
}

#[test]
fn shared_declarations_keep_trailing_comments_on_their_owner() {
    for (language, prefix, suffix, declaration) in [
        (Language::Java, "class Example {\n", "}\n", "int"),
        (Language::C, "", "", "const int"),
        (Language::Cpp, "", "", "const int"),
        (
            Language::Cpp,
            "struct Example {\n",
            "};\n",
            "static const int",
        ),
        (Language::Php, "<?php\n", "", "const"),
        (Language::JavaScript, "", "", "const"),
        (Language::TypeScript, "", "", "const"),
    ] {
        for separator in [" ", "\n"] {
            for annotated_last in [false, true] {
                let declarations = if annotated_last {
                    format!(
                        "// WHY: shared declaration\n{declaration} before = 0,{separator}annotated = 1; // NOTE: annotated only\n{declaration} after = 2;\n"
                    )
                } else {
                    format!(
                        "{declaration} before = 0;\n// WHY: shared declaration\n{declaration} annotated = 1, /* NOTE: annotated only */{separator}after = 2;\n"
                    )
                };
                let source = format!("{prefix}{declarations}{suffix}");
                let tree = parse_file(source.as_bytes(), language).unwrap();
                assert!(
                    !tree.root_node().has_error(),
                    "{language:?}: {}",
                    tree.root_node().to_sexp()
                );
                let symbols =
                    extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
                for name in ["before", "annotated", "after"] {
                    let symbol = symbols.iter().find(|s| s.name == name).unwrap();
                    let notes: Vec<_> = symbol
                        .rationale
                        .iter()
                        .filter(|r| r.kind == RationaleKind::Note)
                        .map(|r| r.text.as_str())
                        .collect();
                    let expected_notes: &[&str] = if name == "annotated" {
                        &["annotated only"]
                    } else {
                        &[]
                    };
                    assert_eq!(notes, expected_notes, "{language:?}: {name}: {source}");
                    let in_group = name == "annotated"
                        || name == if annotated_last { "before" } else { "after" };
                    let whys: Vec<_> = symbol
                        .rationale
                        .iter()
                        .filter(|r| r.kind == RationaleKind::Why)
                        .map(|r| r.text.as_str())
                        .collect();
                    let expected_whys: &[&str] = if in_group {
                        &["shared declaration"]
                    } else {
                        &[]
                    };
                    assert_eq!(whys, expected_whys, "{language:?}: {name}: {source}");
                }
            }
        }
    }
}

#[test]
fn go_type_headers_keep_same_line_rationale() {
    for definition in ["struct", "interface", "= struct", "= interface"] {
        check_rationale(
            Language::Go,
            &format!(
                "package main\ntype before struct {{}}\n// WHY: leading owner\ntype annotated {definition} {{ // NOTE: trailing owner\n}}\ntype after struct {{}}\n"
            ),
        );
    }
}

#[test]
fn go_multiple_names_keep_trailing_comments_on_their_owner() {
    for declaration in ["const", "var"] {
        for declarations in [
            format!(
                "{declaration} before = 0\n{declaration} annotated, /* NOTE: annotated only */ other = 1, 2\n{declaration} after = 3\n"
            ),
            format!(
                "{declaration} before, other = 0, 1 // NOTE: other only\n{declaration} annotated = 2 // NOTE: annotated only\n{declaration} after = 3\n"
            ),
        ] {
            let source = format!("package main\n{declarations}");
            let tree = parse_file(source.as_bytes(), Language::Go).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{}",
                tree.root_node().to_sexp()
            );
            let symbols =
                extract_symbols(&tree, source.as_bytes(), Language::Go, "fixture.go").unwrap();
            for name in ["before", "annotated", "after"] {
                let symbol = symbols
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap_or_else(|| panic!("missing {name}: {source}: {symbols:?}"));
                let notes: Vec<_> = symbol.rationale.iter().map(|r| r.text.as_str()).collect();
                let expected: &[&str] = if name == "annotated" {
                    &["annotated only"]
                } else {
                    &[]
                };
                assert_eq!(notes, expected, "{name}: {source}");
            }
        }
    }
}

#[test]
fn assigned_generator_keeps_its_header_rationale() {
    for language in [Language::JavaScript, Language::TypeScript] {
        for initializer in ["function*()", "async function*()"] {
            check_rationale(
                language,
                &format!(
                    "const before = 0;\n// WHY: leading owner\nconst annotated = {initializer} {{ // NOTE: trailing owner\nyield 1;\n}};\nconst after = 2;\n"
                ),
            );
        }
    }
}

#[test]
fn callable_initializer_forms_keep_header_rationale() {
    for language in [Language::JavaScript, Language::TypeScript] {
        for (prefix, body, suffix) in [
            ("function named()", "return 1;", ""),
            ("async function named()", "return 1;", ""),
            ("function* named()", "yield 1;", ""),
            ("async function* named()", "yield 1;", ""),
            ("(() =>", "return 1;", ")"),
            ("(function*()", "yield 1;", ")"),
            ("class", "method() {}", ""),
        ] {
            check_rationale(
                language,
                &format!(
                    "const before = 0;\n// WHY: leading owner\nconst annotated = {prefix} {{ // NOTE: trailing owner\n{body}\n}}{suffix};\nconst after = 2;\n"
                ),
            );
        }
    }
}

#[test]
fn multiline_rationale_preserves_complete_comment_and_docstring_text() {
    for (language, source, expected) in [
        (
            Language::Rust,
            "fn before() {}\n/* WHY: first line\nsecond line */\nfn annotated() {}\nfn after() {}\n",
            "first line\nsecond line",
        ),
        (
            Language::Rust,
            "fn before() {}\n/** WHY: first line\n * second line */\nfn annotated() {}\nfn after() {}\n",
            "first line\n * second line",
        ),
        (
            Language::Rust,
            "fn before() {}\n/**\n * WHY: first line\n * second line\n */\nfn annotated() {}\nfn after() {}\n",
            "first line\nsecond line",
        ),
        (
            Language::Python,
            "def before(): pass\ndef annotated():\n    \"\"\"WHY: first line\n    second line\"\"\"\n    pass\ndef after(): pass\n",
            "first line\n    second line",
        ),
    ] {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
        let annotated = symbols.iter().find(|s| s.name == "annotated").unwrap();
        assert_eq!(annotated.rationale.len(), 1);
        assert_eq!(annotated.rationale[0].text, expected, "{language:?}");
        for name in ["before", "after"] {
            assert!(
                symbols
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap()
                    .rationale
                    .is_empty(),
                "{language:?}: {name}"
            );
        }
    }
}

#[test]
fn typescript_ambient_class_keeps_leading_rationale() {
    check_rationale(
        Language::TypeScript,
        "class before {}\n// WHY: leading owner\nexport declare class annotated {} // NOTE: trailing owner\nclass after {}\n",
    );
}

#[test]
fn typescript_method_decorators_preserve_adjacent_rationale() {
    for decorators in ["@dec\n", "@first\n@second\n", "@dec "] {
        check_rationale(
            Language::TypeScript,
            &format!(
                "class Example {{\nbefore() {{}}\n// WHY: leading owner\n{decorators}annotated() {{}} // NOTE: trailing owner\nafter() {{}}\n}}\n"
            ),
        );
    }
}

#[test]
fn typescript_method_decorators_do_not_inherit_gapped_or_argument_comments() {
    for declaration in [
        "// WHY: detached\n@dec\n\nannotated() {}",
        "// WHY: detached\n\n@dec\nannotated() {}",
        "@dec(\n/* WHY: argument only */\nvalue)\nannotated() {}",
        "// WHY: detached\n@first\n\n@second\nannotated() {}",
    ] {
        let source = format!("class Example {{\nbefore() {{}}\n{declaration}\nafter() {{}}\n}}\n");
        let tree = parse_file(source.as_bytes(), Language::TypeScript).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let symbols =
            extract_symbols(&tree, source.as_bytes(), Language::TypeScript, "fixture.ts").unwrap();
        for name in ["before", "annotated", "after"] {
            let symbol = symbols.iter().find(|s| s.name == name).unwrap();
            assert!(
                symbol.rationale.is_empty(),
                "{name}: {source}: {:?}",
                symbol.rationale
            );
        }
    }
}

#[test]
fn java_enum_constant_keeps_trailing_rationale_across_comma() {
    for separator in ["// NOTE: trailing owner\n", "/* NOTE: trailing owner */ "] {
        check_rationale(
            Language::Java,
            &format!(
                "enum Example {{\nbefore,\n// WHY: leading owner\nannotated, {separator}after;\n}}\n"
            ),
        );
    }
}

#[test]
fn multiline_trailing_comment_does_not_extend_the_attachment_line() {
    let source =
        "fn annotated() {} /* ordinary\n*/ // NOTE: separate-line-cs25-review\nfn after() {}\n";
    let tree = parse_file(source.as_bytes(), Language::Rust).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Rust, "fixture.rs").unwrap();
    for name in ["annotated", "after"] {
        let symbol = symbols.iter().find(|s| s.name == name).unwrap();
        assert!(
            symbol.rationale.is_empty(),
            "{name}: {:?}",
            symbol.rationale
        );
    }
}

#[test]
fn macro_newline_does_not_capture_the_next_definitions_comment() {
    for language in [Language::C, Language::Cpp] {
        for newline in ["\n", "\r\n"] {
            let source = format!(
                "#define before 0{newline}// WHY: annotated-only{newline}#define annotated 1{newline}#define after 2{newline}"
            );
            let tree = parse_file(source.as_bytes(), language).unwrap();
            let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
            let annotated = symbols.iter().find(|s| s.name == "annotated").unwrap();
            assert_eq!(annotated.rationale.len(), 1);
            assert_eq!(annotated.rationale[0].text, "annotated-only");
            for name in ["before", "after"] {
                assert!(
                    symbols
                        .iter()
                        .find(|s| s.name == name)
                        .unwrap()
                        .rationale
                        .is_empty(),
                    "{language:?}: {name}"
                );
            }
        }
    }
}

#[test]
fn macro_replacement_lists_keep_only_real_trailing_comments() {
    for language in [Language::C, Language::Cpp] {
        let mut replacements = vec![
            "1",
            "call(value)",
            "prefix ## suffix",
            "do { work(); } while (0)",
            r#""// NOTE: literal only""#,
            r#""escaped \" // NOTE: literal only""#,
            r#"'/'"#,
            r#"'\''"#,
        ];
        if language == Language::Cpp {
            replacements.extend([
                r##"R"tag(// NOTE: literal only)tag""##,
                r##"u8R"tag(" // NOTE: literal only)tag""##,
                r##"R"tag(" /* NOTE: literal only */ // literal)tag""##,
            ]);
        }
        for replacement in replacements {
            for trailing in [
                "",
                " /* NOTE: trailing owner */",
                " // NOTE: trailing owner",
            ] {
                let source = format!(
                    "#define before 0\n// WHY: leading owner\n#define annotated {replacement}{trailing}\n#define after 2\n"
                );
                let tree = parse_file(source.as_bytes(), language).unwrap();
                assert!(
                    !tree.root_node().has_error(),
                    "{language:?}: {source}: {}",
                    tree.root_node().to_sexp()
                );
                let symbols =
                    extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
                let annotated = symbols.iter().find(|s| s.name == "annotated").unwrap();
                let text: Vec<_> = annotated
                    .rationale
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect();
                let expected: &[&str] = if trailing.is_empty() {
                    &["leading owner"]
                } else {
                    &["leading owner", "trailing owner"]
                };
                assert_eq!(
                    text,
                    expected,
                    "{language:?}: {source}: {}",
                    tree.root_node().to_sexp()
                );
                if !trailing.is_empty() {
                    assert_eq!(annotated.rationale[1].line_start, 3);
                    assert_eq!(annotated.rationale[1].line_end, 3);
                }
                for name in ["before", "after"] {
                    assert!(
                        symbols
                            .iter()
                            .find(|s| s.name == name)
                            .unwrap()
                            .rationale
                            .is_empty(),
                        "{language:?}: {name}: {source}"
                    );
                }
            }
        }
    }
}

#[test]
fn unsupported_macro_literal_shapes_do_not_emit_comment_contents() {
    for language in [Language::C, Language::Cpp] {
        let mut replacements = vec![
            r#""/* NOTE: literal only */""#,
            "1 /* NOTE: interior only */ + 2",
        ];
        if language == Language::Cpp {
            replacements.push(r##"R"tag(" /* NOTE: literal only */ // literal)tag""##);
        }
        for replacement in replacements {
            let source =
                format!("#define before 0\n#define annotated {replacement}\n#define after 2\n");
            let tree = parse_file(source.as_bytes(), language).unwrap();
            let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
            assert!(symbols.iter().any(|s| s.name == "before"));
            assert!(symbols.iter().any(|s| s.name == "after"));
            assert!(
                symbols.iter().all(|symbol| symbol.rationale.is_empty()),
                "{language:?}: {source}: {symbols:?}"
            );
        }
    }
}

#[test]
fn macro_multiline_comment_does_not_extend_the_trailing_row() {
    for language in [Language::C, Language::Cpp] {
        let source = "#define annotated 1 /* ordinary\n*/ // NOTE: separate row\n#define after 2\n";
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
        for name in ["annotated", "after"] {
            assert!(
                symbols
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap()
                    .rationale
                    .is_empty(),
                "{language:?}: {name}"
            );
        }
    }
}

#[test]
fn macro_continuations_and_multiline_comments_preserve_source_rows() {
    for language in [Language::C, Language::Cpp] {
        for (source, text, start, end) in [
            (
                "#define annotated \\\n1 // NOTE: continued tail\n#define after 2\n",
                "continued tail",
                2,
                2,
            ),
            (
                "#define annotated 1 /* NOTE: first line\nsecond line */\n#define after 2\n",
                "first line\nsecond line",
                1,
                2,
            ),
        ] {
            let tree = parse_file(source.as_bytes(), language).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{}",
                tree.root_node().to_sexp()
            );
            let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
            let annotated = symbols.iter().find(|s| s.name == "annotated").unwrap();
            assert_eq!(annotated.rationale.len(), 1, "{language:?}: {source}");
            let entry = &annotated.rationale[0];
            assert_eq!(
                (&*entry.text, entry.line_start, entry.line_end),
                (text, start, end)
            );
            assert!(
                symbols
                    .iter()
                    .find(|s| s.name == "after")
                    .unwrap()
                    .rationale
                    .is_empty()
            );
        }
    }
}

#[test]
fn python_multiline_header_with_inline_suite_keeps_trailing_rationale() {
    check_rationale(
        Language::Python,
        "def before(): pass\n# WHY: leading owner\ndef annotated(\n): pass # NOTE: trailing owner\ndef after(): pass\n",
    );
}

#[test]
fn adjacent_header_comments_keep_marked_rationale() {
    for (language, source) in [
        (
            Language::Rust,
            "fn before() {}\n// WHY: leading owner\nfn annotated() { /* ordinary */ /* NOTE: trailing owner */\n}\nfn after() {}\n",
        ),
        (
            Language::TypeScript,
            "function before() {}\n// WHY: leading owner\nfunction annotated() { /* ordinary */ /* NOTE: trailing owner */\n}\nfunction after() {}\n",
        ),
    ] {
        check_rationale(language, source);
    }
}

#[test]
fn multiline_header_comment_does_not_extend_the_attachment_line() {
    let source = "fn annotated() { /* ordinary\n*/ // NOTE: body-only-r4\n}\nfn after() {}\n";
    let tree = parse_file(source.as_bytes(), Language::Rust).unwrap();
    let symbols = extract_symbols(&tree, source.as_bytes(), Language::Rust, "fixture.rs").unwrap();
    for name in ["annotated", "after"] {
        assert!(
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .rationale
                .is_empty(),
            "{name}"
        );
    }
}

#[test]
fn rationale_attaches_through_single_declaration_wrappers() {
    for (language, source) in [
        (
            Language::JavaScript,
            "const before = 0;\n/* WHY: leading owner */\nexport const annotated = () => 1; // NOTE: trailing owner\nconst after = 2;\n",
        ),
        (
            Language::TypeScript,
            "const before = 0;\n/* WHY: leading owner */\nexport const annotated: number = 1; // NOTE: trailing owner\nconst after = 2;\n",
        ),
        (
            Language::JavaScript,
            "exports.before = 0;\n/* WHY: leading owner */\nexports.annotated = 1; // NOTE: trailing owner\nexports.after = 2;\n",
        ),
        (
            Language::Go,
            "package main\nconst before = 0\n/* WHY: leading owner */\nconst annotated = 1 // NOTE: trailing owner\nconst after = 2\n",
        ),
        (
            Language::C,
            "struct before {};\n/* WHY: leading owner */\nstruct annotated {}; // NOTE: trailing owner\nstruct after {};\n",
        ),
    ] {
        check_rationale(language, source);
    }
}

#[test]
fn rationale_does_not_attach_from_body_comments_or_strings() {
    for (language, source) in [
        (
            Language::Java,
            "class Example {\nvoid owner() {\n// NOTE: body-only-20260916\nString x = \"WHY: string-only-20260916\";\n}\nvoid neighbor() {}\n}\n",
        ),
        (
            Language::JavaScript,
            "export function owner() {\n// NOTE: body-only-20260916\nreturn \"WHY: string-only-20260916\";\n}\nfunction neighbor() {}\n",
        ),
        (
            Language::TypeScript,
            "export function owner() {\n// NOTE: body-only-20260916\nreturn \"WHY: string-only-20260916\";\n}\nfunction neighbor() {}\n",
        ),
    ] {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let symbols = extract_symbols(&tree, source.as_bytes(), language, "fixture").unwrap();
        for name in ["owner", "neighbor"] {
            let symbol = symbols.iter().find(|s| s.name == name).unwrap();
            assert!(
                symbol.rationale.is_empty(),
                "{language:?}: {name}: {:?}",
                symbol.rationale
            );
        }
    }
}

#[test]
fn python_trailing_comment_does_not_leak_to_nested_definition() {
    let source =
        "def outer(): # NOTE: outer-only-20260916\n    def nested(): pass\n    return nested\n";
    let tree = parse_file(source.as_bytes(), Language::Python).unwrap();
    let symbols =
        extract_symbols(&tree, source.as_bytes(), Language::Python, "fixture.py").unwrap();
    assert_eq!(
        symbols
            .iter()
            .find(|s| s.name == "outer")
            .unwrap()
            .rationale[0]
            .text,
        "outer-only-20260916"
    );
    assert!(
        symbols
            .iter()
            .find(|s| s.name == "nested")
            .unwrap()
            .rationale
            .is_empty()
    );
}
