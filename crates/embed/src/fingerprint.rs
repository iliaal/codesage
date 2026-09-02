//! Identity of the vectors a chunk table holds.
//!
//! A stored embedding is reusable only by a run that would have produced the
//! same bytes: same model files, same dimension, same pooling, same chunker.
//! The table name (`chunks_<model>_<dim>`) carries two of those, so a pooling
//! change or a same-name model revision used to reuse every text-identical
//! vector — including under `--full`, which is the one command a user runs to
//! repair exactly that.
//!
//! The model component is a digest of the artifact bytes on disk — the files
//! the session loader opens — not of the pin table. The pin table is the
//! loader's separate supply-chain gate: a model outside it (loaded under
//! `CODESAGE_ALLOW_ANY_MODEL`) has no pin, and a pinned name loaded with that
//! opt-out is never verified, so under either the pin values said nothing
//! about the vectors a table held.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::chunk::{CHUNKER_VERSION, ChunkConfig};
use crate::config::EmbeddingConfig;
use crate::model::{ModelArtifacts, resolve_model_artifacts};

/// Opaque, comparable identity of an embedding setup. Persisted beside the
/// chunk table and compared byte-for-byte before any stored vector is reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticFingerprint(String);

impl SemanticFingerprint {
    /// Derive the fingerprint from the configured model's files on disk and
    /// the dimension the table was opened for. Resolves the artifacts the
    /// way the loader does (a cache miss downloads) and digests their bytes,
    /// once per process per unchanged file; never builds a session.
    pub fn compute(config: &EmbeddingConfig, dim: usize) -> Result<Self> {
        let artifacts = resolve_model_artifacts(&config.model)
            .with_context(|| format!("resolving model files for {:?}", config.model))?;
        Self::for_artifacts(config, dim, &artifacts)
    }

    /// The fingerprint for `config` over an explicit artifact set.
    pub fn for_artifacts(
        config: &EmbeddingConfig,
        dim: usize,
        artifacts: &ModelArtifacts,
    ) -> Result<Self> {
        let digest = model_artifact_digest(artifacts)?;
        Ok(Self::with_artifact_digest(config, dim, &digest))
    }

    /// The fingerprint for `config` given an already-computed artifact
    /// digest. Pure; the persisted form is built here and nowhere else.
    pub fn with_artifact_digest(
        config: &EmbeddingConfig,
        dim: usize,
        artifact_digest: &str,
    ) -> Self {
        let chunk = ChunkConfig::default();
        let pooling = match config.pooling_strategy() {
            crate::config::PoolingStrategy::Mean => "mean",
            crate::config::PoolingStrategy::Cls => "cls",
        };
        Self(format!(
            "v2;model={};artifacts={artifact_digest};dim={dim};pooling={pooling};\
             chunker={CHUNKER_VERSION};chunk={}/{}/{}",
            config.model, chunk.chunk_size, chunk.min_chunk_size, chunk.overlap
        ))
    }

