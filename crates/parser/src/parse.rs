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

pub fn parse_file(source: &[u8], language: Language) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&ts_language(language))?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))
}
