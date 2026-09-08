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

/// Parse one source file and report whether the parse was degraded. Only a
/// parser that returns no tree at all (cancelled, or the language failed to
/// load) is an error.
pub fn parse_file_tolerant(source: &[u8], language: Language) -> Result<ParsedTree> {
    let mut parser = Parser::new();
    parser.set_language(&ts_language(language))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))?;
    let degraded = tree.root_node().has_error();
    Ok(ParsedTree { tree, degraded })
}

/// Decode only the node's byte range so invalid UTF-8 cannot shift offsets
/// or discard the entire symbol.
pub(crate) fn node_text_lossy(node: &tree_sitter::Node, source: &[u8]) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}
