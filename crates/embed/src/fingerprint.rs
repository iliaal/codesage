//! Stored vectors are reusable only with matching artifacts, dimensions,
//! pooling, chunking, pipeline policy, and execution provider. Table names
//! encode only model and dimension. Digest loaded artifact bytes, not pins:
//! pins govern supply-chain verification and may be bypassed.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::chunk::{CHUNKER_VERSION, ChunkConfig};
use crate::config::{EmbeddingConfig, MAX_SEQ_LENGTH, wants_coreml, wants_cuda};
use crate::model::{ModelArtifacts, resolve_model_artifacts};

/// Bump when tokenizer truncation, BatchLongest padding, or unconditional L2
/// normalization changes; these policies affect vectors without changing artifacts.
pub const EMBEDDING_PIPELINE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineIdentity {
    pub version: u32,
    pub max_seq_length: usize,
    pub normalized: bool,
}

impl PipelineIdentity {
    pub const CURRENT: Self = Self {
        version: EMBEDDING_PIPELINE_VERSION,
        max_seq_length: MAX_SEQ_LENGTH,
        normalized: true,
    };
}

/// Persisted identity compared before vector reuse. Equality uses text alone;
/// digest/stat metadata allows later processes to reuse an artifact attestation.
#[derive(Debug, Clone)]
pub struct SemanticFingerprint {
    text: String,
    artifact_digest: String,
    artifact_stat_key: Option<String>,
    /// Retained to rebind the actual execution provider without rehashing.
    inputs: FingerprintInputs,
}

#[derive(Debug, Clone)]
struct FingerprintInputs {
    model: String,
    dim: usize,
    pooling: &'static str,
    execution_provider: String,
    pipeline: PipelineIdentity,
    ort_runtime: String,
}

/// Dynamic builds record the compiled API level and digest the runtime library
/// with model artifacts. Static builds digest ORT's version/commit/build flags.
pub fn ort_runtime_tag() -> String {
    #[cfg(target_vendor = "apple")]
    {
        let build = hex::encode(Sha256::digest(ort::info().as_bytes()));
        format!("api1.{}/static:{}", ort::MINOR_VERSION, &build[..16])
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        format!("api1.{}/dylib", ort::MINOR_VERSION)
    }
}

impl PartialEq for SemanticFingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for SemanticFingerprint {}

impl SemanticFingerprint {
    /// Derive the fingerprint from the configured model's files on disk and
    /// the dimension the table was opened for. Resolves the artifacts the
    /// way the loader does (a cache miss downloads) and digests their bytes,
    /// once per process per unchanged file; never builds a session.
    ///
    /// Always reads the model files when the process cache is cold. A caller
    /// with a recorded attestation at hand should prefer
    /// `codesage_graph::resolve_semantic_fingerprint`, which compares the
    /// stat key first and reads nothing when it matches.
    pub fn compute(config: &EmbeddingConfig, dim: usize) -> Result<Self> {
        let artifacts = resolve_model_artifacts(&config.model)
            .with_context(|| format!("resolving model files for {:?}", config.model))?;
        Self::for_artifacts(config, dim, &artifacts)
    }

    /// The fingerprint for `config` over an explicit artifact set. Reads
    /// every artifact the process cache does not already hold.
    pub fn for_artifacts(
        config: &EmbeddingConfig,
        dim: usize,
        artifacts: &ModelArtifacts,
    ) -> Result<Self> {
        let digest = model_artifact_digest(artifacts)?;
        let mut fingerprint = Self::with_artifact_digest(config, dim, &digest);
        fingerprint.artifact_stat_key = artifacts.stat_key();
        Ok(fingerprint)
    }

    /// The fingerprint for `config` given an already-computed artifact
    /// digest. Pure; carries no stat key.
    pub fn with_artifact_digest(
        config: &EmbeddingConfig,
        dim: usize,
        artifact_digest: &str,
    ) -> Self {
        Self::with_pipeline(config, dim, artifact_digest, &PipelineIdentity::CURRENT)
    }

    /// The fingerprint for `config` given an artifact digest a persisted
    /// attestation vouches for, together with the stat key it was recorded
    /// under. Pure; nothing is read.
    pub fn with_attested_digest(
        config: &EmbeddingConfig,
        dim: usize,
        artifact_digest: &str,
        artifact_stat_key: &str,
    ) -> Self {
        let mut fingerprint = Self::with_artifact_digest(config, dim, artifact_digest);
        fingerprint.artifact_stat_key = Some(artifact_stat_key.to_string());
        fingerprint
    }

    /// The fingerprint for `config` as configured; see [`render`] for the
    /// persisted form.
    pub fn with_pipeline(
        config: &EmbeddingConfig,
        dim: usize,
        artifact_digest: &str,
        pipeline: &PipelineIdentity,
    ) -> Self {
        let pooling = match config.pooling_strategy() {
            crate::config::PoolingStrategy::Mean => "mean",
            crate::config::PoolingStrategy::Cls => "cls",
        };
        // CPU and CUDA vectors differ. Equivalent device spellings share an
        // identity; the loaded session can rebind the actual provider.
        let inputs = FingerprintInputs {
            model: config.model.clone(),
            dim,
            pooling,
            execution_provider: configured_execution_provider(&config.device).to_string(),
            pipeline: *pipeline,
            ort_runtime: ort_runtime_tag(),
        };
        Self {
            text: render(&inputs, artifact_digest),
            artifact_digest: artifact_digest.to_string(),
            artifact_stat_key: None,
            inputs,
        }
    }

