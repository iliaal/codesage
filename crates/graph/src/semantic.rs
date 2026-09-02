use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use codesage_embed::chunk::{ChunkConfig, chunk_text};
use codesage_embed::model::Embedder;
use codesage_protocol::{FileInfo, SemanticIndexStats, Symbol};

/// Anything that turns chunk texts into vectors. [`Embedder`] is the
/// in-process implementation; [`LazyEmbedder`] defers constructing one until
/// a pass has something to embed, and the CLI adds a daemon-backed one.
pub trait TextEmbedder {
    /// Called once per pass with the number of files about to be embedded,
    /// after the pass has established that the number is non-zero and before
    /// the first [`TextEmbedder::embed_batch`]. Implementations that acquire a
    /// backend lazily do so here.
    fn prepare(&mut self, _files_to_embed: usize) -> Result<()> {
        Ok(())
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed_batch(&[text])?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector for one text"))
    }
}

impl TextEmbedder for Embedder {
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed_batch(self, texts)
    }

    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        Embedder::embed_one(self, text)
    }
}

/// Constructor for a deferred backend. Receives the number of files the pass
/// is about to embed (`None` when a caller embeds without a preceding
/// [`TextEmbedder::prepare`]).
pub type EmbedderInit = Box<dyn FnOnce(Option<usize>) -> Result<Box<dyn TextEmbedder>>>;

/// A [`TextEmbedder`] that constructs its backend on first use.
///
/// An incremental pass computes its file set before it embeds anything, and
/// on a no-change pass that set is empty. Building the model eagerly made
/// every such pass pay a full ONNX session load — and, on a GPU device, a
/// CUDA context — to embed nothing. With this wrapper the constructor runs
/// from the first `embed_batch`, which a pass reaches only with chunk texts
/// that have no stored vector yet; [`TextEmbedder::prepare`] merely records
/// the announced file count for the constructor, so a pass whose every chunk
/// is reused never builds a backend either.
pub struct LazyEmbedder {
    inner: Option<Box<dyn TextEmbedder>>,
    init: Option<EmbedderInit>,
    announced_files: Option<usize>,
}

impl LazyEmbedder {
    pub fn new(init: EmbedderInit) -> Self {
        Self {
            inner: None,
            init: Some(init),
            announced_files: None,
        }
    }

    /// Whether the backend has been constructed.
    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    fn ensure(&mut self, files_to_embed: Option<usize>) -> Result<&mut dyn TextEmbedder> {
        if self.inner.is_none() {
            let init = self
                .init
                .take()
                .ok_or_else(|| anyhow::anyhow!("embedder construction already failed once"))?;
            self.inner = Some(init(files_to_embed)?);
        }
        Ok(self
            .inner
            .as_deref_mut()
            .expect("inner set by the branch above"))
    }
}

impl TextEmbedder for LazyEmbedder {
    fn prepare(&mut self, files_to_embed: usize) -> Result<()> {
        self.announced_files = Some(files_to_embed);
        match self.inner.as_deref_mut() {
            Some(inner) => inner.prepare(files_to_embed),
            None => Ok(()),
        }
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let announced = self.announced_files;
        self.ensure(announced)?.embed_batch(texts)
    }
}
use codesage_storage::Database;
use rayon::prelude::*;

use codesage_parser::discover::discover_files_with_excludes;

#[derive(Debug)]
struct ChunkedFile {
    path: String,
    language: String,
    chunks: Vec<(String, u32, u32)>,
}

