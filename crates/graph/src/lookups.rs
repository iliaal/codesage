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
    let mut entry = db.list_file_dependencies(file_path)?;
    if !entry.found {
        return Ok(entry);
    }
    // The SQL `imported_by` half joins refs to the symbols they name, so it
    // only sees imports recorded as a symbol name. JS/TS/C imports recorded
    // as a path (`./util.js`, `dir/foo.h`) and Rust `use crate::…` module
    // paths never join; resolve those against the target with the same rules
    // `impact_analysis` uses. One set-based query fetches every import ref
    // project-wide, so the sweep is O(import refs), not a per-file N+1.
    let mut known: HashSet<String> = entry.imported_by.iter().cloned().collect();
    known.insert(entry.file_path.clone());
    for (from_path, to_name) in db.import_include_refs_all()? {
        if known.contains(&from_path) {
            continue;
        }
        if import_ref_targets_file(&to_name, &from_path, &entry.file_path) {
            known.insert(from_path.clone());
            entry.imported_by.push(from_path);
        }
    }
    entry.imported_by.sort();
    Ok(entry)
}