    /// This fingerprint with its device component replaced by `provider`,
    /// the execution provider a session actually initialised. Everything
    /// else — digest, stat key, pipeline — is carried over unchanged.
    pub fn with_execution_provider(&self, provider: &str) -> Self {
        let mut inputs = self.inputs.clone();
        inputs.execution_provider = provider.to_string();
        Self {
            text: render(&inputs, &self.artifact_digest),
            artifact_digest: self.artifact_digest.clone(),
            artifact_stat_key: self.artifact_stat_key.clone(),
            inputs,
        }
    }

    /// The execution provider this fingerprint's vectors are attributed to.
    pub fn execution_provider(&self) -> &str {
        &self.inputs.execution_provider
    }

    /// The persisted form.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The model-artifact digest this fingerprint was built over.
    pub fn artifact_digest(&self) -> &str {
        &self.artifact_digest
    }

    /// Path, size, and mtime of every artifact the digest covered, when the
    /// fingerprint was derived from files on disk (or from an attestation
    /// that recorded one).
    pub fn artifact_stat_key(&self) -> Option<&str> {
        self.artifact_stat_key.as_deref()
    }
}

impl fmt::Display for SemanticFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn render(inputs: &FingerprintInputs, artifact_digest: &str) -> String {
    let chunk = ChunkConfig::default();
    let normalized = if inputs.pipeline.normalized {
        "l2"
    } else {
        "none"
    };
    format!(
        "v4;model={};artifacts={artifact_digest};dim={};pooling={};\
         device={};ort={};pipeline={};maxseq={};norm={normalized};\
         chunker={CHUNKER_VERSION};chunk={}/{}/{}",
        inputs.model,
        inputs.dim,
        inputs.pooling,
        inputs.execution_provider,
        inputs.ort_runtime,
        inputs.pipeline.version,
        inputs.pipeline.max_seq_length,
        chunk.chunk_size,
        chunk.min_chunk_size,
        chunk.overlap
    )
}

