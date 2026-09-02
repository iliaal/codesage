use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use codesage_embed::chunk::{ChunkConfig, chunk_text};
use codesage_embed::config::EmbeddingConfig;
pub use codesage_embed::fingerprint::SemanticFingerprint;
use codesage_embed::model::{
    Embedder, ModelArtifacts, cached_model_artifacts, resolve_model_artifacts,
};
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
use codesage_storage::{Database, SemanticAttestation};
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
    reuse_stored: bool,
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
    // that is the difference between 300 GPU embeddings and three. The
    // caller decides whether stored vectors are trustworthy at all (see
    // `stored_vectors_reusable`); with `reuse_stored` false every chunk is
    // embedded afresh.
    let mut all_embeddings: Vec<Option<Vec<f32>>> = Vec::new();
    let mut to_embed: Vec<&str> = Vec::new();
    let mut to_embed_slots: Vec<usize> = Vec::new();
    for cf in &chunked {
        let existing: HashMap<String, Vec<f32>> = if reuse_stored {
            db.chunk_embeddings_for_file(&cf.path)?
                .into_iter()
                .collect()
        } else {
            HashMap::new()
        };
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

/// How the chunk table's recorded fingerprint relates to the one this run
/// would produce. Anything but [`SemanticTableState::Current`] means the
/// table's vectors cannot be vouched for: a reader must not serve them as
/// current and a writer must not reuse them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticTableState {
    /// The table records exactly this run's fingerprint.
    Current,
    /// The table records no fingerprint: it predates the column, or no
    /// completed population has attested it yet.
    Unrecorded,
    /// The table records a different fingerprint.
    Mismatch { stored: String },
}

impl SemanticTableState {
    pub fn is_current(&self) -> bool {
        matches!(self, Self::Current)
    }
}

/// Compare the table's recorded fingerprint with `fingerprint`. An absent
/// record is [`SemanticTableState::Unrecorded`], never a match.
pub fn semantic_table_state(
    db: &Database,
    fingerprint: &SemanticFingerprint,
) -> Result<SemanticTableState> {
    Ok(match db.semantic_fingerprint()? {
        Some(stored) if stored == fingerprint.as_str() => SemanticTableState::Current,
        Some(stored) => SemanticTableState::Mismatch { stored },
        None => SemanticTableState::Unrecorded,
    })
}

/// Where [`resolve_semantic_fingerprint`] may look for the model files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactLookup {
    /// The local hf-hub cache first, then the same resolution the session
    /// loader performs — which downloads on a cache miss.
    Resolve,
    /// The local hf-hub cache only. Absent artifacts are `None`, never a
    /// download: for `status` and any other path that must not block on the
    /// network.
    CachedOnly,
}

/// The fingerprint the table should be compared against, derived without
/// reading a model file whenever the recorded attestation allows it.
///
/// A model file is hundreds of megabytes, and every semantic command used to
/// digest it before deciding anything — a no-change `codesage index` from
/// the git hook paid ~2.6 s to learn it had nothing to do. The completed pass
/// that attested the table recorded the digest it used and the stat key
/// (paths, sizes, mtimes) of the files it digested. When the current files
/// stat to the same key and the fingerprint rebuilt over the recorded digest
/// is the recorded text, that text is the answer and nothing is read. Any
/// other case — no attestation, a stat key that moved, a fingerprint that
/// differs on a non-artifact component — digests the files for real. The
/// stat key's blind spot (a same-length, same-nanosecond rewrite) is the one
/// the per-process cache already accepted.
///
/// `Ok(None)` only under [`ArtifactLookup::CachedOnly`] with the artifacts
/// not in the cache.
pub fn resolve_semantic_fingerprint(
    db: &Database,
    config: &EmbeddingConfig,
    dim: usize,
    lookup: ArtifactLookup,
) -> Result<Option<SemanticFingerprint>> {
    let artifacts = match (cached_model_artifacts(&config.model), lookup) {
        (Some(artifacts), _) => artifacts,
        (None, ArtifactLookup::CachedOnly) => return Ok(None),
        (None, ArtifactLookup::Resolve) => resolve_model_artifacts(&config.model)
            .with_context(|| format!("resolving model files for {:?}", config.model))?,
    };
    resolve_semantic_fingerprint_for_artifacts(db, config, dim, &artifacts).map(Some)
}