fn chunk_one(root: &Path, f: &FileInfo, config: &ChunkConfig) -> Result<Option<ChunkedFile>> {
    let abs = root.join(&f.path);
    let content = std::fs::read_to_string(&abs)
        .with_context(|| format!("reading {} for semantic chunks", f.path))?;
    if content.is_empty() {
        return Ok(None);
    }

    let chunks = chunk_text(&content, config);
    if chunks.is_empty() {
        return Ok(None);
    }

    let tuples: Vec<(String, u32, u32)> = chunks
        .into_iter()
        .map(|c| (c.text, c.start_line, c.end_line))
        .collect();

    Ok(Some(ChunkedFile {
        path: f.path.clone(),
        language: f.language.as_str().to_string(),
        chunks: tuples,
    }))
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

/// Files committed per transaction. Smaller value = more transaction
/// overhead, less progress lost on abort. 50 is a balance: at typical
/// per-file embedding cost the transaction overhead is negligible, and
/// a killed run loses at most ~50 files of work instead of thousands.
const COMMIT_BATCH_SIZE: usize = 50;

fn write_semantic_updates(
    db: &Database,
    selected: &[&FileInfo],
    chunked: &[ChunkedFile],
    all_embeddings: &[Vec<f32>],
    stats: &mut SemanticIndexStats,
) -> Result<()> {
    write_semantic_updates_with_batch(
        db,
        selected,
        chunked,
        all_embeddings,
        stats,
        COMMIT_BATCH_SIZE,
    )
}

fn write_semantic_updates_with_batch(
    db: &Database,
    selected: &[&FileInfo],
    chunked: &[ChunkedFile],
    all_embeddings: &[Vec<f32>],
    stats: &mut SemanticIndexStats,
    batch_size: usize,
) -> Result<()> {
    let total_chunks: usize = chunked.iter().map(|cf| cf.chunks.len()).sum();
    ensure!(
        total_chunks == all_embeddings.len(),
        "semantic chunk/embedding count mismatch: chunks={} embeddings={}",
        total_chunks,
        all_embeddings.len()
    );
    ensure!(batch_size > 0, "batch_size must be > 0");

    // Bind each chunked file to its embedding slice so per-file commits
    // can look up data without re-walking the flat embedding vector.
    let mut by_path: HashMap<&str, (&ChunkedFile, &[Vec<f32>])> =
        HashMap::with_capacity(chunked.len());
    let mut emb_idx = 0;
    for cf in chunked {
        let n = cf.chunks.len();
        by_path.insert(
            cf.path.as_str(),
            (cf, &all_embeddings[emb_idx..emb_idx + n]),
        );
        emb_idx += n;
    }

    for batch in selected.chunks(batch_size) {
        db.execute_batch(|db| {
            for f in batch {
                db.delete_chunks_for_file(&f.path)?;
                if let Some((cf, embs)) = by_path.get(f.path.as_str()) {
                    let chunk_data: Vec<(&str, u32, u32, &[f32])> = cf
                        .chunks
                        .iter()
                        .zip(embs.iter())
                        .map(|((text, start, end), emb)| {
                            (text.as_str(), *start, *end, emb.as_slice())
                        })
                        .collect();
                    db.insert_chunks(&cf.path, &cf.language, &chunk_data)?;
                    stats.chunks_created += cf.chunks.len();
                }
                db.upsert_semantic_file_hash(&f.path, &f.content_hash)?;
            }
            Ok(())
        })?;
    }
    stats.files_processed += selected.len();
    Ok(())
}

fn process_semantic_batch(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    config: &ChunkConfig,
    batch: &[&FileInfo],
    stats: &mut SemanticIndexStats,
) -> Result<()> {
    let chunk_results: Vec<(&FileInfo, Result<Option<ChunkedFile>>)> = batch
        .par_iter()
        .map(|f| (*f, chunk_one(root, f, config)))
        .collect();
    let mut selected = Vec::with_capacity(batch.len());
    let mut chunked = Vec::new();
    for (file, result) in chunk_results {
        match result {
            Ok(Some(cf)) => {
                selected.push(file);
                chunked.push(cf);
            }
            Ok(None) => selected.push(file),
            Err(e) => {
                stats.files_failed += 1;
                tracing::warn!(
                    file = %file.path,
                    error = %e,
                    "skipping file during semantic index"
                );
            }
        }
    }

    if selected.is_empty() {
        return Ok(());
    }

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

    // Chunk-level dedup: a saved file usually changes a few chunks, and the
    // stored content IS the embedded text (header included), so any chunk
    // whose text already has a vector in this table keeps it. Only the rest
    // go to the model — for a watcher re-embedding an edited 300-chunk file
    // that is the difference between 300 GPU embeddings and three.
    let mut all_embeddings: Vec<Option<Vec<f32>>> = Vec::new();
    let mut to_embed: Vec<&str> = Vec::new();
    let mut to_embed_slots: Vec<usize> = Vec::new();
    for cf in &chunked {
        let existing: HashMap<String, Vec<f32>> = db
            .chunk_embeddings_for_file(&cf.path)?
            .into_iter()
            .collect();
        for (text, _, _) in &cf.chunks {
            match existing.get(text) {
                Some(vector) => {
                    stats.chunks_reused += 1;
                    all_embeddings.push(Some(vector.clone()));
                }
                None => {
                    to_embed_slots.push(all_embeddings.len());
                    to_embed.push(text.as_str());
                    all_embeddings.push(None);
                }
            }
        }
    }
    if !to_embed.is_empty() {
        let fresh = embedder.embed_batch(&to_embed)?;
        ensure!(
            fresh.len() == to_embed.len(),
            "embedder returned {} vectors for {} texts",
            fresh.len(),
            to_embed.len()
        );
        for (slot, vector) in to_embed_slots.into_iter().zip(fresh) {
            all_embeddings[slot] = Some(vector);
        }
    }
    let all_embeddings: Vec<Vec<f32>> = all_embeddings
        .into_iter()
        .map(|v| v.expect("every slot filled by reuse or embedding"))
        .collect();
    write_semantic_updates(db, &selected, &chunked, &all_embeddings, stats)?;
    Ok(())
}

fn semantic_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
    verbose: bool,
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

    if to_index.is_empty() {
        return Ok(stats);
    }

    if verbose {
        tracing::info!(files_to_embed = to_index.len(), "semantic indexing");
    }
    embedder.prepare(to_index.len())?;

    let n_batches = to_index.len().div_ceil(COMMIT_BATCH_SIZE);
    for (i, batch) in to_index.chunks(COMMIT_BATCH_SIZE).enumerate() {
        if verbose {
            let file_count = batch.len();
            let start = i * COMMIT_BATCH_SIZE + 1;
            let end = (start + file_count - 1).min(to_index.len());
            tracing::info!(
                batch = i + 1,
                total_batches = n_batches,
                files = file_count,
                range = format!("{start}-{end}"),
                "embedding batch"
            );
        }
        process_semantic_batch(root, db, embedder, &config, batch, &mut stats)?;
    }
    Ok(stats)
}

