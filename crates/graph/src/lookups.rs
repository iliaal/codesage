use std::collections::HashSet;

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FindReferencesRequest, FindReferencesResults, FindSymbolRequest, Symbol,
};
use codesage_storage::Database;

use crate::bundle::import_ref_targets_file;

pub fn find_symbol(db: &Database, req: &FindSymbolRequest) -> Result<Vec<Symbol>> {
    db.find_symbols(&req.name, req.kind)
}

/// References to a name plus what the row list alone cannot say: how many
/// indexed definitions share the name (so an agent knows whether the rows are
/// the union across homonyms) and that the count is a floor.
pub fn find_references(
    db: &Database,
    req: &FindReferencesRequest,
) -> Result<FindReferencesResults> {
    let results = db.find_references(&req.symbol_name, req.kind)?;
    let definitions = db.find_symbols(&req.symbol_name, None)?;
    let definition_count = definitions.len();
    let ambiguous = definition_count > 1;
    let note = if ambiguous {
        // `impact_analysis` disambiguates by qualified name only (see
        // `impact::impact_analysis_walk`). Languages without namespaces give every
        // definition the bare name, so when the qualified names collapse to
        // one the only handle left is the file.
        let qualified = distinct_sorted(definitions.iter().map(|s| s.qualified_name.as_str()));
        if qualified.len() > 1 {
            Some(format!(
                "{definition_count} definitions share the name '{}'; rows are the union across \
                 all of them. Use find_symbol to list them and impact_analysis with one \
                 qualified name ({}) to scope to one.",
                req.symbol_name,
                sample_list(&qualified, 5)
            ))
        } else {
            let files = distinct_sorted(definitions.iter().map(|s| s.file_path.as_str()));
            Some(format!(
                "{definition_count} definitions share the name '{}' and are indistinguishable \
                 by qualified name; rows are the union across all of them. Use find_symbol to \
                 list them and scope by file instead (impact_analysis on the file, or filter \
                 rows by from_file): {}.",
                req.symbol_name,
                sample_list(&files, 5)
            ))
        }
    } else if definition_count == 0 {
        Some(format!(
            "no indexed definition named '{}'; references may target an external or unindexed \
             symbol.",
            req.symbol_name
        ))
    } else {
        None
    };
    Ok(FindReferencesResults {
        results,
        counts_floor: true,
        definition_count,
        ambiguous,
        note,
    })
}

fn distinct_sorted<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = items.collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Comma-joined prefix of `items`, with the overflow counted rather than listed.
fn sample_list(items: &[&str], max: usize) -> String {
    let shown = items[..items.len().min(max)].join(", ");
    if items.len() > max {
        format!("{shown}, +{} more", items.len() - max)
    } else {
        shown
    }
}

pub fn list_dependencies(db: &Database, file_path: &str) -> Result<DependencyEntry> {
    let mut out = list_dependencies_batch(db, &[file_path])?;
    // Batch preserves input order, so the single requested file is the only row.
    Ok(out.pop().expect("batch returns one entry per input path"))
}

/// Batched [`list_dependencies`] over many files with a single project-wide
/// import-ref sweep. The single-file wrapper delegates here, so there is one
/// resolution code path; multi-file callers should prefer this directly —
/// one sweep total instead of one per file.
pub fn list_dependencies_batch(db: &Database, file_paths: &[&str]) -> Result<Vec<DependencyEntry>> {
    // One set-based query fetches every import ref project-wide, so the
    // per-file sweep below is O(import refs), not a per-file N+1.
    let all_refs = db.import_include_refs_all()?;
    let mut out = Vec::with_capacity(file_paths.len());
    for file_path in file_paths {
        let mut entry = db.list_file_dependencies(file_path)?;
        if entry.found {
            resolve_path_imported_by(&mut entry, &all_refs);
        }
        out.push(entry);
    }
    Ok(out)
}