/// [`resolve_semantic_fingerprint`] over an explicit artifact set.
pub fn resolve_semantic_fingerprint_for_artifacts(
    db: &Database,
    config: &EmbeddingConfig,
    dim: usize,
    artifacts: &ModelArtifacts,
) -> Result<SemanticFingerprint> {
    if let Some(stat_key) = artifacts.stat_key()
        && let Some(attestation) = db.semantic_attestation()?
        && attestation.artifact_stat_key.as_deref() == Some(stat_key.as_str())
        && let Some(digest) = &attestation.artifact_digest
    {
        let candidate = SemanticFingerprint::with_attested_digest(config, dim, digest, &stat_key);
        if candidate.as_str() == attestation.fingerprint {
            return Ok(candidate);
        }
    }
    SemanticFingerprint::for_artifacts(config, dim, artifacts)
}

/// The chunk table cannot serve a query: its vectors were produced under an
/// unknown or different setup. Typed so a CLI can map it to its own exit
/// status rather than the generic failure.
#[derive(Debug)]
pub struct StaleSemanticTable {
    pub state: SemanticTableState,
    pub current: String,
}

impl std::fmt::Display for StaleSemanticTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.state {
            SemanticTableState::Current => write!(f, "semantic index is current"),
            SemanticTableState::Unrecorded => write!(
                f,
                "semantic index records no fingerprint (never fully embedded under this \
                 CodeSage version, or a rebuild did not complete); run `codesage index --full`"
            ),
            SemanticTableState::Mismatch { stored } => write!(
                f,
                "semantic index was embedded under a different setup (stored {stored}, \
                 current {}); run `codesage index --full`",
                self.current
            ),
        }
    }
}

impl std::error::Error for StaleSemanticTable {}

/// Refuse to read a chunk table whose fingerprint is absent or differs from
/// `fingerprint`. Returns a [`StaleSemanticTable`] error naming the repair.
pub fn require_current_semantic_table(
    db: &Database,
    fingerprint: &SemanticFingerprint,
) -> Result<()> {
    match semantic_table_state(db, fingerprint)? {
        SemanticTableState::Current => Ok(()),
        state => Err(StaleSemanticTable {
            state,
            current: fingerprint.as_str().to_string(),
        }
        .into()),
    }
}

/// Whether a stored vector may stand in for a fresh one on this pass.
///
/// A full rebuild never reuses: it is the command a user runs to repair
/// vectors that a same-name model revision or a pooling change left stale,
/// and the table name cannot tell those apart. An incremental pass reuses
/// only when the table records exactly the fingerprint this run would
/// produce; a different or absent record is "unknown", never "matches".
fn stored_vectors_reusable(strategy: IndexStrategy, table_state: &SemanticTableState) -> bool {
    if strategy == IndexStrategy::Full {
        return false;
    }
    match table_state {
        SemanticTableState::Current => true,
        SemanticTableState::Mismatch { stored } => {
            tracing::warn!(
                stored,
                "stored embeddings were produced under a different semantic fingerprint; \
                 reusing none of them"
            );
            false
        }
        SemanticTableState::Unrecorded => {
            tracing::debug!(
                "chunk table records no semantic fingerprint; reusing no stored vectors"
            );
            false
        }
    }
}

/// Record `fingerprint` as the identity of every vector in the table, but
/// only when the pass really did rewrite every row it was responsible for: a
/// file that failed to read keeps its previous rows, whose provenance this
/// run cannot vouch for.
fn record_fingerprint_if_complete(
    db: &Database,
    fingerprint: &SemanticFingerprint,
    stats: &SemanticIndexStats,
) -> Result<()> {
    if stats.files_failed > 0 {
        tracing::warn!(
            files_failed = stats.files_failed,
            "semantic fingerprint not recorded: some files kept rows this pass could not rewrite"
        );
        return Ok(());
    }
    db.record_semantic_attestation(&SemanticAttestation {
        fingerprint: fingerprint.as_str().to_string(),
        artifact_digest: Some(fingerprint.artifact_digest().to_string()),
        artifact_stat_key: fingerprint.artifact_stat_key().map(str::to_string),
    })
}