pub fn semantic_full_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    verbose: bool,
) -> Result<SemanticIndexStats> {
    semantic_index(
        root,
        db,
        embedder,
        exclude_patterns,
        IndexStrategy::Full,
        verbose,
    )
}

pub fn semantic_incremental_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    verbose: bool,
) -> Result<SemanticIndexStats> {
    semantic_index(
        root,
        db,
        embedder,
        exclude_patterns,
        IndexStrategy::Incremental,
        verbose,
    )
}

pub fn semantic_index_files(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    files: &[FileInfo],
    verbose: bool,
) -> Result<SemanticIndexStats> {
    let config = ChunkConfig::default();
    let mut stats = SemanticIndexStats::default();

    if files.is_empty() {
        return Ok(stats);
    }

    if verbose {
        tracing::info!(count = files.len(), "semantic indexing specific files");
    }
    embedder.prepare(files.len())?;

    let file_refs: Vec<&FileInfo> = files.iter().collect();
    let n_batches = file_refs.len().div_ceil(COMMIT_BATCH_SIZE);
    for (i, batch) in file_refs.chunks(COMMIT_BATCH_SIZE).enumerate() {
        if verbose {
            let file_count = batch.len();
            tracing::info!(
                batch = i + 1,
                total_batches = n_batches,
                files = file_count,
                "semantic per-file batch"
            );
        }
        process_semantic_batch(root, db, embedder, &config, batch, &mut stats)?;
    }
    Ok(stats)
}

