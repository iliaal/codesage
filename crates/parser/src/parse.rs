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
    let neutralized = crate::export_macro::neutralize_c(source);
    let input = neutralized.as_deref().unwrap_or(source);
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
/// errors. C and C++ export macros are neutralized first (`export_macro.rs`).
pub fn parse_file_tolerant(source: &[u8], language: Language) -> Result<ParsedTree> {
    codesage_protocol::work::checkpoint()?;
    let mut parser = Parser::new();
    parser.set_language(&ts_language(language))?;
    // Every extractor reads node text from the original `source`; the
    // neutralized copy is byte-for-byte the same length with the same
    // newlines, so the tree's offsets apply to both.
    let neutralized = match language {
        Language::C => crate::export_macro::neutralize_c(source),
        Language::Cpp => crate::export_macro::neutralize(source),
        _ => None,
    };
    let input = neutralized.as_deref().unwrap_or(source);
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
    let tree = tree.ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))?;
    if language == Language::Java {
        validate_java_depth(&tree)?;
    }
    let degraded = tree.root_node().has_error();
    Ok(ParsedTree { tree, degraded })
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
