use anyhow::Result;
use codesage_protocol::Language;
use tree_sitter::{Parser, Tree};

/// Shared grammar selection for parsing and queries. TSX also parses plain `.ts`.
pub(crate) fn ts_language(language: Language) -> tree_sitter::Language {
    match language {
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// Preserve recoverable subtrees around malformed syntax or unknown macros.
/// Use [`parse_file_tolerant`] to also inspect [`ParsedTree::degraded`].
pub fn parse_file(source: &[u8], language: Language) -> Result<Tree> {
    Ok(parse_file_tolerant(source, language)?.tree)
}

/// Routing needs a clean C parse, not the recovered tree. Once every live
/// tree-sitter stack is in error, recovery cannot establish a clean parse.
#[cfg(test)]
fn c_header_is_clean(source: &[u8]) -> Result<bool> {
    probe_c_header(source, true)
}

pub(crate) fn c_header_prefers_c(source: &[u8]) -> Result<bool> {
    probe_c_header(source, false)
}

fn probe_c_header(source: &[u8], accept_ambiguous: bool) -> Result<bool> {
    codesage_protocol::work::checkpoint()?;
    let mut parser = Parser::new();
    parser.set_language(&ts_language(Language::C))?;
    let prepared = prepare_c_family(source, Language::C)?;
    let input = prepared.as_deref().unwrap_or(source);
    let control = codesage_protocol::work::current();
    let mut degraded = false;
    let mut progress = |state: &tree_sitter::ParseState| {
        degraded |= state.has_error();
        if degraded
            || control
                .as_ref()
                .is_some_and(|control| control.check().is_err())
        {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| input.get(offset..).unwrap_or_default(),
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    codesage_protocol::work::checkpoint()?;
    if degraded {
        return Ok(false);
    }
    let tree = tree.ok_or_else(|| anyhow::anyhow!("C header quality parse failed"))?;
    if tree.root_node().has_error() {
        return Ok(false);
    }
    if accept_ambiguous {
        return Ok(true);
    }
    let mut cursor = tree.walk();
    loop {
        codesage_protocol::work::checkpoint()?;
        let node = cursor.node();
        if node.kind() == "function_declarator" {
            let mut children = node.walk();
            if node
                .named_children(&mut children)
                .any(|child| child.kind() == "call_expression")
            {
                // The C grammar accepts trailing macro calls as declarator
                // attributes, including repeated labels around one shared body.
                return Ok(false);
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(true);
            }
        }
    }
}

/// The C/C++ text tree-sitter parses, or `None` to parse `source` as is.
/// Export macros are neutralized (`export_macro.rs`), then conditional groups
/// whose arms each hold part of one statement keep a single arm
/// (`preproc_arms.rs`). Every extractor reads node text from the original
/// `source`; each rewrite is byte-for-byte the same length with the same
/// newlines, so the tree's offsets apply to both.
fn prepare_c_family(source: &[u8], language: Language) -> Result<Option<Vec<u8>>> {
    let neutralized = match language {
        Language::C => crate::export_macro::neutralize_c(source),
        Language::Cpp => crate::export_macro::neutralize(source),
        _ => return Ok(None),
    };
    let input = neutralized.as_deref().unwrap_or(source);
    Ok(crate::preproc_arms::select_split_arms(input)?.or(neutralized))
}

/// A parsed tree plus whether tree-sitter had to recover from syntax it
/// could not fully parse.
#[derive(Debug)]
pub struct ParsedTree {
    pub tree: Tree,
    /// Contains `ERROR` or `MISSING` nodes; undamaged regions remain extractable.
    pub degraded: bool,
}

/// Parse one source file and report whether the parse was degraded. Missing
/// trees, cancellation, and Java nesting beyond the extractor's safe depth are
/// errors. C and C++ input is prepared first (`prepare_c_family`).
pub fn parse_file_tolerant(source: &[u8], language: Language) -> Result<ParsedTree> {
    codesage_protocol::work::checkpoint()?;
    let mut parser = Parser::new();
    parser.set_language(&ts_language(language))?;
    let prepared = prepare_c_family(source, language)?;
    let input = prepared.as_deref().unwrap_or(source);
    let mut tree = parse_with_control(&mut parser, input)?;
    if matches!(language, Language::C | Language::Cpp) {
        tree = repair_macro_invocations(&mut parser, tree, input, language)?.into_tree();
    }
    if language == Language::Java {
        validate_java_depth(&tree)?;
    }
    let degraded = tree.root_node().has_error();
    Ok(ParsedTree { tree, degraded })
}

fn parse_with_control(parser: &mut Parser, input: &[u8]) -> Result<Tree> {
    let control = codesage_protocol::work::current();
    let mut progress = |_: &tree_sitter::ParseState| {
        if control
            .as_ref()
            .is_some_and(|control| control.check().is_err())
        {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| input.get(offset..).unwrap_or_default(),
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    codesage_protocol::work::checkpoint()?;
    tree.ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))
}

/// What [`repair_macro_invocations`] did with a file, carrying the tree to
/// index.
#[derive(Debug)]
enum Repair {
    /// The first parse is clean and builds no definition from a macro, or
    /// nothing in the file needs a repair.
    NotNeeded(Tree),
    /// The repaired parse had more syntax errors than the first one; the
    /// tree is the first parse again.
    MoreErrors(Tree),
    /// The repaired parse lost a definition the first parse had; the tree is
    /// the first parse again.
    LostDefinition(Tree),
    Kept(Tree),
}

impl Repair {
    fn into_tree(self) -> Tree {
        match self {
            Self::NotNeeded(tree)
            | Self::MoreErrors(tree)
            | Self::LostDefinition(tree)
            | Self::Kept(tree) => tree,
        }
    }
}

/// Reparse C/C++ with the repairs [`crate::macro_invocation::find`] locates,
/// when the first parse is degraded or holds a function definition built
/// from a macro invocation. The inserted `;` bytes are then removed from the
/// tree, so its offsets, lines, and columns are those of `source` again:
/// extractors always slice the original bytes.
///
/// The repaired tree is kept only when it has no more syntax errors than the
/// first parse and still holds, at the same body, every definition the first
/// parse had outside a function body, except the definitions the repairs
/// remove on purpose. Error recovery elsewhere in the
/// file (a brace opened inside `#ifdef`, say) can otherwise swallow a real
/// definition that the first parse kept.
///
/// Only one tree is resident at a time: the first parse is summarized and
/// dropped before the repaired parse, and parsed again when the repair is
/// refused, which is rare.
fn repair_macro_invocations(
    parser: &mut Parser,
    tree: Tree,
    source: &[u8],
    language: Language,
) -> Result<Repair> {
    if !(tree.root_node().has_error() || macro_definition(&tree)?) {
        return Ok(Repair::NotNeeded(tree));
    }
    let repairs = crate::macro_invocation::find(source)?;
    if repairs.is_empty() {
        return Ok(Repair::NotNeeded(tree));
    }
    let first_errors = error_count(&tree)?;
    let kept = top_level_definitions(&tree, source, language, Some(&repairs))?;
    drop(tree);
    let mut repaired = parse_with_control(parser, &repairs.apply(source))?;
    if error_count(&repaired)? > first_errors {
        drop(repaired);
        return Ok(Repair::MoreErrors(parse_with_control(parser, source)?));
    }
    let points = insertion_points(source, &repairs.insertions);
    for (delta, &(offset, point)) in points.iter().enumerate().rev() {
        repaired.edit(&tree_sitter::InputEdit {
            start_byte: offset + delta,
            old_end_byte: offset + delta + 1,
            new_end_byte: offset + delta,
            start_position: point,
            old_end_position: tree_sitter::Point {
                row: point.row,
                column: point.column + 1,
            },
            new_end_position: point,
        });
    }
    let found = top_level_definitions(&repaired, source, language, None)?;
    if kept.keys().any(|body| !found.contains_key(body)) {
        drop(repaired);
        return Ok(Repair::LostDefinition(parse_with_control(parser, source)?));
    }
    Ok(Repair::Kept(repaired))
}

/// Function definitions outside any function body, keyed by where their
/// body starts, with their names. The body identifies a definition across
/// the two parses even when the repair renames it (a phantom named after a
/// macro becomes the function the body belongs to). With `repairs`, a
/// definition the repairs remove on purpose is left out: one named inside a
/// blanked annotation, or one whose name is followed, inside its head, by an
/// inserted `;`, so that the name is the macro of a terminated invocation.
fn top_level_definitions<'s>(
    tree: &Tree,
    source: &'s [u8],
    language: Language,
    repairs: Option<&crate::macro_invocation::Repairs>,
) -> Result<std::collections::BTreeMap<usize, &'s [u8]>> {
    let mut definitions = std::collections::BTreeMap::new();
    walk(tree, |node| {
        if node.kind() == "function_definition"
            && !has_ancestor(node, "compound_statement")
            && let Some(name) = definition_name(node, source, language)
            && let Some(text) = source.get(name.byte_range())
        {
            let body = node
                .child_by_field_name("body")
                .map_or(node.end_byte(), |body| body.start_byte());
            if !repairs
                .is_some_and(|repairs| repairs.removes(node.start_byte()..body, name.byte_range()))
            {
                definitions.insert(body, text);
            }
        }
        false
    })?;
    Ok(definitions)
}

fn has_ancestor(node: tree_sitter::Node, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// The identifier a definition's declarator chain ends in, or the argument
/// that names a C function-defining macro head, as symbol extraction reads it.
fn definition_name<'tree>(
    definition: tree_sitter::Node<'tree>,
    source: &[u8],
    language: Language,
) -> Option<tree_sitter::Node<'tree>> {
    if language == Language::C
        && let Some(name) = crate::extract::c_macro_head_name(definition, source)
    {
        return Some(name);
    }
    let mut node = definition.child_by_field_name("declarator")?;
    loop {
        node = match node.kind() {
            "identifier"
            | "field_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name" => return Some(node),
            "function_declarator" | "pointer_declarator" => {
                node.child_by_field_name("declarator")?
            }
            _ => node.named_children(&mut node.walk()).find(|child| {
                child.kind().ends_with("declarator") || child.kind() == "identifier"
            })?,
        };
    }
}

/// Each insertion offset with its position in the inserted text: the row of
/// the original offset, and its column shifted by the insertions earlier on
/// the same row.
fn insertion_points(source: &[u8], insertions: &[usize]) -> Vec<(usize, tree_sitter::Point)> {
    let mut points = Vec::with_capacity(insertions.len());
    let (mut row, mut line_start, mut scanned) = (0, 0, 0);
    let mut previous_row = None;
    let mut same_row = 0;
    for &offset in insertions {
        for (i, byte) in source[scanned..offset].iter().enumerate() {
            if *byte == b'\n' {
                row += 1;
                line_start = scanned + i + 1;
            }
        }
        scanned = offset;
        same_row = if previous_row == Some(row) {
            same_row + 1
        } else {
            0
        };
        previous_row = Some(row);
        points.push((
            offset,
            tree_sitter::Point {
                row,
                column: offset - line_start + same_row,
            },
        ));
    }
    points
}

/// A function definition whose return type is a macro invocation, or whose
/// declarator holds a call: a clean parse can still read a semicolonless
/// `NAME(args)` run as one definition.
fn macro_definition(tree: &Tree) -> Result<bool> {
    let mut found = false;
    walk(tree, |node| {
        if node.kind() == "function_definition"
            && (node
                .child_by_field_name("type")
                .is_some_and(|ty| ty.kind() == "macro_type_specifier")
                || node
                    .child_by_field_name("declarator")
                    .is_some_and(|declarator| {
                        declarator.kind() == "function_declarator"
                            && declarator
                                .named_children(&mut declarator.walk())
                                .any(|child| child.kind() == "call_expression")
                    }))
        {
            found = true;
        }
        found
    })?;
    Ok(found)
}

fn error_count(tree: &Tree) -> Result<usize> {
    let mut count = 0;
    walk(tree, |node| {
        if node.is_error() || node.is_missing() {
            count += 1;
        }
        false
    })?;
    Ok(count)
}

/// Visit every node in document order until `visit` returns true.
fn walk(tree: &Tree, mut visit: impl FnMut(tree_sitter::Node) -> bool) -> Result<()> {
    let mut cursor = tree.walk();
    let mut visited = 0u32;
    loop {
        visited += 1;
        if visited.is_multiple_of(4096) {
            codesage_protocol::work::checkpoint()?;
        }
        if visit(cursor.node()) {
            return Ok(());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

fn validate_java_depth(tree: &Tree) -> Result<()> {
    // Java declaration-context extraction recursively visits nested types.
    const MAX_JAVA_SYNTAX_DEPTH: usize = 128;
    let mut cursor = tree.walk();
    let mut depth = 0;
    loop {
        codesage_protocol::work::checkpoint()?;
        if cursor.goto_first_child() {
            depth += 1;
            anyhow::ensure!(
                depth <= MAX_JAVA_SYNTAX_DEPTH,
                "Java syntax nesting exceeds {MAX_JAVA_SYNTAX_DEPTH} levels"
            );
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

/// Decode only the node's byte range so invalid UTF-8 cannot shift offsets
/// or discard the entire symbol.
pub(crate) fn node_text_lossy(node: &tree_sitter::Node, source: &[u8]) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod header_quality_tests {
    use super::*;

    fn parse_file_with(source: &[u8], repairs: &crate::macro_invocation::Repairs) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&ts_language(Language::C)).unwrap();
        parse_with_control(&mut parser, &repairs.apply(source)).unwrap()
    }

    #[test]
    fn clean_probe_matches_full_parse_for_clean_and_recovery_inputs() {
        let large_clean = "static inline int convert(int x) { return x + 1; }\n".repeat(500);
        let large_degraded =
            format!("struct Broken {{ UNKNOWN_FIELD(int, value) }};\n{large_clean}");
        for source in [
            b"struct Item { int value; };\n".as_slice(),
            b"class Item { public: int value; };\n",
            b"int missing( {\n",
            b"PHPAPI int exported(int value);\n",
            large_clean.as_bytes(),
            large_degraded.as_bytes(),
        ] {
            assert_eq!(
                c_header_is_clean(source).unwrap(),
                !parse_file_tolerant(source, Language::C).unwrap().degraded,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    #[test]
    fn clean_c_macro_declarator_ambiguity_retains_the_legacy_grammar() {
        let ambiguous = b"RULE(EQ(A, B))\nRULE(EQ(C, D))\nRULE(EQ(E, F))\n{ emit(); }\n";
        assert!(c_header_is_clean(ambiguous).unwrap());
        assert!(!c_header_prefers_c(ambiguous).unwrap());
        for source in [
            "int ordinary(int x) { return helper(x); }\n",
            "int *pointer_return(int x) { return helper(x); }\n",
            "typedef int result_t; result_t (parenthesized)(int x) { return helper(x); }\n",
            "int old_style(x) int x; { return helper(x); }\n",
            "#define FNAME(name) name\nint FNAME(wrapped)(int x) { return helper(x); }\n",
        ] {
            assert!(c_header_prefers_c(source.as_bytes()).unwrap(), "{source}");
        }
    }

    #[test]
    fn retained_definitions_leave_out_only_what_the_repairs_remove() {
        let source = b"int real(void) { return 0; }\nvoid printflike(1, 2) log_debug(void) {}\nint SEED(void) { return 1; }\nint outer(void) { int inner(void) { return 1; } return inner(); }\nEXPORT(a)\nEXPORT(b)\nEXPORT(c)\nint last(void) { return 0; }\n";
        let position = |needle: &[u8]| {
            source
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap()
        };
        let tree = parse_file_with(source, &crate::macro_invocation::Repairs::default());
        let names = |repairs| -> std::collections::BTreeSet<&[u8]> {
            top_level_definitions(&tree, source, Language::C, repairs)
                .unwrap()
                .into_values()
                .collect()
        };
        let all = names(None);
        let blank = position(b"printflike(1, 2)");
        let repairs = crate::macro_invocation::Repairs {
            blanks: vec![(blank, blank + 16)],
            insertions: vec![position(b"EXPORT(b)") + 9],
        };
        let kept = names(Some(&repairs));
        assert_eq!(
            all,
            [
                b"EXPORT".as_slice(),
                b"SEED",
                b"outer",
                b"printflike",
                b"real"
            ]
            .into()
        );
        assert_eq!(kept, [b"SEED".as_slice(), b"outer", b"real"].into());
    }

    fn repair_outcome(source: &[u8], language: Language) -> Repair {
        let mut parser = Parser::new();
        parser.set_language(&ts_language(language)).unwrap();
        let tree = parse_with_control(&mut parser, source).unwrap();
        repair_macro_invocations(&mut parser, tree, source, language).unwrap()
    }

    #[test]
    fn repair_with_more_syntax_errors_keeps_the_first_parse() {
        // Reduced from php-src Zend/zend_ini_scanner.h: the enum body reads as
        // a brace initializer, and splitting it off breaks the parse.
        let source =
            b"C23_ENUM(zend_ini_scanner_mode, uint8_t) {\n   ZEND_INI_SCANNER_TYPED  = 2,\n};\n";
        assert!(matches!(
            repair_outcome(source, Language::Cpp),
            Repair::MoreErrors(_)
        ));
        let mut parser = Parser::new();
        parser.set_language(&ts_language(Language::Cpp)).unwrap();
        let first = parse_with_control(&mut parser, source).unwrap();
        assert_eq!(
            parse_file(source, Language::Cpp)
                .unwrap()
                .root_node()
                .to_sexp(),
            first.root_node().to_sexp()
        );
    }

    #[test]
    fn repair_that_loses_a_definition_keeps_the_first_parse() {
        // Reduced from php-src ext/sockets/sockets.c: the brace opened under
        // #if swallows the next function once the macro run is terminated.
        let source = b"\t\tZ_PARAM_STR_OR_NULL(service)\n\t\tZ_PARAM_ARRAY(zhints)\n\t\tZ_PARAM_ZVAL_OR_NULL(error_code)\n\tZEND_PARSE_PARAMETERS_END();\n\tmemset(&hints, 0, sizeof(hints));\n#if defined(PHP_WIN32)\n\t\t\tif (rp->ai_canonname != NULL) {\n}\n/* {{{ Creates and binds to a socket from a given addrinfo resource */\nPHP_FUNCTION(socket_addrinfo_bind)\n{\n";
        assert!(matches!(
            repair_outcome(source, Language::C),
            Repair::LostDefinition(_)
        ));
        let tree = parse_file(source, Language::C).unwrap();
        let symbols = crate::extract::extract_symbols(&tree, source, Language::C, "s.c").unwrap();
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.name == "socket_addrinfo_bind"),
            "{symbols:?}"
        );
    }

    #[test]
    fn a_semicolon_before_a_definitions_name_does_not_excuse_it() {
        let source = b"ATTR_COLD(x)\nint victim(void)\n{\n\treturn helper();\n}\n";
        let tree = parse_file_with(source, &crate::macro_invocation::Repairs::default());
        let repairs = crate::macro_invocation::Repairs {
            blanks: Vec::new(),
            insertions: vec![b"ATTR_COLD(x)".len()],
        };
        let kept = top_level_definitions(&tree, source, Language::C, Some(&repairs)).unwrap();
        assert_eq!(
            kept.into_values().collect::<Vec<_>>(),
            [b"victim".as_slice()]
        );
        let tree = parse_file(source, Language::C).unwrap();
        let symbols = crate::extract::extract_symbols(&tree, source, Language::C, "v.c").unwrap();
        assert!(
            symbols.iter().any(|symbol| symbol.name == "victim"),
            "{symbols:?}"
        );
    }

    #[test]
    fn repair_that_renames_a_phantom_keeps_its_body_and_is_kept() {
        let source = b"ZEND_DECLARE_MODULE_GLOBALS(dl_test)\n\nPHP_FUNCTION(dl_test_test1)\n{\n\thelper();\n}\n";
        assert!(matches!(
            repair_outcome(source, Language::C),
            Repair::Kept(_)
        ));
        let tree = parse_file(source, Language::C).unwrap();
        let symbols = crate::extract::extract_symbols(&tree, source, Language::C, "d.c").unwrap();
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, ["dl_test_test1"]);
    }

    #[test]
    fn retained_definitions_count_each_definition_of_a_name() {
        let source =
            b"#if A\nint f(void) { return 0; }\n#else\nint f(void) { return 1; }\n#endif\n";
        let tree = parse_file_with(source, &crate::macro_invocation::Repairs::default());
        let definitions = top_level_definitions(&tree, source, Language::C, None).unwrap();
        assert_eq!(
            definitions.into_values().collect::<Vec<_>>(),
            [b"f".as_slice(), b"f"]
        );
    }

    #[test]
    #[ignore = "requires CODESAGE_HEADER_CORPUS pointing at a pinned source snapshot"]
    fn quality_probe_matches_full_parse_on_pinned_headers() {
        let root = std::env::var("CODESAGE_HEADER_CORPUS").expect("CODESAGE_HEADER_CORPUS");
        let mut compared = 0;
        for entry in ignore::WalkBuilder::new(&root).build() {
            let entry = entry.unwrap();
            if !entry.file_type().is_some_and(|kind| kind.is_file())
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "h")
                || entry.metadata().unwrap().len() > crate::discover::MAX_INDEXABLE_FILE_BYTES
            {
                continue;
            }
            let source = std::fs::read(entry.path()).unwrap();
            assert_eq!(
                c_header_is_clean(&source).unwrap(),
                !parse_file_tolerant(&source, Language::C).unwrap().degraded,
                "{}",
                entry.path().display()
            );
            compared += 1;
        }
        assert!(compared > 0, "snapshot contains no headers");
        println!("quality_probe_headers_compared={compared}");
    }
}