pub fn semantic_remove_files(db: &Database, paths: &[String]) -> Result<usize> {
    let mut removed = 0;
    db.execute_batch(|db| {
        for path in paths {
            db.delete_chunks_for_file(path)?;
            db.delete_semantic_file_hash(path)?;
            removed += 1;
        }
        Ok(())
    })?;
    Ok(removed)
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

    struct FakeEmbedder {
        prepared_with: Vec<usize>,
        batches: usize,
    }

    impl TextEmbedder for FakeEmbedder {
        fn prepare(&mut self, files_to_embed: usize) -> Result<()> {
            self.prepared_with.push(files_to_embed);
            Ok(())
        }

        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.batches += 1;
            Ok(texts.iter().map(|_| embedding(0.5)).collect())
        }
    }

    /// Records every construction on a shared counter so a test can assert
    /// the constructor never ran.
    fn counting_lazy(
        constructions: std::sync::Arc<std::sync::Mutex<Vec<Option<usize>>>>,
    ) -> LazyEmbedder {
        LazyEmbedder::new(Box::new(move |n| {
            constructions.lock().unwrap().push(n);
            Ok(Box::new(FakeEmbedder {
                prepared_with: Vec::new(),
                batches: 0,
            }) as Box<dyn TextEmbedder>)
        }))
    }

    #[test]
    fn no_change_incremental_pass_never_constructs_the_embedder() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], false).unwrap();

        assert_eq!(stats.files_processed, 0);
        assert!(constructions.lock().unwrap().is_empty());
        assert!(!lazy.is_loaded());
    }

    #[test]
    fn unchanged_files_never_construct_the_embedder() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = discover_files_with_excludes(root.path(), &[]).unwrap();
        assert_eq!(files.len(), 1);
        db.upsert_semantic_file_hash(&files[0].path, &files[0].content_hash)
            .unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], false).unwrap();

        assert_eq!(stats.files_skipped, 1);
        assert!(constructions.lock().unwrap().is_empty());
    }

    #[test]
    fn changed_file_constructs_the_embedder_once_with_the_file_count() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], false).unwrap();

        assert_eq!(stats.files_processed, 2);
        assert_eq!(stats.chunks_created, 2);
        assert_eq!(stats.chunks_reused, 0);
        assert_eq!(*constructions.lock().unwrap(), vec![Some(2)]);
        assert!(lazy.is_loaded());
    }

    #[test]
    fn edited_file_reembeds_only_the_chunks_whose_text_changed() {
        let root = tempfile::tempdir().unwrap();
        // Two paragraphs far enough apart to land in separate chunks.
        let first = "// first\n".repeat(120);
        let second = "// second\n".repeat(120);
        std::fs::write(root.path().join("a.rs"), format!("{first}\n\n{second}")).unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };

        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], false).unwrap();
        let initial_chunks = stats.chunks_created;
        assert!(initial_chunks >= 2, "fixture must chunk into >= 2 pieces");
        assert_eq!(stats.chunks_reused, 0);
        assert_eq!(fake.batches, 1);

        // Edit the second paragraph only; the first chunk's text is unchanged.
        let edited_second = "// edited\n".repeat(120);
        std::fs::write(
            root.path().join("a.rs"),
            format!("{first}\n\n{edited_second}"),
        )
        .unwrap();
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], false).unwrap();

        assert_eq!(stats.files_processed, 1);
        assert!(
            stats.chunks_reused >= 1,
            "unchanged chunk must be reused: {stats:?}"
        );
        assert!(
            stats.chunks_reused < stats.chunks_created,
            "edited chunk must be re-embedded: {stats:?}"
        );
        assert_eq!(fake.batches, 2);
        assert_eq!(
            db.chunks_for_file("a.rs").unwrap().len(),
            stats.chunks_created,
            "the file's rows are rewritten as one set"
        );
    }

    #[test]
    fn touched_file_with_identical_chunks_never_constructs_the_embedder() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], false).unwrap();
        assert_eq!(fake.batches, 1);

        // Force the file back into the selection with a stale semantic hash
        // while its chunk text stays byte-identical (a touch, a revert, a
        // mode change): every chunk is reused and no backend is ever built.
        db.upsert_semantic_file_hash("a.rs", "stale").unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], false).unwrap();

        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_reused, stats.chunks_created);
        assert!(constructions.lock().unwrap().is_empty());
        assert!(!lazy.is_loaded());
    }

    #[test]
    fn lazy_embedder_failure_does_not_retry_construction() {
        let mut lazy = LazyEmbedder::new(Box::new(|_| anyhow::bail!("no model")));
        lazy.prepare(1).expect("prepare only records the count");
        assert!(!lazy.is_loaded());
        let first = lazy.embed_batch(&["x"]).unwrap_err().to_string();
        let second = lazy.embed_batch(&["x"]).unwrap_err().to_string();
        assert_eq!(first, "no model");
        assert!(second.contains("already failed"), "{second}");
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
    fn chunk_one_reports_read_failures() {
        let root = tempfile::tempdir().unwrap();
        let info = file("missing.rs", "h");

        let err = chunk_one(root.path(), &info, &ChunkConfig::default()).unwrap_err();

        assert!(
            err.to_string()
                .contains("reading missing.rs for semantic chunks"),
            "unexpected error: {err:#}"
        );
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
    fn semantic_update_commits_in_chunks() {
        let db = Database::open_in_memory().unwrap();
        let files: Vec<FileInfo> = (0..5).map(|i| file(&format!("f{i}.rs"), "h")).collect();
        let selected: Vec<&FileInfo> = files.iter().collect();
        let chunked: Vec<ChunkedFile> = files
            .iter()
            .map(|f| ChunkedFile {
                path: f.path.clone(),
                language: "rust".to_string(),
                chunks: vec![(format!("// {}", f.path), 1, 1)],
            })
            .collect();
        let embeddings: Vec<Vec<f32>> = (0..5).map(|i| embedding(0.1 * (i + 1) as f32)).collect();
        let mut stats = SemanticIndexStats::default();

        // batch_size=2 forces 3 separate transactions (2 + 2 + 1).
        write_semantic_updates_with_batch(&db, &selected, &chunked, &embeddings, &mut stats, 2)
            .unwrap();

        let hashes = db.all_semantic_file_hashes().unwrap();
        assert_eq!(hashes.len(), 5);
        for f in &files {
            assert_eq!(hashes.get(&f.path).map(String::as_str), Some("h"));
            assert_eq!(db.chunks_for_file(&f.path).unwrap().len(), 1);
        }
        assert_eq!(stats.files_processed, 5);
        assert_eq!(stats.chunks_created, 5);
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
