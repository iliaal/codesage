//! Validation gate for the embedded tree-sitter queries.
//!
//! Symbol and reference `.scm` queries are compiled lazily (per language, on
//! first use) via `LazyLock` with a panicking `.expect()`. That means a
//! tree-sitter grammar version bump that renames or removes a node type or
//! field doesn't fail the build — it panics the indexer worker the first time a
//! file of the affected language is parsed, in production. `Query::new`
//! validates the query against the grammar's node-type/field schema, so
//! force-compiling every query up front turns that latent runtime panic into an
//! eager, CI-visible error.

use anyhow::{Result, bail};
use codesage_protocol::Language;
use tree_sitter::Query;

use crate::parse::ts_language;

/// Compile every embedded symbol and reference query against its grammar and
/// verify the capture names the extractors depend on (`@name`/`@def` for symbol
/// queries, `@ref` for reference queries) exist. Returns an aggregated error
/// naming every failing (language, query-kind) pair rather than stopping at the
/// first — a grammar bump usually breaks several queries at once, and seeing all
/// of them is more useful than fixing them one panic at a time.
///
/// Backs the `cargo test` gate in this module and the `queries` check in
/// `codesage doctor`.
pub fn validate_all_queries() -> Result<()> {
    let mut errors = Vec::new();

    for &(lang, src) in crate::extract::SYMBOL_QUERY_SOURCES {
        validate_one(lang, "symbol", src, &["name", "def"], &mut errors);
    }
    for &(lang, src) in crate::references::REF_QUERY_SOURCES {
        validate_one(lang, "reference", src, &["ref"], &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "{} tree-sitter query validation failure(s):\n  {}",
            errors.len(),
            errors.join("\n  ")
        );
    }
}

fn validate_one(
    lang: Language,
    kind: &str,
    src: &str,
    required_captures: &[&str],
    errors: &mut Vec<String>,
) {
    let ts = ts_language(lang);
    match Query::new(&ts, src) {
        Ok(query) => {
            for cap in required_captures {
                if query.capture_index_for_name(cap).is_none() {
                    errors.push(format!("{lang:?} {kind} query missing @{cap} capture"));
                }
            }
        }
        Err(e) => errors.push(format!("{lang:?} {kind} query failed to compile: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_embedded_queries_compile_against_their_grammars() {
        // Grammar-bump guard: if a tree-sitter dependency renames a node type or
        // field, the matching .scm query stops compiling. Without this gate that
        // surfaces as a runtime panic on the first file of the affected language;
        // with it, the break fails CI.
        if let Err(e) = validate_all_queries() {
            panic!("embedded tree-sitter queries failed validation:\n{e:#}");
        }
    }

    #[test]
    fn source_tables_cover_every_language() {
        // The validation gate is only as good as its coverage: if a language is
        // added to extract/references but left out of the SOURCES tables, its
        // queries would never be checked. Assert both tables list all 9.
        let langs = [
            Language::Php,
            Language::Python,
            Language::C,
            Language::Cpp,
            Language::Java,
            Language::Rust,
            Language::JavaScript,
            Language::TypeScript,
            Language::Go,
        ];
        for lang in langs {
            assert!(
                crate::extract::SYMBOL_QUERY_SOURCES
                    .iter()
                    .any(|(l, _)| *l == lang),
                "{lang:?} missing from SYMBOL_QUERY_SOURCES"
            );
            assert!(
                crate::references::REF_QUERY_SOURCES
                    .iter()
                    .any(|(l, _)| *l == lang),
                "{lang:?} missing from REF_QUERY_SOURCES"
            );
        }
    }
}
