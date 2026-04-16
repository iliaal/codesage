use anyhow::Result;
use codesage_protocol::Language;
use tree_sitter::{Parser, Tree};

pub fn parse_file(source: &[u8], language: Language) -> Result<Tree> {
    let mut parser = Parser::new();

    let ts_language = match language {
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    };

    parser.set_language(&ts_language)?;

    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parsing failed"))
}
