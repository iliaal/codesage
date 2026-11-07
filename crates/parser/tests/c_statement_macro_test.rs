use codesage_parser::{
    extract::extract_symbols, parse::parse_file, references::extract_references,
};
use codesage_protocol::{Language, ReferenceKind, SymbolKind};
use tree_sitter::{Node, Parser, Point};

#[test]
fn statement_macro_without_semicolon_keeps_calls_and_function_range() {
    let source = "#include \"m.h\"
int helper(int a) { return a + 1; }
int uses_assert(int c) {
    XXH_ASSERT(c)
    return helper(c);
}
";
    for (language, path) in [(Language::C, "a.c"), (Language::Cpp, "b.cpp")] {
        let tree = parse_file(source.as_bytes(), language).unwrap();
        let references = extract_references(&tree, source.as_bytes(), language, path).unwrap();
        let calls: Vec<_> = references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Call)
            .map(|reference| (reference.to_name.as_str(), reference.line, reference.col))
            .collect();
        assert_eq!(
            calls,
            [("XXH_ASSERT", 4, 4), ("helper", 5, 11)],
            "{language:?}"
        );
        let symbols = extract_symbols(&tree, source.as_bytes(), language, path).unwrap();
        let function = symbols
            .iter()
            .find(|symbol| symbol.name == "uses_assert")
            .unwrap();
        assert_eq!(function.kind, SymbolKind::Function);
        assert_eq!(
            (
                function.line_start,
                function.col_start,
                function.line_end,
                function.col_end,
            ),
            (3, 0, 6, 1)
        );
    }
}

#[test]
fn recovery_preserves_original_bytes_and_columns_with_comments_and_crlf() {
    let source = b"int first(int c) {\r\nASSERT(c) /* trailing */\r\nreturn helper(c);\r\n}\r\nint second(int c) {\r\n    CHECK(\r\n        c\r\n    ) // trailing\r\n    return other(c);\r\n}\r\n";
    let tree = parse_file(source, Language::C).unwrap();
    let expected: &[(&[u8], Point, Point)] = &[
        (b"ASSERT(c)", Point::new(1, 0), Point::new(1, 9)),
        (b"helper(c)", Point::new(2, 7), Point::new(2, 16)),
        (
            b"CHECK(\r\n        c\r\n    )",
            Point::new(5, 4),
            Point::new(7, 5),
        ),
        (b"other(c)", Point::new(8, 11), Point::new(8, 19)),
    ];
    let mut calls = Vec::new();
    collect_nodes(tree.root_node(), "call_expression", &mut calls);
    assert_eq!(calls.len(), expected.len());
    for (node, &(text, start, end)) in calls.iter().zip(expected) {
        let offset = source
            .windows(text.len())
            .position(|bytes| bytes == text)
            .unwrap();
        assert_eq!(node.byte_range(), offset..offset + text.len());
        assert_eq!(&source[node.byte_range()], text);
        assert_eq!(node.start_position(), start);
        assert_eq!(node.end_position(), end);
    }
    let mut functions = Vec::new();
    collect_nodes(tree.root_node(), "function_definition", &mut functions);
    assert_eq!(functions.len(), 2);
    for (function, name, start_row, end_row) in [
        (functions[0], b"int first".as_slice(), 0, 3),
        (functions[1], b"int second".as_slice(), 4, 9),
    ] {
        let start = source
            .windows(name.len())
            .position(|bytes| bytes == name)
            .unwrap();
        let end = start
            + source[start..]
                .iter()
                .position(|byte| *byte == b'}')
                .unwrap()
            + 1;
        assert_eq!(function.byte_range(), start..end);
        assert_eq!(function.start_position(), Point::new(start_row, 0));
        assert_eq!(function.end_position(), Point::new(end_row, 1));
    }
    assert_eq!(tree.root_node().end_byte(), source.len());
}

#[test]
fn expression_macros_and_unbraced_control_bodies_are_unchanged() {
    let controls = b"int unchanged(int c) {
    int values[] = {XXH_ASSERT(c), helper(c)};
    int value = WRAP(XXH_ASSERT(c), helper(c));
    OUTER(XXH_ASSERT(c), helper(c));
    if (c)
        XXH_ASSERT(c)
    return helper(c);
    for (;;)
        XXH_ASSERT(c)
    return helper(c);
    TYPE(int)
    local_variable;
}
";
    for prefix in [
        b"".as_slice(),
        b"int recovered(int c) {\n    ASSERT(c)\n    return helper(c);\n}\n",
    ] {
        let source = [prefix, controls].concat();
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .unwrap();
        let original = parser.parse(&source, None).unwrap();
        let recovered = parse_file(&source, Language::C).unwrap();
        if prefix.is_empty() {
            assert_eq!(
                recovered.root_node().to_sexp(),
                original.root_node().to_sexp()
            );
        }
        let mut original_functions = Vec::new();
        let mut recovered_functions = Vec::new();
        collect_nodes(
            original.root_node(),
            "function_definition",
            &mut original_functions,
        );
        collect_nodes(
            recovered.root_node(),
            "function_definition",
            &mut recovered_functions,
        );
        assert_eq!(original_functions.len(), recovered_functions.len());
        let original_control = original_functions.last().unwrap();
        let recovered_control = recovered_functions.last().unwrap();
        assert_eq!(recovered_control.to_sexp(), original_control.to_sexp());
        assert_eq!(recovered_control.range(), original_control.range());
        let control_start = original_control.start_position().row as u32 + 1;
        let original_refs =
            extract_references(&original, &source, Language::C, "control.c").unwrap();
        let recovered_refs =
            extract_references(&recovered, &source, Language::C, "control.c").unwrap();
        let original_control_refs: Vec<_> = original_refs
            .iter()
            .filter(|reference| reference.line >= control_start)
            .collect();
        let recovered_control_refs: Vec<_> = recovered_refs
            .iter()
            .filter(|reference| reference.line >= control_start)
            .collect();
        assert_eq!(recovered_control_refs, original_control_refs);
        assert_eq!(
            recovered_control_refs
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::Call)
                .count(),
            12
        );
        if prefix.is_empty() {
            assert_eq!(recovered_refs, original_refs);
        } else {
            let recovered_calls: Vec<_> = recovered_refs
                .iter()
                .filter(|reference| reference.line < control_start)
                .map(|reference| (reference.to_name.as_str(), reference.line, reference.col))
                .collect();
            assert_eq!(recovered_calls, [("ASSERT", 2, 4), ("helper", 3, 11)]);
        }
    }
}

fn collect_nodes<'tree>(node: Node<'tree>, kind: &str, nodes: &mut Vec<Node<'tree>>) {
    if node.kind() == kind {
        nodes.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes(child, kind, nodes);
    }
}
