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