    /// The persisted form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SemanticFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One digest over every artifact's bytes, label-prefixed and in the fixed
/// [`ModelArtifacts::labelled_files`] order, so a sidecar appearing or a file
/// swapping roles changes the result as much as a byte edit does.
pub fn model_artifact_digest(artifacts: &ModelArtifacts) -> Result<String> {
    let mut hasher = Sha256::new();
    for (label, path) in artifacts.labelled_files() {
        let digest = cached_file_digest(path)
            .with_context(|| format!("digesting model {label} at {}", path.display()))?;
        hasher.update(label.as_bytes());
        hasher.update(b"=");
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

struct CachedDigest {
    len: u64,
    modified: SystemTime,
    hex: String,
}

fn digest_cache() -> &'static Mutex<HashMap<PathBuf, CachedDigest>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedDigest>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// SHA-256 of the file at `path`, hex. A model file is tens to hundreds of
/// megabytes and every semantic command needs its digest, so the result is
/// kept for the life of the process and reused while the file's size and
/// mtime are unchanged; a rewrite that keeps both (same-second, same-length)
/// is the accepted blind spot of that key.
fn cached_file_digest(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    let len = meta.len();
    let modified = meta.modified()?;
    {
        let cache = digest_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(path)
            && entry.len == len
            && entry.modified == modified
        {
            return Ok(entry.hex.clone());
        }
    }
    let hex = sha256_file(path)?;
    let mut cache = digest_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(
        path.to_path_buf(),
        CachedDigest {
            len,
            modified,
            hex: hex.clone(),
        },
    );
    Ok(hex)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PoolingStrategy;
    use std::time::Duration;

    /// A scratch model: distinct tokenizer and graph bytes, no sidecar.
    fn scratch_artifacts(dir: &Path) -> ModelArtifacts {
        let tokenizer = dir.join("tokenizer.json");
        let onnx = dir.join("model.onnx");
        std::fs::write(&tokenizer, b"{\"model\":{\"type\":\"WordPiece\"}}").unwrap();
        std::fs::write(&onnx, b"ONNX graph bytes 0123456789").unwrap();
        ModelArtifacts {
            tokenizer,
            onnx,
            onnx_data: None,
        }
    }

    /// Rewrite one byte of `path` in place, keeping its length, and move its
    /// mtime forward so the per-process cache cannot answer from memory.
    fn flip_one_byte(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(path, &bytes).unwrap();
        let file = std::fs::File::open(path).unwrap();
        let later = std::fs::metadata(path).unwrap().modified().unwrap() + Duration::from_secs(2);
        file.set_modified(later).unwrap();
    }

    #[test]
    fn pooling_change_alters_the_fingerprint() {
        let mut config = EmbeddingConfig::default();
        let mean = SemanticFingerprint::with_artifact_digest(&config, 384, "d");
        config.pooling = Some(PoolingStrategy::Cls);
        let cls = SemanticFingerprint::with_artifact_digest(&config, 384, "d");
        assert_ne!(mean, cls);
        assert!(mean.as_str().contains("pooling=mean"), "{mean}");
        assert!(cls.as_str().contains("pooling=cls"), "{cls}");
    }

    #[test]
    fn fingerprint_carries_the_artifact_digest_not_the_pin_table() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = scratch_artifacts(dir.path());
        let config = EmbeddingConfig::default();
        let fp = SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap();
        let digest = model_artifact_digest(&artifacts).unwrap();
        assert!(fp.as_str().contains(&format!("artifacts={digest}")), "{fp}");
        assert!(fp.as_str().contains("dim=384"), "{fp}");
        assert!(
            fp.as_str().contains(&format!("chunker={CHUNKER_VERSION}")),
            "{fp}"
        );
        assert!(
            !fp.as_str().contains("revision="),
            "the pin table is the loader's gate, not the fingerprint's source: {fp}"
        );
    }

    #[test]
    fn one_rewritten_model_byte_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = scratch_artifacts(dir.path());
        let config = EmbeddingConfig::default();
        let before = SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap();
        // Same call twice: the second answer comes from the cache and agrees.
        assert_eq!(
            before,
            SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap()
        );

        flip_one_byte(&artifacts.onnx);
        let after = SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap();
        assert_ne!(
            before, after,
            "a same-name, same-length model edit must be visible"
        );

        flip_one_byte(&artifacts.tokenizer);
        let tokenizer_edit = SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap();
        assert_ne!(
            after, tokenizer_edit,
            "the tokenizer is part of the identity"
        );
    }

    #[test]
    fn a_sidecar_and_a_swapped_role_both_change_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = scratch_artifacts(dir.path());
        let plain = model_artifact_digest(&artifacts).unwrap();

        let swapped = ModelArtifacts {
            tokenizer: artifacts.onnx.clone(),
            onnx: artifacts.tokenizer.clone(),
            onnx_data: None,
        };
        assert_ne!(plain, model_artifact_digest(&swapped).unwrap());

        let data = dir.path().join("model.onnx_data");
        std::fs::write(&data, b"external weights").unwrap();
        let with_sidecar = ModelArtifacts {
            onnx_data: Some(data),
            ..artifacts
        };
        assert_ne!(plain, model_artifact_digest(&with_sidecar).unwrap());
    }

    #[test]
    fn a_missing_artifact_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifacts = scratch_artifacts(dir.path());
        artifacts.onnx = dir.path().join("absent.onnx");
        let err = model_artifact_digest(&artifacts).unwrap_err();
        assert!(err.to_string().contains("digesting model onnx"), "{err:#}");
    }

    #[test]
    fn dimension_and_model_are_part_of_the_identity() {
        let config = EmbeddingConfig::default();
        assert_ne!(
            SemanticFingerprint::with_artifact_digest(&config, 384, "d"),
            SemanticFingerprint::with_artifact_digest(&config, 768, "d")
        );
        let mut other = config.clone();
        other.model = "custom/model".to_string();
        assert_ne!(
            SemanticFingerprint::with_artifact_digest(&other, 384, "d"),
            SemanticFingerprint::with_artifact_digest(&config, 384, "d")
        );
    }
}
