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

/// Embedding backend shared by in-process, lazy, and daemon-backed execution.
pub trait TextEmbedder {
    /// Called once per nonempty indexing pass, before embedding, with the file count.
    fn prepare(&mut self, _files_to_embed: usize) -> Result<()> {
        Ok(())
    }

    /// Called once per nonempty indexing pass, after prepare and before embedding.
    /// Reject a known identity mismatch before writing incorrectly attested vectors.
    fn bind_fingerprint(&mut self, _expected: &SemanticFingerprint) -> Result<()> {
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
    /// CPU fallback must never be attested as the configured CUDA provider.
    fn bind_fingerprint(&mut self, expected: &SemanticFingerprint) -> Result<()> {
        let actual = self.execution_provider();
        ensure!(
            expected.execution_provider() == actual,
            "the embedding session runs on the {actual} execution provider but this pass \
             fingerprints its vectors as {:?}; set `device` in .codesage/config.toml to the \
             provider that actually runs, or make the configured one available",
            expected.execution_provider()
        );
        Ok(())
    }

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

/// Construct the backend only when a chunk needs embedding; fully reused passes
/// avoid loading ONNX/CUDA. prepare records the file count without constructing it.
pub struct LazyEmbedder {
    inner: Option<Box<dyn TextEmbedder>>,
    init: Option<EmbedderInit>,
    announced_files: Option<usize>,
    /// The fingerprint bound before the backend existed, forwarded to it
    /// the moment it is constructed.
    bound: Option<SemanticFingerprint>,
}

impl LazyEmbedder {
    pub fn new(init: EmbedderInit) -> Self {
        Self {
            inner: None,
            init: Some(init),
            announced_files: None,
            bound: None,
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    fn ensure(&mut self, files_to_embed: Option<usize>) -> Result<&mut dyn TextEmbedder> {
        if self.inner.is_none() {
            let init = self
                .init
                .take()
                .ok_or_else(|| anyhow::anyhow!("embedder construction already failed once"))?;
            let mut inner = init(files_to_embed)?;
            if let Some(expected) = &self.bound {
                inner.bind_fingerprint(expected)?;
            }
            self.inner = Some(inner);
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

    fn bind_fingerprint(&mut self, expected: &SemanticFingerprint) -> Result<()> {
        self.bound = Some(expected.clone());
        match self.inner.as_deref_mut() {
            Some(inner) => inner.bind_fingerprint(expected),
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

#[cfg(test)]
use codesage_parser::discover::discover_files_with_excludes;
use codesage_parser::discover::{DiscoveryReport, discover_files_report_with_cache};

#[derive(Debug)]
struct ChunkedFile {
    path: String,
    language: String,
    chunks: Vec<(String, u32, u32)>,
}

fn chunk_one(root: &Path, f: &FileInfo, config: &ChunkConfig) -> Result<Option<ChunkedFile>> {
    let abs = root.join(&f.path);
    let bytes =
        std::fs::read(&abs).with_context(|| format!("reading {} for semantic chunks", f.path))?;
    // Match structural parsing: invalid UTF-8 replaces characters rather than dropping files.
    let content = String::from_utf8_lossy(&bytes);
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

/// Bound uncommitted work without paying transaction overhead for every file.
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

#[derive(Clone, Copy)]
struct BatchPolicy {
    /// Stored vectors of text-identical chunks may be kept (see
    /// `stored_vectors_reusable`).
    reuse_stored: bool,
    /// A file this pass cannot read loses its rows and hash. True only for a
    /// pass that rewrites the whole table and will attest it afterwards.
    purge_failed: bool,
}

fn process_semantic_batch(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    config: &ChunkConfig,
    batch: &[&FileInfo],
    policy: BatchPolicy,
    stats: &mut SemanticIndexStats,
) -> Result<()> {
    let BatchPolicy {
        reuse_stored,
        purge_failed,
    } = policy;
    let chunk_results: Vec<(&FileInfo, Result<Option<ChunkedFile>>)> = batch
        .par_iter()
        .map(|f| (*f, chunk_one(root, f, config)))
        .collect();
    let mut selected = Vec::with_capacity(batch.len());
    let mut chunked = Vec::new();
    let mut failed = Vec::new();
    for (file, result) in chunk_results {
        match result {
            Ok(Some(cf)) => {
                selected.push(file);
                chunked.push(cf);
            }
            Ok(None) => selected.push(file),
            Err(e) => {
                stats.files_failed += 1;
                stats.failed_paths.push(file.path.clone());
                failed.push(file);
                tracing::warn!(
                    file = %file.path,
                    error = %e,
                    "skipping file during semantic index"
                );
            }
        }
    }

    // A whole-table attestation cannot cover unreadable rows from another setup.
    // Drop their hashes too, so the next incremental pass retries those files.
    if purge_failed && !failed.is_empty() {
        db.execute_batch(|db| {
            for f in &failed {
                db.delete_chunks_for_file(&f.path)?;
                db.delete_semantic_file_hash(&f.path)?;
            }
            Ok(())
        })?;
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

    // Stored text includes the augmentation header: exact equality permits reuse
    // only when the caller has also validated the stored vectors' fingerprint.
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

/// Only Current permits serving or reusing stored vectors; unknown is stale.
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

/// Reuse the attested digest only when artifact paths/sizes/mtimes and the
/// reconstructed fingerprint match; otherwise digest the artifacts.
/// Same-size, same-mtime rewrites remain the stat key's accepted blind spot.
/// Returns None only for CachedOnly with unavailable artifacts.
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

/// Full rebuilds bypass reuse to repair undetected corruption. Incremental passes
/// reuse only under an exactly matching recorded fingerprint.
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

/// Record `fingerprint` as the identity of every vector in the table. A
/// failure after discovery purges its rows during a full rewrite (see
/// `process_semantic_batch`). Discovery failures retain prior rows; callers
/// must withhold attestation if those rows have an unknown or different
/// fingerprint.
fn record_fingerprint(
    db: &Database,
    fingerprint: &SemanticFingerprint,
    stats: &SemanticIndexStats,
) -> Result<()> {
    if stats.files_failed > 0 {
        tracing::warn!(
            files_failed = stats.files_failed,
            files = %summarize_paths(&stats.failed_paths, 10),
            "semantic fingerprint recorded; the named files could not be refreshed"
        );
    }
    db.record_semantic_attestation(&SemanticAttestation {
        fingerprint: fingerprint.as_str().to_string(),
        artifact_digest: Some(fingerprint.artifact_digest().to_string()),
        artifact_stat_key: fingerprint.artifact_stat_key().map(str::to_string),
    })
}

/// Bound failure logs to limit paths plus a count of omitted paths.
pub fn summarize_paths(paths: &[String], limit: usize) -> String {
    let shown: Vec<&str> = paths.iter().take(limit).map(String::as_str).collect();
    let mut out = shown.join(", ");
    if paths.len() > limit {
        out.push_str(&format!(" (+{} more)", paths.len() - limit));
    }
    out
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
    let cache = if strategy == IndexStrategy::Incremental {
        db.file_hash_cache()?
    } else {
        HashMap::new()
    };
    let discovery = discover_files_report_with_cache(root, exclude_patterns, &cache)?;
    db.replace_file_hash_cache(&discovery.hash_cache)?;
    semantic_index_discovery_report(
        root,
        db,
        embedder,
        &discovery,
        strategy,
        fingerprint,
        verbose,
    )
}

/// Inject files that discovery would omit, to test failures after discovery.
#[cfg(test)]
fn semantic_index_discovered(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    files: &[FileInfo],
    strategy: IndexStrategy,
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    semantic_index_discovery_report(
        root,
        db,
        embedder,
        &DiscoveryReport {
            files: files.to_vec(),
            failed_paths: Vec::new(),
            hash_cache: HashMap::new(),
            hashes_reused: 0,
            bytes_hashed: 0,
        },
        strategy,
        fingerprint,
        verbose,
    )
}

fn semantic_index_discovery_report(
    root: &Path,
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    discovery: &DiscoveryReport,
    strategy: IndexStrategy,
    fingerprint: &SemanticFingerprint,
    verbose: bool,
) -> Result<SemanticIndexStats> {
    let files = &discovery.files;
    let config = ChunkConfig::default();
    let mut stats = SemanticIndexStats {
        files_failed: discovery.failed_paths.len(),
        failed_paths: discovery.failed_paths.clone(),
        ..Default::default()
    };
    let table_state = semantic_table_state(db, fingerprint)?;
    let reuse_stored = stored_vectors_reusable(strategy, &table_state);
    // Stale vectors require rewriting even files whose content hashes match.
    let stale_table = !table_state.is_current();
    if strategy == IndexStrategy::Full || stale_table {
        // Clear before writing: an interrupted rebuild must not leave mixed vectors
        // attested under the old setup, even if the configuration is reverted.
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

    let discovered_paths: HashSet<&str> = files
        .iter()
        .map(|f| f.path.as_str())
        .chain(discovery.failed_paths.iter().map(String::as_str))
        .collect();
    let existing_chunk_paths = db.all_chunk_file_paths()?;
    let existing_semantic_hashes = db.all_semantic_file_hashes()?;
    // Discovery failures retain prior rows; stale retained rows prohibit attestation.
    let retained_stale_rows = stale_table
        && discovery.failed_paths.iter().any(|path| {
            existing_chunk_paths.contains(path) || existing_semantic_hashes.contains_key(path)
        });
    let records_fingerprint = !retained_stale_rows
        && (selection == IndexStrategy::Full
            || (existing_chunk_paths.is_empty() && existing_semantic_hashes.is_empty()));
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

    let to_index = select_semantic_files(files, &existing_semantic_hashes, selection);

    if selection == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_index.len();
    }

    if to_index.is_empty() {
        if records_fingerprint {
            record_fingerprint(db, fingerprint, &stats)?;
        }
        return Ok(stats);
    }

    if verbose {
        tracing::info!(files_to_embed = to_index.len(), "semantic indexing");
    }
    embedder.prepare(to_index.len())?;
    embedder.bind_fingerprint(fingerprint)?;
    let policy = BatchPolicy {
        reuse_stored,
        purge_failed: selection == IndexStrategy::Full,
    };

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
        process_semantic_batch(root, db, embedder, &config, batch, policy, &mut stats)?;
    }
    if records_fingerprint {
        record_fingerprint(db, fingerprint, &stats)?;
    }
    Ok(stats)
}

/// Re-embed every discovered file without reuse. Attest only when no stale rows
/// survive discovery failures.
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

/// Re-embed changed files under a matching fingerprint, reusing identical chunks.
/// A stale or absent fingerprint requires rewriting every discovered file.
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
        // A partial pass cannot attest mixed setups; invalidate before the first write.
        db.clear_semantic_fingerprint()?;
    }

    if verbose {
        tracing::info!(count = files.len(), "semantic indexing specific files");
    }
    embedder.prepare(files.len())?;
    embedder.bind_fingerprint(fingerprint)?;
    let policy = BatchPolicy {
        reuse_stored,
        purge_failed: false,
    };

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
        process_semantic_batch(root, db, embedder, &config, batch, policy, &mut stats)?;
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

    /// Same table/model, different pooling.
    fn other_fp() -> SemanticFingerprint {
        let mut config = codesage_embed::config::EmbeddingConfig::default();
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Cls);
        SemanticFingerprint::with_artifact_digest(
            &config,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
            "artifact-digest-a",
        )
    }

    fn scratch_model(dir: &Path) -> codesage_embed::model::ModelArtifacts {
        let tokenizer = dir.join("tokenizer.json");
        let onnx = dir.join("model.onnx");
        std::fs::write(&tokenizer, b"{}").unwrap();
        std::fs::write(&onnx, b"graph bytes v1").unwrap();
        codesage_embed::model::ModelArtifacts {
            tokenizer,
            onnx,
            onnx_data: None,
            ort_runtime: None,
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
        // Unattested tables re-embed even when file hashes match.
        db.record_semantic_fingerprint(test_fp().as_str()).unwrap();
        let constructions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut lazy = counting_lazy(constructions.clone());

        let stats = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &test_fp(), false)
            .unwrap();

        assert_eq!(stats.files_skipped, 1);
        assert!(constructions.lock().unwrap().is_empty());
    }

    #[test]
    fn interpretation_upgrade_refreshes_semantic_headers_without_content_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.py"), "def actual(): pass\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = discover_files_with_excludes(root.path(), &[]).unwrap();
        db.upsert_file(&files[0]).unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        let before = db.chunk_embeddings_for_file("a.py").unwrap()[0].0.clone();
        assert!(!before.contains("# actual (function)"));

        crate::index::incremental_index(root.path(), &db, &[], false).unwrap();
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.chunks_created, 1);
        assert_eq!(fake.batches, 2);
        let after = db.chunk_embeddings_for_file("a.py").unwrap()[0].0.clone();
        assert!(after.contains("# actual (function)"), "{after}");
        assert_eq!(
            db.all_semantic_file_hashes().unwrap()["a.py"],
            files[0].content_hash
        );
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

        // Select the file despite unchanged chunk text to exercise vector reuse.
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

    /// A backend that runs on `provider` and refuses any other identity,
    /// as [`Embedder`] does with its session's execution provider.
    struct ProviderBoundEmbedder {
        provider: &'static str,
        bound_with: Vec<String>,
        batches: usize,
    }

    impl TextEmbedder for ProviderBoundEmbedder {
        fn bind_fingerprint(&mut self, expected: &SemanticFingerprint) -> Result<()> {
            self.bound_with
                .push(expected.execution_provider().to_string());
            ensure!(
                expected.execution_provider() == self.provider,
                "session runs on {} but the pass fingerprints {}",
                self.provider,
                expected.execution_provider()
            );
            Ok(())
        }

        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.batches += 1;
            Ok(texts.iter().map(|_| embedding(0.5)).collect())
        }
    }

    #[test]
    fn a_backend_on_another_provider_aborts_the_pass_before_any_row() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let cuda_fp = test_fp().with_execution_provider("cuda");
        let mut cpu = ProviderBoundEmbedder {
            provider: "cpu",
            bound_with: Vec::new(),
            batches: 0,
        };

        let err = semantic_incremental_index(root.path(), &db, &mut cpu, &[], &cuda_fp, false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("session runs on cpu"), "{err}");
        assert_eq!(cpu.bound_with, vec!["cuda".to_string()]);
        assert_eq!(cpu.batches, 0, "no text reached the backend");
        assert!(
            db.all_chunk_file_paths().unwrap().is_empty(),
            "no row written"
        );
        assert_eq!(db.semantic_fingerprint().unwrap(), None, "nothing attested");

        let files = discover_files_with_excludes(root.path(), &[]).unwrap();
        let err = semantic_index_files(root.path(), &db, &mut cpu, &files, &cuda_fp, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("session runs on cpu"), "{err}");
        assert_eq!(cpu.batches, 0);

        let cpu_fp = cuda_fp.with_execution_provider("cpu");
        semantic_incremental_index(root.path(), &db, &mut cpu, &[], &cpu_fp, false).unwrap();
        assert_eq!(cpu.batches, 1);
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(cpu_fp.as_str())
        );
    }

    #[test]
    fn lazy_embedder_forwards_the_bound_fingerprint_to_the_constructed_backend() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let cuda_fp = test_fp().with_execution_provider("cuda");
        let mut lazy = LazyEmbedder::new(Box::new(|_| {
            Ok(Box::new(ProviderBoundEmbedder {
                provider: "cpu",
                bound_with: Vec::new(),
                batches: 0,
            }) as Box<dyn TextEmbedder>)
        }));

        let err = semantic_incremental_index(root.path(), &db, &mut lazy, &[], &cuda_fp, false)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("session runs on cpu"),
            "the bind must reach the backend constructed after it: {err}"
        );
        assert!(db.all_chunk_file_paths().unwrap().is_empty());
        assert_eq!(db.semantic_fingerprint().unwrap(), None);
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

        // Change model bytes without changing its name or length.
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
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();
        assert_eq!(fake.batches, 1);

        #[cfg(unix)]
        assert_eq!(
            discover_files_report_with_cache(root.path(), &[], &db.file_hash_cache().unwrap())
                .unwrap()
                .hashes_reused,
            2
        );

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

        db.clear_semantic_fingerprint().unwrap();
        let stats = semantic_incremental_index(root.path(), &db, &mut fake, &[], &test_fp(), false)
            .unwrap();
        assert_eq!(stats.files_processed, 2, "{stats:?}");
        assert_eq!(stats.files_skipped, 0, "{stats:?}");
        assert_eq!(fake.batches, 3);

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
    fn non_utf8_source_is_chunked_lossily_not_failed() {
        let root = tempfile::tempdir().unwrap();
        // A Latin-1 byte in a comment (php-src's ext/gd/libgd/gdtestft.c).
        std::fs::write(root.path().join("a.rs"), b"// caf\xe9\nfn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };

        let stats =
            semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();

        assert_eq!(stats.files_failed, 0, "{stats:?}");
        assert_eq!(stats.files_processed, 1, "{stats:?}");
        assert!(stats.chunks_created > 0, "{stats:?}");
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_failures_preserve_semantic_rows_without_attesting_mixed_vectors() {
        use std::os::unix::fs::PermissionsExt;

        for (strategy, change_fingerprint) in [
            (IndexStrategy::Incremental, false),
            (IndexStrategy::Full, false),
            (IndexStrategy::Incremental, true),
            (IndexStrategy::Full, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            for name in ["durable", "deleted", "excluded", "healthy"] {
                std::fs::write(
                    root.path().join(format!("{name}.rs")),
                    format!("fn {name}() {{}}\n"),
                )
                .unwrap();
            }
            let db = Database::open_in_memory().unwrap();
            let mut fake = FakeEmbedder {
                prepared_with: Vec::new(),
                batches: 0,
            };
            semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
            let original_hash = db.all_semantic_file_hashes().unwrap()["durable.rs"].clone();
            let durable = root.path().join("durable.rs");
            std::fs::set_permissions(&durable, std::fs::Permissions::from_mode(0o0)).unwrap();
            if std::fs::read(&durable).is_ok() {
                std::fs::set_permissions(&durable, std::fs::Permissions::from_mode(0o600)).unwrap();
                eprintln!("unreadability requires a user without permission bypass");
                continue;
            }
            std::fs::remove_file(root.path().join("deleted.rs")).unwrap();
            std::fs::write(root.path().join("healthy.rs"), "fn changed() {}\n").unwrap();
            let fingerprint = if change_fingerprint {
                other_fp()
            } else {
                test_fp()
            };
            let stats = semantic_index(
                root.path(),
                &db,
                &mut fake,
                &["excluded.rs".to_string()],
                strategy,
                &fingerprint,
                false,
            )
            .unwrap();
            std::fs::set_permissions(&durable, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(stats.files_failed, 1, "{stats:?}");
            assert_eq!(stats.failed_paths, vec!["durable.rs"]);
            assert_eq!(stats.files_removed, 2);
            assert_eq!(stats.files_processed, 1);
            assert_eq!(
                db.all_chunk_file_paths().unwrap(),
                vec!["durable.rs", "healthy.rs"]
            );
            assert_eq!(
                db.all_semantic_file_hashes().unwrap()["durable.rs"],
                original_hash
            );
            assert_eq!(
                db.semantic_fingerprint().unwrap(),
                (!change_fingerprint).then(|| fingerprint.as_str().to_string())
            );

            std::fs::write(&durable, "fn recovered() {}\n").unwrap();
            semantic_incremental_index(
                root.path(),
                &db,
                &mut fake,
                &["excluded.rs".to_string()],
                &fingerprint,
                false,
            )
            .unwrap();
            assert_ne!(
                db.all_semantic_file_hashes().unwrap()["durable.rs"],
                original_hash
            );
            require_current_semantic_table(&db, &fingerprint).unwrap();
        }
    }

    /// A file discovery listed but the pass cannot read (deleted or made
    /// unreadable between the two, or any other per-file read error).
    fn unreadable(path: &str) -> FileInfo {
        file(path, "gone")
    }

    #[test]
    fn full_rebuild_with_a_failed_file_purges_its_rows_and_attests_the_table() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        // b.rs holds rows from a previous setup that this pass cannot rewrite.
        db.insert_chunks(
            "b.rs",
            "rust",
            &[("old b", 1, 1, embedding(0.1).as_slice())],
        )
        .unwrap();
        db.upsert_semantic_file_hash("b.rs", "stale").unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        let files = vec![file("a.rs", "h"), unreadable("b.rs")];

        let stats = semantic_index_discovered(
            root.path(),
            &db,
            &mut fake,
            &files,
            IndexStrategy::Full,
            &test_fp(),
            false,
        )
        .unwrap();

        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(stats.failed_paths, vec!["b.rs".to_string()], "{stats:?}");
        assert_eq!(stats.files_processed, 1, "{stats:?}");
        assert!(
            db.chunk_embeddings_for_file("b.rs").unwrap().is_empty(),
            "rows this pass could not rewrite must not survive under the new attestation"
        );
        assert!(
            !db.all_semantic_file_hashes().unwrap().contains_key("b.rs"),
            "the failed file must be retried by the next incremental pass"
        );
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(test_fp().as_str()),
            "the table holds only this run's vectors, so the pass attests it"
        );
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    #[test]
    fn incremental_over_an_unrecorded_table_with_a_failed_file_purges_and_attests() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        db.insert_chunks(
            "b.rs",
            "rust",
            &[("old b", 1, 1, embedding(0.1).as_slice())],
        )
        .unwrap();
        db.upsert_semantic_file_hash("b.rs", "stale").unwrap();
        assert_eq!(db.semantic_fingerprint().unwrap(), None);
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        let files = vec![file("a.rs", "h"), unreadable("b.rs")];

        let stats = semantic_index_discovered(
            root.path(),
            &db,
            &mut fake,
            &files,
            IndexStrategy::Incremental,
            &test_fp(),
            false,
        )
        .unwrap();

        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert!(db.chunk_embeddings_for_file("b.rs").unwrap().is_empty());
        assert!(!db.all_semantic_file_hashes().unwrap().contains_key("b.rs"));
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    #[test]
    fn a_persistently_failing_file_does_not_force_a_full_reembed_every_pass() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        let files = vec![file("a.rs", "h"), unreadable("b.rs")];
        let full = |db: &Database, fake: &mut FakeEmbedder| {
            semantic_index_discovered(
                root.path(),
                db,
                fake,
                &files,
                IndexStrategy::Full,
                &test_fp(),
                false,
            )
            .unwrap()
        };
        full(&db, &mut fake);
        assert_eq!(fake.batches, 1);

        let stats = semantic_index_discovered(
            root.path(),
            &db,
            &mut fake,
            &files,
            IndexStrategy::Incremental,
            &test_fp(),
            false,
        )
        .unwrap();
        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(
            stats.files_skipped, 1,
            "a.rs is unchanged and skipped: {stats:?}"
        );
        assert_eq!(stats.files_processed, 0, "{stats:?}");
        assert_eq!(fake.batches, 1, "nothing was re-embedded");
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    #[test]
    fn incremental_pass_keeps_an_attested_files_rows_when_it_fails_to_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        let before = db.chunk_embeddings_for_file("b.rs").unwrap();
        assert!(!before.is_empty());

        // Simulate disappearance after discovery; retained rows keep their old hash
        // so the next pass retries the read.
        std::fs::remove_file(root.path().join("b.rs")).unwrap();
        let files = vec![file("a.rs", "h"), unreadable("b.rs")];
        let stats = semantic_index_discovered(
            root.path(),
            &db,
            &mut fake,
            &files,
            IndexStrategy::Incremental,
            &test_fp(),
            false,
        )
        .unwrap();
        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(
            db.chunk_embeddings_for_file("b.rs").unwrap().len(),
            before.len()
        );
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
        let hashes = db.all_semantic_file_hashes().unwrap();
        assert_ne!(
            hashes.get("b.rs").map(String::as_str),
            Some("gone"),
            "a failed file's hash must not advance, or the next pass would skip it"
        );
        assert!(
            hashes.contains_key("b.rs"),
            "the attested rows keep their hash"
        );
    }

    #[test]
    fn watcher_pass_never_purges_a_file_it_fails_to_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut fake = FakeEmbedder {
            prepared_with: Vec::new(),
            batches: 0,
        };
        semantic_full_index(root.path(), &db, &mut fake, &[], &test_fp(), false).unwrap();
        let before = db.chunk_embeddings_for_file("b.rs").unwrap();
        assert!(!before.is_empty());

        std::fs::remove_file(root.path().join("b.rs")).unwrap();
        let stats = semantic_index_files(
            root.path(),
            &db,
            &mut fake,
            &[unreadable("b.rs")],
            &test_fp(),
            false,
        )
        .unwrap();

        assert_eq!(stats.failed_paths, vec!["b.rs".to_string()], "{stats:?}");
        assert_eq!(
            db.chunk_embeddings_for_file("b.rs").unwrap().len(),
            before.len(),
            "the watcher never sees the whole table, so it never purges"
        );
        let hashes = db.all_semantic_file_hashes().unwrap();
        assert!(hashes.contains_key("b.rs"));
        assert_ne!(hashes.get("b.rs").map(String::as_str), Some("gone"));
        assert!(semantic_table_state(&db, &test_fp()).unwrap().is_current());
    }

    #[test]
    fn summarize_paths_caps_the_list_and_counts_the_rest() {
        let paths: Vec<String> = (0..12).map(|i| format!("f{i}.rs")).collect();
        let s = summarize_paths(&paths, 10);
        assert!(s.starts_with("f0.rs, f1.rs"));
        assert!(s.ends_with("f9.rs (+2 more)"), "{s}");
        assert_eq!(summarize_paths(&paths[..2], 10), "f0.rs, f1.rs");
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

        // Clear the in-memory cache so only persisted attestation can avoid reads.
        forget_cached_digests();
        let original = std::fs::metadata(&artifacts.onnx)
            .unwrap()
            .modified()
            .unwrap();
        let file = std::fs::File::open(&artifacts.onnx).unwrap();

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

        // Both digest entries are cold; an mtime change forces rehashing both artifacts.
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

        // A matching stat key cannot authorize a different pooling fingerprint.
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
        semantic_incremental_index(root.path(), &db, &mut fake, &[], &other_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap().as_deref(),
            Some(other_fp().as_str())
        );
        let rows_under_a = db.chunk_count().unwrap();
        assert!(rows_under_a > COMMIT_BATCH_SIZE);

        // Switching setups forces a full rewrite; fail after the first committed batch.
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

        semantic_index_files(root.path(), &db, &mut fake, &only_a, &test_fp(), false).unwrap();
        assert_eq!(
            db.semantic_fingerprint().unwrap(),
            None,
            "a.rs holds B's vectors and b.rs holds A's; no record may describe that"
        );
        assert!(require_current_semantic_table(&db, &other_fp()).is_err());
        assert!(require_current_semantic_table(&db, &test_fp()).is_err());

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
