use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, ensure};
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

fn select_semantic_files<'a>(
    files: &'a [FileInfo],
    semantic_hashes: &HashMap<String, String>,
    strategy: IndexStrategy,
) -> Vec<&'a FileInfo> {
    match strategy {
        IndexStrategy::Full => files.iter().collect(),
        IndexStrategy::Incremental => files
            .iter()
            .filter(|f| semantic_hashes.get(&f.path) != Some(&f.content_hash))
            .collect(),
    }
}

fn count_removed_paths(orphan_chunks: &[&str], orphan_semantic_paths: &[&str]) -> usize {
    orphan_chunks
        .iter()
        .chain(orphan_semantic_paths.iter())
        .copied()
        .collect::<HashSet<_>>()
        .len()
}

fn write_semantic_updates(
    db: &Database,
    selected: &[&FileInfo],
    chunked: &[ChunkedFile],
    all_embeddings: &[Vec<f32>],
    stats: &mut SemanticIndexStats,
) -> Result<()> {
    let total_chunks: usize = chunked.iter().map(|cf| cf.chunks.len()).sum();
    ensure!(
        total_chunks == all_embeddings.len(),
        "semantic chunk/embedding count mismatch: chunks={} embeddings={}",
        total_chunks,
        all_embeddings.len()
    );

    let mut emb_idx = 0;
    db.execute_batch(|db| {
        for f in selected {
            db.delete_chunks_for_file(&f.path)?;
            db.upsert_semantic_file_hash(&f.path, &f.content_hash)?;
        }

        for cf in chunked {
            let n = cf.chunks.len();
            let chunk_data: Vec<(&str, u32, u32, &[f32])> = cf
                .chunks
                .iter()
                .zip(&all_embeddings[emb_idx..emb_idx + n])
                .map(|((text, start, end), emb)| (text.as_str(), *start, *end, emb.as_slice()))
                .collect();
            emb_idx += n;
            db.insert_chunks(&cf.path, &cf.language, &chunk_data)?;
            stats.chunks_created += n;
        }
        Ok(())
    })?;
    stats.files_processed = selected.len();
    Ok(())
}

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

    let discovered_paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let existing_chunk_paths = db.all_chunk_file_paths()?;
    let existing_semantic_hashes = db.all_semantic_file_hashes()?;
    let orphan_chunks: Vec<&str> = existing_chunk_paths
        .iter()
        .filter(|p| !discovered_paths.contains(p.as_str()))
        .map(|p| p.as_str())
        .collect();
    let orphan_semantic_paths: Vec<&str> = existing_semantic_hashes
        .keys()
        .filter(|p| !discovered_paths.contains(p.as_str()))
        .map(|p| p.as_str())
        .collect();
    let removed_count = count_removed_paths(&orphan_chunks, &orphan_semantic_paths);
    if !orphan_chunks.is_empty() || !orphan_semantic_paths.is_empty() {
        db.execute_batch(|db| {
            for path in &orphan_chunks {
                db.delete_chunks_for_file(path)?;
            }
            for path in &orphan_semantic_paths {
                db.delete_semantic_file_hash(path)?;
            }
            Ok(())
        })?;
        stats.files_removed = removed_count;
    }

    let to_index = select_semantic_files(&files, &existing_semantic_hashes, strategy);

    if strategy == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_index.len();
    }

    let mut chunked: Vec<ChunkedFile> = to_index
        .par_iter()
        .filter_map(|f| chunk_one(root, f, &config))
        .collect();

    if to_index.is_empty() {
        return Ok(stats);
    }

    // Batched symbol lookup for augmentation: one multi-path query instead of N.
    let augment_paths: Vec<String> = chunked
        .iter()
        .filter(|cf| should_augment(&cf.language))
        .map(|cf| cf.path.clone())
        .collect();
    if !augment_paths.is_empty() {
        let by_file = db.symbols_for_files(&augment_paths)?;
        for cf in &mut chunked {
            if should_augment(&cf.language)
                && let Some(symbols) = by_file.get(&cf.path)
            {
                augment_chunks(cf, symbols);
            }
        }
    }

    let all_texts: Vec<&str> = chunked
        .iter()
        .flat_map(|f| f.chunks.iter().map(|(text, _, _)| text.as_str()))
        .collect();

    let all_embeddings = embedder.embed_batch(&all_texts)?;
    write_semantic_updates(db, &to_index, &chunked, &all_embeddings, &mut stats)?;
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
    semantic_index(
        root,
        db,
        embedder,
        exclude_patterns,
        IndexStrategy::Incremental,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::Language;

    fn file(path: &str, hash: &str) -> FileInfo {
        FileInfo {
            path: path.to_string(),
            language: Language::Rust,
            content_hash: hash.to_string(),
        }
    }

    fn embedding(seed: f32) -> Vec<f32> {
        let mut v = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        v[0] = seed;
        v
    }

    #[test]
    fn incremental_selection_uses_semantic_hashes_not_structural_hashes() {
        let files = vec![file("a.rs", "new"), file("b.rs", "same")];
        let semantic_hashes = HashMap::from([
            ("a.rs".to_string(), "old".to_string()),
            ("b.rs".to_string(), "same".to_string()),
        ]);

        let selected = select_semantic_files(&files, &semantic_hashes, IndexStrategy::Incremental);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path, "a.rs");
    }

    #[test]
    fn removed_path_count_includes_metadata_only_orphans_once() {
        let count = count_removed_paths(&["gone.rs", "both.rs"], &["stale.rs", "both.rs"]);

        assert_eq!(count, 3);
    }

    #[test]
    fn semantic_update_deletes_stale_chunks_for_unchunkable_selected_file() {
        let db = Database::open_in_memory().unwrap();
        let old_embedding = embedding(0.1);
        db.insert_chunks(
            "a.rs",
            "rust",
            &[("fn stale() {}", 1, 1, old_embedding.as_slice())],
        )
        .unwrap();

        let selected = [file("a.rs", "new")];
        let selected_refs: Vec<&FileInfo> = selected.iter().collect();
        let mut stats = SemanticIndexStats::default();

        write_semantic_updates(&db, &selected_refs, &[], &[], &mut stats).unwrap();

        assert!(db.chunks_for_file("a.rs").unwrap().is_empty());
        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_created, 0);
        assert_eq!(
            db.all_semantic_file_hashes()
                .unwrap()
                .get("a.rs")
                .map(String::as_str),
            Some("new")
        );
    }

    #[test]
    fn semantic_update_replaces_chunks_and_records_hash() {
        let db = Database::open_in_memory().unwrap();
        let old_embedding = embedding(0.1);
        db.insert_chunks(
            "a.rs",
            "rust",
            &[("fn old() {}", 1, 1, old_embedding.as_slice())],
        )
        .unwrap();

        let selected = [file("a.rs", "new")];
        let selected_refs: Vec<&FileInfo> = selected.iter().collect();
        let chunked = vec![ChunkedFile {
            path: "a.rs".to_string(),
            language: "rust".to_string(),
            chunks: vec![("fn new() {}".to_string(), 1, 1)],
        }];
        let embeddings = vec![embedding(0.2)];
        let mut stats = SemanticIndexStats::default();

        write_semantic_updates(&db, &selected_refs, &chunked, &embeddings, &mut stats).unwrap();

        let chunks = db.chunks_for_file("a.rs").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "fn new() {}");
        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_created, 1);
    }
}
