use std::collections::HashSet;

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FindReferencesRequest, FindSymbolRequest, Reference, Symbol,
};
use codesage_storage::Database;

use crate::bundle::import_ref_targets_file;

pub fn find_symbol(db: &Database, req: &FindSymbolRequest) -> Result<Vec<Symbol>> {
    db.find_symbols(&req.name, req.kind)
}

pub fn find_references(db: &Database, req: &FindReferencesRequest) -> Result<Vec<Reference>> {
    db.find_references(&req.symbol_name, req.kind)
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
