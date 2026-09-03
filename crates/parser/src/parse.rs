use anyhow::Result;
use codesage_protocol::Language;
use tree_sitter::{Parser, Tree};

/// Map a CodeSage `Language` to its tree-sitter grammar. Single source of truth
/// for the grammar choice — notably TypeScript → TSX (the TSX grammar is a
/// superset that also parses plain `.ts`). `parse_file` and the lazily-compiled
/// symbol/reference query tables in `extract`/`references` all route through
/// here so the mapping can't drift between them.
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

/// Parse one source file. Tree-sitter is error-tolerant: it returns `Some`
/// even for malformed input, surfacing the damage as `ERROR` / `MISSING`
/// nodes. Callers store whatever symbols/references come out, so a partial
/// parse stored silently would read as a complete file downstream. Fail
/// instead — the indexer's per-file `files_failed` accounting already skips
/// and warns on `Err`, which is the degraded-file marking available today.
pub fn parse_file(source: &[u8], language: Language) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&ts_language(language))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))?;
    if tree.root_node().has_error() {
        anyhow::bail!("tree-sitter parse produced error nodes (malformed input)");
    }
    Ok(tree)
}

/// Lossy text of the byte range a node spans. `Node::utf8_text` fails on
/// non-UTF8 input, and every extractor treated that as "empty name, skip" —
/// half-vanishing non-UTF8 files. Slicing by byte range keeps offsets valid
/// (unlike converting the whole buffer up front, which shifts them) and
/// `from_utf8_lossy` degrades to U+FFFD instead of dropping the symbol.
pub(crate) fn node_text_lossy(node: &tree_sitter::Node, source: &[u8]) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}