/// Extend `entry.imported_by` with path/module-specifier importers the SQL
/// half cannot join (see the call-site comment in [`list_dependencies`]).
fn resolve_path_imported_by(entry: &mut DependencyEntry, all_refs: &[(String, String)]) {
    // The SQL `imported_by` half joins refs to the symbols they name, so it
    // only sees imports recorded as a symbol name. JS/TS/C imports recorded
    // as a path (`./util.js`, `dir/foo.h`) and Rust `use crate::…` module
    // paths never join; resolve those against the target with the same rules
    // `impact_analysis` uses.
    let mut known: HashSet<String> = entry.imported_by.iter().cloned().collect();
    known.insert(entry.file_path.clone());
    for (from_path, to_name) in all_refs {
        if known.contains(from_path) {
            continue;
        }
        if import_ref_targets_file(to_name, from_path, &entry.file_path) {
            known.insert(from_path.clone());
            entry.imported_by.push(from_path.clone());
        }
    }
    entry.imported_by.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, SymbolKind};

    fn file(db: &Database, path: &str) -> i64 {
        db.upsert_file(&FileInfo {
            path: path.to_string(),
            language: Language::Rust,
            content_hash: path.to_string(),
        })
        .unwrap()
    }

    fn symbol(name: &str, file_path: &str) -> Symbol {
        qualified_symbol(name, name, file_path)
    }

    fn qualified_symbol(name: &str, qualified_name: &str, file_path: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind: SymbolKind::Function,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 1,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
        }
    }

    fn reference(to_name: &str, file_path: &str) -> Reference {
        Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name: to_name.to_string(),
            kind: ReferenceKind::Call,
            line: 5,
            col: 12,
        }
    }

    fn lookup(db: &Database, name: &str) -> FindReferencesResults {
        find_references(
            db,
            &FindReferencesRequest {
                symbol_name: name.to_string(),
                kind: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn envelope_with_no_definition_flags_external_target() {
        let db = Database::open_in_memory().unwrap();
        let caller = file(&db, "caller.rs");
        db.insert_references(caller, &[reference("helper", "caller.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.results.len(), 1);
        assert!(out.counts_floor);
        assert_eq!(out.definition_count, 0);
        assert!(!out.ambiguous);
        let note = out.note.expect("zero definitions must carry a note");
        assert!(note.contains("no indexed definition"), "{note}");
    }

    #[test]
    fn envelope_with_one_definition_is_unambiguous_and_silent() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();

        let out = lookup(&db, "helper");
        assert!(out.results.is_empty());
        assert!(out.counts_floor, "a zero must still read as a floor");
        assert_eq!(out.definition_count, 1);
        assert!(!out.ambiguous);
        assert!(out.note.is_none(), "note: {:?}", out.note);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["counts_floor"], serde_json::Value::Bool(true));
        assert!(json.get("note").is_none(), "{json}");
    }

    #[test]
    fn envelope_with_same_qualified_name_in_two_files_scopes_by_file() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();
        db.insert_symbols(b, &[symbol("helper", "b.rs")]).unwrap();
        db.insert_references(a, &[reference("helper", "a.rs")])
            .unwrap();
        db.insert_references(b, &[reference("helper", "b.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(
            out.results.len(),
            2,
            "rows are the union: {:?}",
            out.results
        );
        assert_eq!(out.definition_count, 2);
        assert!(out.ambiguous);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(
            note.starts_with("2 definitions share the name 'helper'"),
            "{note}"
        );
        assert!(note.contains("find_symbol"), "{note}");
        assert!(note.contains("impact_analysis"), "{note}");
        // Both carry the bare name as qualified name (JS `.d.ts` beside `.js`
        // shape): "qualify it" is unsatisfiable, so the note must name files.
        assert!(note.contains("indistinguishable"), "{note}");
        assert!(note.contains("a.rs, b.rs"), "{note}");
        assert!(!note.contains("qualified name ("), "{note}");
    }

    #[test]
    fn envelope_with_two_distinct_qualified_names_lists_them() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        db.insert_symbols(a, &[qualified_symbol("helper", "alpha::helper", "a.rs")])
            .unwrap();
        db.insert_symbols(b, &[qualified_symbol("helper", "beta::helper", "b.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.definition_count, 2);
        assert!(out.ambiguous);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(note.contains("impact_analysis"), "{note}");
        assert!(note.contains("alpha::helper, beta::helper"), "{note}");
        assert!(!note.contains("indistinguishable"), "{note}");
    }

    #[test]
    fn sample_list_counts_the_overflow() {
        let items = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(sample_list(&items, 5), "a, b, c, d, e, +2 more");
        assert_eq!(sample_list(&items[..2], 5), "a, b");
    }
}