fn semantic_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    let files = discover_files_with_excludes(root, exclude_patterns)?;
    let config = ChunkConfig::default();
    let mut stats = SemanticIndexStats::default();
    let table_state = semantic_table_state(db, fingerprint)?;
    let reuse_stored = stored_vectors_reusable(strategy, &table_state);
    // A table whose fingerprint is absent or differs holds vectors this run
    // cannot vouch for in ANY file, not only the ones whose content moved:
    // an incremental pass over it re-embeds every file, as `--full` would.
    let stale_table = !table_state.is_current();
    if strategy == IndexStrategy::Full || stale_table {
        // Every row is about to be rewritten; until they all have been, the
        // table holds vectors no fingerprint describes. On a stale table the
        // OLD record must go before the first new row lands: a pass that
        // dies midway otherwise leaves this run's vectors attested under the
        // previous setup, and reverting the config to that setup would read
        // the mix as current.
        db.clear_semantic_fingerprint()?;
    }
    let selection = if stale_table {
        IndexStrategy::Full
    } else {
        strategy
    };
    if strategy == IndexStrategy::Incremental && stale_table {
        tracing::warn!(
            state = ?table_state,
            current = %fingerprint,
            "chunk table fingerprint is absent or differs; re-embedding every file"
        );
    }

    let discovered_paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let existing_chunk_paths = db.all_chunk_file_paths()?;
    let existing_semantic_hashes = db.all_semantic_file_hashes()?;
    // A full pass rewrites every row; so does a pass over a stale table, and
    // a first population of an empty table. Any of them may vouch for the
    // table's vectors afterwards.
    let records_fingerprint = selection == IndexStrategy::Full
        || (existing_chunk_paths.is_empty() && existing_semantic_hashes.is_empty());
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

    let to_index = select_semantic_files(&files, &existing_semantic_hashes, selection);

    if selection == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_index.len();
    }

    if to_index.is_empty() {
        if records_fingerprint {
            record_fingerprint_if_complete(db, fingerprint, &stats)?;
        }
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
        process_semantic_batch(root, db, embedder, &config, batch, reuse_stored, &mut stats)?;
    }
    if records_fingerprint {
        record_fingerprint_if_complete(db, fingerprint, &stats)?;
    }
    Ok(stats)
}

/// Re-embed every file. Never reuses a stored vector, and on completion
/// records `fingerprint` as the identity of the table's contents.
pub fn semantic_full_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    semantic_index(
        root,
        db,
        embedder,
        exclude_patterns,
        IndexStrategy::Full,
        fingerprint,
        verbose,
    )
}

/// Re-embed files whose content hash moved. Stored vectors of text-identical
/// chunks are reused only when the table records exactly `fingerprint`.
pub fn semantic_incremental_index(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    exclude_patterns: &[String],
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    semantic_index(
        root,
        db,
        embedder,
        exclude_patterns,
        IndexStrategy::Incremental,
        fingerprint,
        verbose,
    )
}

