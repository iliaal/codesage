use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FileInfo, IndexStats, Reference, Symbol};
use codesage_storage::Database;
use rayon::prelude::*;

use codesage_parser::discover::discover_files_with_excludes;
use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;

struct ParsedFile {
    info: FileInfo,
    symbols: Vec<Symbol>,
    refs: Vec<Reference>,
}

fn parse_one(root: &Path, file_info: &FileInfo) -> Option<ParsedFile> {
    let abs_path = root.join(&file_info.path);
    let source = std::fs::read(&abs_path).ok()?;
    let tree = parse_file(&source, file_info.language).ok()?;
    let symbols =
        extract_symbols(&tree, &source, file_info.language, &file_info.path).unwrap_or_default();
    let refs =
        extract_references(&tree, &source, file_info.language, &file_info.path).unwrap_or_default();
    Some(ParsedFile {
        info: file_info.clone(),
        symbols,
        refs,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStrategy {
    Full,
    Incremental,
}

fn index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
) -> Result<IndexStats> {
    let files = discover_files_with_excludes(root, exclude_patterns)?;
    let mut stats = IndexStats::default();

    let discovered_paths: std::collections::HashSet<String> =
        files.iter().map(|f| f.path.clone()).collect();
    let existing_paths = db.all_file_paths()?;
    for path in &existing_paths {
        if !discovered_paths.contains(path) {
            db.remove_file(path)?;
            stats.files_removed += 1;
        }
    }

    let to_parse: Vec<&FileInfo> = match strategy {
        IndexStrategy::Full => files.iter().collect(),
        IndexStrategy::Incremental => files
            .iter()
            .filter(|f| {
                !matches!(db.get_file_hash(&f.path), Ok(Some(hash)) if hash == f.content_hash)
            })
            .collect(),
    };
    if strategy == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_parse.len();
    }

    let parsed: Vec<ParsedFile> = to_parse
        .par_iter()
        .filter_map(|f| parse_one(root, f))
        .collect();

    db.execute_batch(|db| {
        for p in &parsed {
            let file_id = db.upsert_file(&p.info)?;
            db.insert_symbols(file_id, &p.symbols)?;
            db.insert_references(file_id, &p.refs)?;
        }
        Ok(())
    })?;
    for p in &parsed {
        stats.symbols_found += p.symbols.len();
        stats.references_found += p.refs.len();
        stats.files_indexed += 1;
    }

    Ok(stats)
}

pub fn full_index(root: &Path, db: &Database, exclude_patterns: &[String]) -> Result<IndexStats> {
    index(root, db, exclude_patterns, IndexStrategy::Full)
}

pub fn incremental_index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
) -> Result<IndexStats> {
    index(root, db, exclude_patterns, IndexStrategy::Incremental)
}
