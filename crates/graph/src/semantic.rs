use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use codesage_embed::chunk::{ChunkConfig, chunk_text};
use codesage_embed::model::Embedder;
use codesage_protocol::{FileInfo, SemanticIndexStats, Symbol};
use codesage_storage::Database;
use rayon::prelude::*;

use codesage_parser::discover::discover_files_with_excludes;

struct ChunkedFile {
    path: String,
    language: String,
    chunks: Vec<(String, u32, u32)>,
}

fn chunk_one(root: &Path, f: &FileInfo, config: &ChunkConfig) -> Option<ChunkedFile> {
    let abs = root.join(&f.path);
    let content = std::fs::read_to_string(&abs).ok()?;
    if content.is_empty() {
        return None;
    }

    let chunks = chunk_text(&content, config);
    if chunks.is_empty() {
        return None;
    }

    let tuples: Vec<(String, u32, u32)> = chunks
        .into_iter()
        .map(|c| (c.text, c.start_line, c.end_line))
        .collect();

    Some(ChunkedFile {
        path: f.path.clone(),
        language: f.language.as_str().to_string(),
        chunks: tuples,
    })
}

fn augment_chunks(cf: &mut ChunkedFile, symbols: &[Symbol]) {
    for (text, start_line, end_line) in &mut cf.chunks {
        let header = build_chunk_header(&cf.path, symbols, *start_line, *end_line);
        if !header.is_empty() {
            *text = format!("{header}\n{text}");
        }
    }
}

fn build_chunk_header(
    file_path: &str,
    symbols: &[Symbol],
    start_line: u32,
    end_line: u32,
) -> String {
    let overlapping: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.line_start <= end_line && s.line_end >= start_line)
        .collect();

    let mut lines = vec![format!("# {file_path}")];

    for sym in &overlapping {
        let kind = sym.kind.as_str();
        if sym.qualified_name != sym.name {
            lines.push(format!("# {} ({kind})", sym.qualified_name));
        } else {
            lines.push(format!("# {} ({kind})", sym.name));
        }
    }

    lines.join("\n")
}

fn should_augment(language: &str) -> bool {
    !matches!(language, "c" | "rust")
}

use crate::index::IndexStrategy;

fn semantic_index(
    root: &Path,
    db: &Database,
    embedder: &mut Embedder,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
) -> Result<SemanticIndexStats> {
    let files = discover_files_with_excludes(root, exclude_patterns)?;
    let config = ChunkConfig::default();
    let mut stats = SemanticIndexStats::default();

    let discovered_paths: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
    let existing_chunk_paths = db.all_chunk_file_paths()?;
    for path in &existing_chunk_paths {
        if !discovered_paths.contains(path) {
            db.delete_chunks_for_file(path)?;
            stats.files_removed += 1;
        }
    }

    let to_index: Vec<&FileInfo> = match strategy {
        IndexStrategy::Full => files.iter().collect(),
        IndexStrategy::Incremental => {
            let existing_set: HashSet<String> = existing_chunk_paths.into_iter().collect();
            files
                .iter()
                .filter(|f| {
                    if !existing_set.contains(&f.path) {
                        return true;
                    }
                    !matches!(db.get_file_hash(&f.path), Ok(Some(hash)) if hash == f.content_hash)
                })
                .collect()
        }
    };

    if strategy == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_index.len();
    }

    let mut chunked: Vec<ChunkedFile> = to_index
        .par_iter()
        .filter_map(|f| chunk_one(root, f, &config))
        .collect();

    if chunked.is_empty() {
        return Ok(stats);
    }

    for cf in &mut chunked {
        if should_augment(&cf.language)
            && let Ok(symbols) = db.symbols_for_file(&cf.path) {
                augment_chunks(cf, &symbols);
            }
    }

    let all_texts: Vec<&str> = chunked
        .iter()
        .flat_map(|f| f.chunks.iter().map(|(text, _, _)| text.as_str()))
        .collect();

    let all_embeddings = embedder.embed_batch(&all_texts)?;

    let mut emb_idx = 0;
    db.execute_batch(|db| {
        for cf in &chunked {
            db.delete_chunks_for_file(&cf.path)?;
            let mut chunk_data = Vec::with_capacity(cf.chunks.len());
            for (text, start, end) in &cf.chunks {
                chunk_data.push((
                    text.clone(),
                    *start,
                    *end,
                    all_embeddings[emb_idx].clone(),
                ));
                emb_idx += 1;
            }
            db.insert_chunks(&cf.path, &cf.language, &chunk_data)?;
            stats.chunks_created += chunk_data.len();
        }
        Ok(())
    })?;

    stats.files_processed = chunked.len();
    Ok(stats)
}

pub fn semantic_full_index(
    root: &Path,
    db: &Database,
    embedder: &mut Embedder,
    exclude_patterns: &[String],
) -> Result<SemanticIndexStats> {
    semantic_index(root, db, embedder, exclude_patterns, IndexStrategy::Full)
}

pub fn semantic_incremental_index(
    root: &Path,
    db: &Database,
    embedder: &mut Embedder,
    exclude_patterns: &[String],
) -> Result<SemanticIndexStats> {
    semantic_index(root, db, embedder, exclude_patterns, IndexStrategy::Incremental)
}
