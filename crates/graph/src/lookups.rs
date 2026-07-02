use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FindReferencesRequest, FindSymbolRequest, Reference, Symbol,
};
use codesage_storage::Database;

pub fn find_symbol(db: &Database, req: &FindSymbolRequest) -> Result<Vec<Symbol>> {
    db.find_symbols(&req.name, req.kind)
}

pub fn find_references(db: &Database, req: &FindReferencesRequest) -> Result<Vec<Reference>> {
    db.find_references(&req.symbol_name, req.kind)
}

pub fn list_dependencies(db: &Database, file_path: &str) -> Result<DependencyEntry> {
    db.list_file_dependencies(file_path)
}
