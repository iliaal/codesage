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
    validate_pattern_counts(&mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "{} query validation failure(s):\n{}",
            errors.len(),
            errors.join("\n")
        )
    }
}

/// Expected top-level pattern count per (language, query-kind) pair.
/// Tree-sitter pattern indices are positional and the `*_kind_map`
/// functions in `extract`/`references` map them by number, so appending,
/// removing, or reordering a pattern in an `.scm` file silently re-kinds
/// every pattern after it. `Query::new` already catches grammar bumps; this
/// table catches same-schema drift. When you change an `.scm` file, update
/// the count here AND the corresponding kind map.
const EXPECTED_SYMBOL_PATTERN_COUNTS: &[(Language, usize)] = &[
    (Language::Php, 8),
    (Language::Python, 2),
    (Language::C, 8),
    (Language::Cpp, 23),
    (Language::Java, 9),
    (Language::Rust, 10),
    (Language::JavaScript, 12),
    (Language::TypeScript, 15),
    (Language::Go, 6),
];

const EXPECTED_REF_PATTERN_COUNTS: &[(Language, usize)] = &[
    (Language::Php, 15),
    (Language::Python, 11),
    (Language::C, 3),
    (Language::Cpp, 14),
    (Language::Java, 16),
    (Language::Rust, 13),
    (Language::JavaScript, 18),
    (Language::TypeScript, 21),
    (Language::Go, 3),
];

fn validate_pattern_counts(errors: &mut Vec<String>) {
    for &(lang, src) in crate::extract::SYMBOL_QUERY_SOURCES {
        let Some(&(_, expected)) = EXPECTED_SYMBOL_PATTERN_COUNTS
            .iter()
            .find(|(l, _)| *l == lang)
        else {
            errors.push(format!(
                "{lang:?} symbol query missing from EXPECTED_SYMBOL_PATTERN_COUNTS"
            ));
            continue;
        };
        check_patterns(
            lang,
            "symbol",
            src,
            expected,
            crate::extract::kind_map_for(lang),
            errors,
        );
    }
    for &(lang, src) in crate::references::REF_QUERY_SOURCES {
        let Some(&(_, expected)) = EXPECTED_REF_PATTERN_COUNTS.iter().find(|(l, _)| *l == lang)
        else {
            errors.push(format!(
                "{lang:?} reference query missing from EXPECTED_REF_PATTERN_COUNTS"
            ));
            continue;
        };
        check_patterns(
            lang,
            "reference",
            src,
            expected,
            crate::references::ref_kind_map_for(lang),
            errors,
        );
    }
}

/// One query's share of the positional contract: the compiled pattern count
/// must equal the table, and every index below it must map to a kind (a
/// pattern no map arm covers is silently skipped at extraction time).
fn check_patterns<K>(
    lang: Language,
    kind: &str,
    src: &str,
    expected: usize,
    kind_map: fn(usize) -> Option<K>,
    errors: &mut Vec<String>,
) {
    let ts = ts_language(lang);
    let query = match Query::new(&ts, src) {
        Ok(q) => q,
        Err(_) => return, // compile failure already reported by validate_one
    };
    let actual = query.pattern_count();
    if actual != expected {
        errors.push(format!(
            "{lang:?} {kind} query has {actual} patterns but the count table says {expected}: \
             update the .scm file, the kind map, and the table together"
        ));
        return;
    }
    for i in 0..actual {
        if kind_map(i).is_none() {
            errors.push(format!(
                "{lang:?} {kind} query pattern {i} maps to no kind (silently skipped): \
                 add a kind-map arm"
            ));
        }
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
        // queries would never be checked. Assert all four tables list all 9.
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
            assert!(
                super::EXPECTED_SYMBOL_PATTERN_COUNTS
                    .iter()
                    .any(|(l, _)| *l == lang),
                "{lang:?} missing from EXPECTED_SYMBOL_PATTERN_COUNTS"
            );
            assert!(
                super::EXPECTED_REF_PATTERN_COUNTS
                    .iter()
                    .any(|(l, _)| *l == lang),
                "{lang:?} missing from EXPECTED_REF_PATTERN_COUNTS"
            );
        }
    }

    #[test]
    fn pattern_counts_match_the_kind_maps() {
        // The positional contract itself: every compiled query has exactly
        // the tabled number of patterns, and each one maps to a kind.
        if let Err(e) = validate_all_queries() {
            panic!("pattern-count gate failed:\n{e:#}");
        }
    }
}