/// Re-embed exactly `files` (the watcher's path). Same reuse rule as the
/// incremental pass; never records the fingerprint, because it never sees
/// the whole table.
pub fn semantic_index_files(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    files: &[FileInfo],
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    let config = ChunkConfig::default();
    let mut stats = SemanticIndexStats::default();

    if files.is_empty() {
        return Ok(stats);
    }
    let table_state = semantic_table_state(db, fingerprint)?;
    let reuse_stored = stored_vectors_reusable(IndexStrategy::Incremental, &table_state);
    if !table_state.is_current() {
        // The rows about to be written are this setup's; the record, if any,
        // is another's. Forget it before the first write so no reader takes
        // the mix for that setup's table. Never re-recorded here: this pass
        // does not see the whole table.
        db.clear_semantic_fingerprint()?;
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
        process_semantic_batch(root, db, embedder, &config, batch, reuse_stored, &mut stats)?;
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
    use std::time::Duration;

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

    fn test_fp() -> SemanticFingerprint {
        SemanticFingerprint::with_artifact_digest(
            &codesage_embed::config::EmbeddingConfig::default(),
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            "artifact-digest-a",
        )
    }

    /// A fingerprint for the same table produced by a different setup
    /// (here: the pooling strategy changed under the same model name).
    fn other_fp() -> SemanticFingerprint {
        let mut config = codesage_embed::config::EmbeddingConfig::default();
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Cls);
        SemanticFingerprint::with_artifact_digest(
            &config,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            "artifact-digest-a",
        )
    }

    /// A scratch model on disk, so a test can rewrite a byte of it.
    fn scratch_model(dir: &Path) -> codesage_embed::model::ModelArtifacts {
        let tokenizer = dir.join("tokenizer.json");
        let onnx = dir.join("model.onnx");
        std::fs::write(&tokenizer, b"{}").unwrap();
        std::fs::write(&onnx, b"graph bytes v1").unwrap();
        codesage_embed::model::ModelArtifacts {
            tokenizer,
            onnx,
            onnx_data: None,
        }
    }

    fn fp_for(artifacts: &codesage_embed::model::ModelArtifacts) -> SemanticFingerprint {
        SemanticFingerprint::for_artifacts(
            &codesage_embed::config::EmbeddingConfig::default(),
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            artifacts,
        )
        .unwrap()
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

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &test_fp(), false)
            .unwrap();

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
        // The skip holds only for a table attested under this fingerprint;
        // an unattested table is stale and re-embeds everything.
        db.record_semantic_fingerprint(test_fp().as_str()).unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &test_fp(), false)
            .unwrap();

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

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &test_fp(), false)
            .unwrap();

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

        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
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
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();

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
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        assert_eq!(fake.batches, 1);

        // Force the file back into the selection with a stale semantic hash
        // while its chunk text stays byte-identical (a touch, a revert, a
        // mode change): every chunk is reused and no backend is ever built.
        db.upsert_semantic_file_hash("a.rs", "stale").unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &test_fp(), false)
            .unwrap();

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
    fn first_population_records_the_fingerprint() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.semantic_fingerprint().unwrap(), None);
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };

        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();

        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str())
        );
    }

    #[test]
    fn fingerprint_change_disables_stored_vector_reuse() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        assert_eq!(fake.batches, 1);

        // Same chunk text, same table, but the vectors in it were produced
        // under another pooling strategy: text identity proves nothing.
        db.upsert_semantic_file_hash("a.rs", "stale").unwrap();
        let stats =
            semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false)
                .unwrap();

        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_reused, 0, "{stats:?}");
        assert_eq!(stats.chunks_created, 1);
        assert_eq!(fake.batches, 2, "the chunk must go back to the model");
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(other_fp().as_str()),
            "a pass that rewrote every row under the new setup attests it"
        );
    }

    #[test]
    fn one_rewritten_model_byte_disables_stored_vector_reuse() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let model_dir = tempfile::tempdir().unwrap();
        let artifacts = scratch_model(model_dir.path());
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        let before = fp_for(&artifacts);
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &before, false).unwrap();
        assert_eq!(fake.batches, 1);
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(before.as_str())
        );

        // Same model name, same length, one byte of the graph rewritten in
        // place: the table's vectors are another model's output.
        let mut bytes = std::fs::read(&artifacts.onnx).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&artifacts.onnx, &bytes).unwrap();
        let later = std::fs::metadata(&artifacts.onnx)
            .unwrap()
            .modified()
            .unwrap()
            + std::time::Duration::from_secs(2);
        std::fs::File::open(&artifacts.onnx)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let after = fp_for(&artifacts);
        assert_ne!(before, after);

        // The file's content hash is unchanged, so a content-keyed pass would
        // skip it; a stale table re-embeds it instead, reusing nothing.
        let stats =
            semantic_incremental_index(root.path(), &db, &mut fake, &[], &after, false).unwrap();
        assert_eq!(stats.files_processed, 1, "{stats:?}");
        assert_eq!(stats.files_skipped, 0, "{stats:?}");
        assert_eq!(stats.chunks_reused, 0, "{stats:?}");
        assert_eq!(fake.batches, 2, "the chunk must go back to the model");
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(after.as_str())
        );
    }

    #[test]
    fn incremental_pass_over_a_stale_table_reembeds_unchanged_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();
        assert_eq!(fake.batches, 1);

        // Nothing on disk changed. Under the recorded fingerprint every file
        // would be skipped; under a different one none may be.
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
        assert_eq!(stats.files_processed, 2, "{stats:?}");
        assert_eq!(stats.files_skipped, 0, "{stats:?}");
        assert_eq!(stats.chunks_reused, 0, "{stats:?}");
        assert_eq!(fake.batches, 2);
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str())
        );

        // A table that records no fingerprint at all is stale the same way.
        db.clear_semantic_fingerprint().unwrap();
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
        assert_eq!(stats.files_processed, 2, "{stats:?}");
        assert_eq!(stats.files_skipped, 0, "{stats:?}");
        assert_eq!(fake.batches, 3);

        // Current again: the ordinary content-keyed skip applies.
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
        assert_eq!(stats.files_skipped, 2, "{stats:?}");
        assert_eq!(fake.batches, 3);
    }

    #[test]
    fn require_current_semantic_table_refuses_unrecorded_and_mismatched() {
        let db = Database::open_in_memory().unwrap();
        let err = require_current_semantic_table(&db, &test_fp()).unwrap_err();
        let stale = err
            .downcast_ref::<StaleSemanticTable>()
            .expect("typed refusal");
        assert_eq!(stale.state, SemanticTableState::Unrecorded);
        assert!(err.to_string().contains("codesage index --full"), "{err}");

        db.record_semantic_fingerprint(other_fp().as_str()).unwrap();
        let err = require_current_semantic_table(&db, &test_fp()).unwrap_err();
        let stale = err
            .downcast_ref::<StaleSemanticTable>()
            .expect("typed refusal");
        assert_eq!(
            stale.state,
            SemanticTableState::Mismatch {
                stored: other_fp().as_str().to_string()
            }
        );
        assert!(err.to_string().contains("codesage index --full"), "{err}");

        db.record_semantic_fingerprint(test_fp().as_str()).unwrap();
        require_current_semantic_table(&db, &test_fp()).unwrap();
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    #[test]
    fn full_rebuild_never_reuses_stored_vectors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        assert_eq!(fake.batches, 1);

        // Nothing changed, the fingerprint matches — and --full still
        // re-embeds every chunk, because a full rebuild is how a user repairs
        // vectors the fingerprint cannot see (a same-name model revision).
        let stats =
            semantic_full_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();

        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_reused, 0, "{stats:?}");
        assert_eq!(fake.batches, 2);
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(other_fp().as_str()),
            "a completed full pass records the fingerprint it embedded under"
        );
    }

    #[test]
    fn full_rebuild_with_a_failed_file_does_not_attest_the_table() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        // Non-UTF-8 bytes make `read_to_string` fail for this file only.
        std::fs::write(root.path().join("b.rs"), b"fn b() {}\n\xff\xfe").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };

        let stats =
            semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();

        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(
            db.semantic_fingerprint().unwrap(),
            None,
            "rows this pass could not rewrite have unknown provenance"
        );
    }

    #[test]
    fn partial_full_rebuild_clears_the_previous_attestation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str())
        );

        // A second file that cannot be read makes the next --full partial:
        // a.rs holds new vectors, b.rs none, and the old attestation must not
        // survive to describe that mix.
        std::fs::write(root.path().join("b.rs"), b"fn b() {}\n\xff\xfe").unwrap();
        let stats =
            semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(stats.files_processed, 1, "{stats:?}");
        assert_eq!(
            db.semantic_fingerprint().unwrap(),
            None,
            "a partial --full leaves the table unattested"
        );
        assert!(!semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    /// Fails every `embed_batch` call from `fail_on_call` onwards: a process
    /// dying midway through a pass, as far as the table can tell.
    struct CrashingEmbedder {
        calls: usize,
        fail_on_call: usize,
    }

    impl TextEmbedder for CrashingEmbedder {
        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.calls += 1;
            if self.calls >= self.fail_on_call {
                anyhow::bail!("simulated crash on embed call {}", self.calls);
            }
            Ok(texts.iter().map(|_| embedding(0.25)).collect())
        }
    }

    fn resolved_fp(
        db: &Database,
        artifacts: &codesage_embed::model::ModelArtifacts,
    ) -> SemanticFingerprint {
        resolve_semantic_fingerprint_for_artifacts(
            db,
            &codesage_embed::config::EmbeddingConfig::default(),
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            artifacts,
        )
        .unwrap()
    }

    #[test]
    fn no_change_incremental_pass_over_a_current_table_reads_no_model_file() {
        use codesage_embed::fingerprint::{artifact_read_count, forget_cached_digests};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let model = tempfile::tempdir().unwrap();
        let artifacts = scratch_model(model.path());
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };

        // First population: no attestation yet, so the files are digested.
        let reads_before = artifact_read_count(&artifacts.onnx);
        let first = resolved_fp(&db, &artifacts);
        assert_eq!(artifact_read_count(&artifacts.onnx), reads_before + 1);
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &first, false).unwrap();
        let attestation = db.semantic_attestation().unwrap().unwrap();
        assert_eq!(attestation.fingerprint, first.as_str());
        assert_eq!(
            attestation.artifact_digest.as_deref(),
            Some(first.artifact_digest())
        );
        assert_eq!(
            attestation.artifact_stat_key.as_deref(),
            artifacts.stat_key().as_deref()
        );

        // Start the way a new process would: with no digest in memory. Only
        // the recorded attestation can now answer without a read.
        forget_cached_digests();
        let original = std::fs::metadata(&artifacts.onnx)
            .unwrap()
            .modified()
            .unwrap();
        let file = std::fs::File::open(&artifacts.onnx).unwrap();

        // No-change pass over a current table: the stat key matches, the
        // recorded digest is reused, and not one artifact byte is read.
        let reads_before = artifact_read_count(&artifacts.onnx);
        let second = resolved_fp(&db, &artifacts);
        assert_eq!(second, first);
        assert_eq!(second.artifact_stat_key(), first.artifact_stat_key());
        let stats =
            semantic_incremental_index(root.path(), &db, &mut fake, &[], &second, false).unwrap();
        assert_eq!(stats.files_skipped, 1, "{stats:?}");
        assert_eq!(
            artifact_read_count(&artifacts.onnx),
            reads_before,
            "a no-change pass over a current table must not read the model"
        );
        assert_eq!(artifact_read_count(&artifacts.tokenizer), reads_before);

        // A moved stat key (same bytes, new mtime on the graph) triggers
        // exactly one digest per artifact (the cache is cold for both), and
        // the table is still current.
        let tokenizer_reads = artifact_read_count(&artifacts.tokenizer);
        file.set_modified(original + Duration::from_secs(60))
            .unwrap();
        let third = resolved_fp(&db, &artifacts);
        assert_eq!(third, first, "same bytes, same fingerprint");
        assert_eq!(artifact_read_count(&artifacts.onnx), reads_before + 1);
        assert_eq!(
            artifact_read_count(&artifacts.tokenizer),
            tokenizer_reads + 1
        );
        assert!(semantic_table_state(&db, &third).unwrap().is_current());

        // A recorded stat key that matches but a fingerprint that differs
        // on another component (pooling) is not taken on trust: the files
        // are digested for real and the table reads as mismatched.
        file.set_modified(original).unwrap();
        let cls = codesage_embed::config::EmbeddingConfig {
            pooling: Some(codesage_embed::config::PoolingStrategy::Cls),
            ..Default::default()
        };
        let reads_before = artifact_read_count(&artifacts.onnx);
        let fourth = resolve_semantic_fingerprint_for_artifacts(
            &db,
            &cls,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            &artifacts,
        )
        .unwrap();
        assert_ne!(fourth, first);
        assert!(!semantic_table_state(&db, &fourth).unwrap().is_current());
        assert_eq!(artifact_read_count(&artifacts.onnx), reads_before + 1);
    }

    #[test]
    fn a_crash_midway_through_a_stale_incremental_pass_leaves_the_table_unattested() {
        let root = tempfile::tempdir().unwrap();
        // More files than one commit batch, so the first batch lands before
        // the second fails.
        for i in 0..(COMMIT_BATCH_SIZE + 1) {
            std::fs::write(
                root.path().join(format!("f{i:03}.rs")),
                format!("fn f{i}() {{ let x = {i}; }}\n"),
            )
            .unwrap();
        }
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        // The table is complete and attested under setup A.
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(other_fp().as_str())
        );
        let rows_under_a = db.chunk_count().unwrap();
        assert!(rows_under_a > COMMIT_BATCH_SIZE);

        // Setup B runs an ordinary incremental pass — which the mismatch
        // turns into a full re-embed — and dies after the first batch.
        let mut crashing = CrashingEmbedder {
            calls: 0,
            fail_on_call: 2,
        };
        let err =
            semantic_incremental_index(root.path(), &db, &mut crashing, &[], &test_fp(), false)
                .unwrap_err();
        assert!(err.to_string().contains("simulated crash"), "{err:#}");
        assert_eq!(crashing.calls, 2);
        assert_eq!(
            db.chunk_count().unwrap(),
            rows_under_a,
            "the first batch was rewritten under B, the rest still hold A's rows"
        );

        // Neither setup may read the mix as its own table: A's record is
        // gone before B's first write, and B never completed.
        assert_eq!(db.semantic_fingerprint().unwrap(), None);
        let under_a = require_current_semantic_table(&db, &other_fp()).unwrap_err();
        assert!(
            under_a.downcast_ref::<StaleSemanticTable>().is_some(),
            "reverting the config to A must not make the mixed table current: {under_a:#}"
        );
        assert!(
            require_current_semantic_table(&db, &test_fp())
                .unwrap_err()
                .downcast_ref::<StaleSemanticTable>()
                .is_some()
        );
    }

    #[test]
    fn per_file_pass_over_a_mismatched_table_forgets_the_record_before_writing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();
        let files = discover_files_with_excludes(root.path(), &[]).unwrap();
        let only_a: Vec<FileInfo> = files.into_iter().filter(|f| f.path == "a.rs").collect();

        // The watcher re-embeds one file under B while the table records A.
        semantic_index_files(root.path(), &db, &mut fake, &only_a, &test_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap(),
            None,
            "a.rs holds B's vectors and b.rs holds A's; no record may describe that"
        );
        assert!(require_current_semantic_table(&db, &other_fp()).is_err());
        assert!(require_current_semantic_table(&db, &test_fp()).is_err());

        // Under a current table the record survives a per-file pass.
        semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        semantic_index_files(root.path(), &db, &mut fake, &only_a, &test_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str())
        );
    }

    #[test]
    fn per_file_pass_reuses_only_under_a_matching_fingerprint() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        let files = discover_files_with_excludes(root.path(), &[]).unwrap();

        let reused =
            semantic_index_files(root.path(), &db, &mut fake, &files, &test_fp(), false).unwrap();
        assert_eq!(reused.chunks_reused, 1, "{reused:?}");
        assert_eq!(fake.batches, 1);

        let fresh =
            semantic_index_files(root.path(), &db, &mut fake, &files, &other_fp(), false).unwrap();
        assert_eq!(fresh.chunks_reused, 0, "{fresh:?}");
        assert_eq!(fake.batches, 2);
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