/// The execution provider a configured `device` string selects — what the
/// config asks for, not necessarily what a session ends up running on.
pub fn configured_execution_provider(device: &str) -> &'static str {
    if wants_cuda(device) {
        "cuda"
    } else if wants_coreml(device) {
        "coreml"
    } else {
        "cpu"
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

fn read_counts() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Invalidate before replacing an artifact: same-size, same-mtime refetches
/// otherwise retain the evicted bytes' digest. Harmless if no replacement occurs.
pub(crate) fn forget_cached_digest(path: &Path) {
    digest_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Drop every cached digest, so the next lookup reads the file as a fresh
/// process would. For tests that need a cold cache without a new process;
/// nothing in a running command should call it.
pub fn forget_cached_digests() {
    digest_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Full-file digest reads for this path, isolated from concurrent tests' files.
pub fn artifact_read_count(path: &Path) -> u64 {
    read_counts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .copied()
        .unwrap_or(0)
}

/// Hex SHA-256 cached by path, size, and mtime for the process lifetime.
/// Same-size, same-mtime rewrites are an accepted blind spot. Pin verification
/// shares this cache so fingerprinting does not reread large model artifacts.
pub fn cached_file_digest(path: &Path) -> Result<String> {
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
    *read_counts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(path.to_path_buf())
        .or_insert(0) += 1;
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
            ort_runtime: None,
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
    fn pipeline_version_and_policy_are_part_of_the_identity() {
        let config = EmbeddingConfig::default();
        let current = SemanticFingerprint::with_artifact_digest(&config, 384, "d");
        assert!(
            current.as_str().contains(&format!(
                "pipeline={EMBEDDING_PIPELINE_VERSION};maxseq={MAX_SEQ_LENGTH};norm=l2"
            )),
            "{current}"
        );
        let bumped = SemanticFingerprint::with_pipeline(
            &config,
            384,
            "d",
            &PipelineIdentity {
                version: EMBEDDING_PIPELINE_VERSION + 1,
                ..PipelineIdentity::CURRENT
            },
        );
        assert_ne!(current, bumped, "a pipeline version bump must be visible");
        let longer = SemanticFingerprint::with_pipeline(
            &config,
            384,
            "d",
            &PipelineIdentity {
                max_seq_length: MAX_SEQ_LENGTH * 2,
                ..PipelineIdentity::CURRENT
            },
        );
        assert_ne!(current, longer, "a truncation change must be visible");
        let raw = SemanticFingerprint::with_pipeline(
            &config,
            384,
            "d",
            &PipelineIdentity {
                normalized: false,
                ..PipelineIdentity::CURRENT
            },
        );
        assert_ne!(
            current, raw,
            "dropping the L2 normalisation must be visible"
        );
    }

    #[test]
    fn for_artifacts_carries_the_stat_key_and_counts_its_reads() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = scratch_artifacts(dir.path());
        let config = EmbeddingConfig::default();
        let before = artifact_read_count(&artifacts.onnx);
        let fp = SemanticFingerprint::for_artifacts(&config, 384, &artifacts).unwrap();
        assert_eq!(fp.artifact_stat_key(), artifacts.stat_key().as_deref());
        assert_eq!(
            fp.artifact_digest(),
            model_artifact_digest(&artifacts).unwrap()
        );
        assert_eq!(artifact_read_count(&artifacts.onnx), before + 1);

        let attested = SemanticFingerprint::with_attested_digest(
            &config,
            384,
            fp.artifact_digest(),
            fp.artifact_stat_key().unwrap(),
        );
        assert_eq!(attested, fp);
        assert_eq!(attested.artifact_stat_key(), fp.artifact_stat_key());
        assert_eq!(artifact_read_count(&artifacts.onnx), before + 1);
    }

    #[test]
    fn device_is_part_of_the_identity_by_execution_provider() {
        let gpu = EmbeddingConfig {
            device: "gpu".to_string(),
            ..EmbeddingConfig::default()
        };
        let cuda = EmbeddingConfig {
            device: "CUDA".to_string(),
            ..EmbeddingConfig::default()
        };
        let cpu = EmbeddingConfig {
            device: "cpu".to_string(),
            ..EmbeddingConfig::default()
        };
        let fp = |c: &EmbeddingConfig| SemanticFingerprint::with_artifact_digest(c, 384, "d");
        assert_eq!(fp(&gpu), fp(&cuda), "two spellings of one provider");
        assert_ne!(
            fp(&gpu),
            fp(&cpu),
            "CPU and CUDA vectors must never share a table"
        );
        assert!(fp(&gpu).as_str().contains("device=cuda"), "{}", fp(&gpu));
        assert!(fp(&cpu).as_str().contains("device=cpu"), "{}", fp(&cpu));
    }

    #[test]
    fn rebinding_the_execution_provider_changes_only_the_device_component() {
        let cuda = EmbeddingConfig {
            device: "cuda".to_string(),
            ..EmbeddingConfig::default()
        };
        let cpu = EmbeddingConfig {
            device: "cpu".to_string(),
            ..EmbeddingConfig::default()
        };
        let as_cuda = SemanticFingerprint::with_attested_digest(&cuda, 384, "d", "stat");
        assert_eq!(as_cuda.execution_provider(), "cuda");

        let fell_back = as_cuda.with_execution_provider("cpu");
        assert_ne!(fell_back, as_cuda, "the fallback is another identity");
        assert_eq!(
            fell_back,
            SemanticFingerprint::with_artifact_digest(&cpu, 384, "d")
        );
        assert_eq!(fell_back.execution_provider(), "cpu");
        assert_eq!(fell_back.artifact_digest(), "d");
        assert_eq!(fell_back.artifact_stat_key(), Some("stat"));
        assert_eq!(as_cuda.with_execution_provider("cuda"), as_cuda);
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
            ort_runtime: None,
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
    fn the_onnx_runtime_is_part_of_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let config = EmbeddingConfig::default();
        let without = scratch_artifacts(dir.path());
        let runtime = dir.path().join("libonnxruntime.so.1.24.1");
        std::fs::write(&runtime, b"ELF onnxruntime 1.24.1 kernels").unwrap();
        let with = ModelArtifacts {
            ort_runtime: Some(runtime.clone()),
            ..without.clone()
        };

        let fp_without = SemanticFingerprint::for_artifacts(&config, 384, &without).unwrap();
        let fp_with = SemanticFingerprint::for_artifacts(&config, 384, &with).unwrap();
        assert!(fp_with.as_str().starts_with("v4;"), "{fp_with}");
        assert!(
            fp_with
                .as_str()
                .contains(&format!(";ort={};", ort_runtime_tag())),
            "{fp_with}"
        );
        assert_ne!(
            fp_without, fp_with,
            "a runtime joining the artifact set must be visible"
        );
        assert!(
            fp_with
                .artifact_stat_key()
                .unwrap()
                .contains("ort_runtime="),
            "the stat key names the runtime: {:?}",
            fp_with.artifact_stat_key()
        );
        assert!(
            !fp_without
                .artifact_stat_key()
                .unwrap()
                .contains("ort_runtime="),
            "{:?}",
            fp_without.artifact_stat_key()
        );

        flip_one_byte(&runtime);
        let upgraded = SemanticFingerprint::for_artifacts(&config, 384, &with).unwrap();
        assert_ne!(
            fp_with, upgraded,
            "a changed runtime library must change the fingerprint"
        );
        assert_ne!(fp_with.artifact_digest(), upgraded.artifact_digest());
        assert_ne!(fp_with.artifact_stat_key(), upgraded.artifact_stat_key());
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
