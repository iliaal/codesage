use std::num::NonZeroUsize;

use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

// Paired with DEFAULT_CHUNK_SIZE; see bench/history/cap512-1500-2026-05-04.md.
pub const MAX_SEQ_LENGTH: usize = 512;
#[cfg(not(target_vendor = "apple"))]
pub const BATCH_SIZE: usize = 64;
#[cfg(target_vendor = "apple")]
pub const BATCH_SIZE: usize = 10;
pub const MAX_BATCH_SIZE: usize = 256;

pub fn wants_cuda(device: &str) -> bool {
    matches!(device.trim().to_ascii_lowercase().as_str(), "gpu" | "cuda")
}

pub fn wants_coreml(device: &str) -> bool {
    matches!(device.trim().to_ascii_lowercase().as_str(), "coreml")
}

/// Reject unknown devices before provider selection can silently choose CPU.
pub fn validate_device(device: &str) -> Result<()> {
    match device.trim().to_ascii_lowercase().as_str() {
        "cpu" | "gpu" | "cuda" | "coreml" => Ok(()),
        other => bail!(
            "unknown device {other:?} in .codesage/config.toml: expected one of \"cpu\", \"gpu\", \"cuda\", \"coreml\""
        ),
    }
}

#[cfg(test)]
mod device_tests {
    use super::{validate_device, wants_coreml, wants_cuda};

    #[test]
    fn validate_device_accepts_known_values_any_case() {
        for d in [
            "cpu", "gpu", "cuda", "coreml", "GPU", "Cuda", "CoreML", " cpu ", " CoreML ",
        ] {
            assert!(validate_device(d).is_ok(), "{d} should be valid");
        }
    }

    #[test]
    fn validate_device_rejects_unknown_values() {
        for d in ["cuda:0", "gpuu", "CPU0", "metal", ""] {
            assert!(validate_device(d).is_err(), "{d} should be rejected");
        }
    }

    #[test]
    fn wants_cuda_is_case_insensitive() {
        assert!(wants_cuda("gpu"));
        assert!(wants_cuda("GPU"));
        assert!(wants_cuda("CUDA"));
        assert!(!wants_cuda("cpu"));
        assert!(!wants_cuda("cuda:0"));
    }

    #[test]
    fn wants_coreml_is_case_insensitive() {
        assert!(wants_coreml("coreml"));
        assert!(wants_coreml("CoreML"));
        assert!(wants_coreml(" COREml "));
        assert!(!wants_coreml("cpu"));
        assert!(!wants_coreml("gpu"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model: String,
    pub device: String,
    #[serde(default)]
    pub reranker: Option<String>,
    /// Override the case-insensitive heuristic (`bge-*` → CLS, otherwise Mean).
    /// Set explicitly when a custom model's pooling differs from that heuristic.
    #[serde(default)]
    pub pooling: Option<PoolingStrategy>,
    /// Embedding batch size. When omitted, falls back to
    /// `CODESAGE_BATCH_SIZE`, then the platform default.
    #[serde(default)]
    pub batch_size: Option<NonZeroUsize>,
    /// Runtime override from CLI flag. Highest priority and not serialized.
    #[serde(skip)]
    pub batch_size_override: Option<NonZeroUsize>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "cpu".to_string(),
            reranker: None,
            pooling: None,
            batch_size: None,
            batch_size_override: None,
        }
    }
}

impl EmbeddingConfig {
    pub fn effective_batch_size(&self) -> Result<NonZeroUsize> {
        self.effective_batch_size_with_env(std::env::var("CODESAGE_BATCH_SIZE").ok().as_deref())
    }

    fn effective_batch_size_with_env(&self, env_value: Option<&str>) -> Result<NonZeroUsize> {
        if let Some(n) = self.batch_size_override {
            return validate_batch_size(n, "batch size override");
        }
        if let Some(value) = env_value {
            let parsed = value.trim().parse::<usize>().map_err(|e| {
                anyhow!(
                    "invalid CODESAGE_BATCH_SIZE value {value:?}: expected a positive integer ({e})"
                )
            })?;
            let n = NonZeroUsize::new(parsed).ok_or_else(|| {
                anyhow!("invalid CODESAGE_BATCH_SIZE value {value:?}: expected a positive integer")
            })?;
            return validate_batch_size(n, "CODESAGE_BATCH_SIZE");
        }
        if let Some(n) = self.batch_size {
            return validate_batch_size(n, "[embedding].batch_size");
        }
        Ok(default_batch_size())
    }

    pub fn set_batch_size_override(&mut self, n: NonZeroUsize) {
        self.batch_size_override = Some(n);
    }

    pub fn pooling_strategy(&self) -> PoolingStrategy {
        if let Some(p) = self.pooling {
            return p;
        }
        let model = self.model.to_lowercase();
        if model.contains("bge-") {
            PoolingStrategy::Cls
        } else {
            if !model.contains("minilm") {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        model = %self.model,
                        "no [embedding].pooling set; defaulting to mean pooling. \
                         If this model expects CLS pooling, set pooling = \"cls\" explicitly."
                    );
                }
            }
            PoolingStrategy::Mean
        }
    }
}

pub fn default_batch_size() -> NonZeroUsize {
    NonZeroUsize::new(BATCH_SIZE).expect("BATCH_SIZE must be non-zero")
}

fn validate_batch_size(n: NonZeroUsize, source: &str) -> Result<NonZeroUsize> {
    ensure!(
        n.get() <= MAX_BATCH_SIZE,
        "{source} value {} exceeds max supported batch size {MAX_BATCH_SIZE}",
        n.get()
    );
    Ok(n)
}

#[cfg(test)]
mod batch_size_tests {
    use std::num::NonZeroUsize;

    use super::{BATCH_SIZE, EmbeddingConfig, MAX_BATCH_SIZE};

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn effective_batch_size_uses_platform_default() {
        let cfg = EmbeddingConfig::default();

        assert_eq!(
            cfg.effective_batch_size_with_env(None).unwrap().get(),
            BATCH_SIZE
        );
    }

    #[test]
    fn effective_batch_size_uses_config_value_before_default() {
        let cfg = EmbeddingConfig {
            batch_size: Some(nz(7)),
            ..EmbeddingConfig::default()
        };

        assert_eq!(cfg.effective_batch_size_with_env(None).unwrap().get(), 7);
    }

    #[test]
    fn effective_batch_size_env_overrides_config_value() {
        let cfg = EmbeddingConfig {
            batch_size: Some(nz(7)),
            ..EmbeddingConfig::default()
        };

        assert_eq!(
            cfg.effective_batch_size_with_env(Some("9")).unwrap().get(),
            9
        );
    }

    #[test]
    fn effective_batch_size_cli_override_wins_over_env_and_config() {
        let mut cfg = EmbeddingConfig {
            batch_size: Some(nz(7)),
            ..EmbeddingConfig::default()
        };
        cfg.set_batch_size_override(nz(11));

        assert_eq!(
            cfg.effective_batch_size_with_env(Some("9")).unwrap().get(),
            11
        );
    }

    #[test]
    fn effective_batch_size_rejects_zero_env_value() {
        let cfg = EmbeddingConfig::default();

        let err = cfg.effective_batch_size_with_env(Some("0")).unwrap_err();

        assert!(
            err.to_string().contains("positive integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_batch_size_rejects_invalid_env_value() {
        let cfg = EmbeddingConfig::default();

        let err = cfg.effective_batch_size_with_env(Some("abc")).unwrap_err();

        assert!(
            err.to_string().contains("positive integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_batch_size_rejects_oversized_config_value() {
        let cfg = EmbeddingConfig {
            batch_size: Some(nz(MAX_BATCH_SIZE + 1)),
            ..EmbeddingConfig::default()
        };

        let err = cfg.effective_batch_size_with_env(None).unwrap_err();

        assert!(
            err.to_string().contains("exceeds max supported batch size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_batch_size_rejects_oversized_env_value() {
        let cfg = EmbeddingConfig::default();

        let err = cfg
            .effective_batch_size_with_env(Some(&(MAX_BATCH_SIZE + 1).to_string()))
            .unwrap_err();

        assert!(
            err.to_string().contains("exceeds max supported batch size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_batch_size_rejects_oversized_cli_override() {
        let mut cfg = EmbeddingConfig::default();
        cfg.set_batch_size_override(nz(MAX_BATCH_SIZE + 1));

        let err = cfg.effective_batch_size_with_env(None).unwrap_err();

        assert!(
            err.to_string().contains("exceeds max supported batch size"),
            "unexpected error: {err}"
        );
    }
}

#[cfg(test)]
mod pooling_tests {
    use super::*;

    fn config_for(model: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            model: model.to_string(),
            ..EmbeddingConfig::default()
        }
    }

    #[test]
    fn pooling_heuristic_is_case_insensitive() {
        assert_eq!(
            config_for("BAAI/bge-m3").pooling_strategy(),
            PoolingStrategy::Cls,
            "an uppercase BGE id must take the CLS path like its lowercase spelling"
        );
        assert_eq!(
            config_for("baai/BGE-M3").pooling_strategy(),
            PoolingStrategy::Cls
        );
        assert_eq!(
            config_for("sentence-transformers/all-MiniLM-L6-v2").pooling_strategy(),
            PoolingStrategy::Mean
        );
        assert_eq!(
            config_for("sentence-transformers/ALL-MINILM-L6-V2").pooling_strategy(),
            PoolingStrategy::Mean,
            "an uppercase MiniLM id must stay on the mean path"
        );
    }

    #[test]
    fn explicit_pooling_overrides_the_heuristic() {
        let mut cfg = config_for("BAAI/bge-m3");
        cfg.pooling = Some(PoolingStrategy::Mean);
        assert_eq!(cfg.pooling_strategy(), PoolingStrategy::Mean);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolingStrategy {
    Mean,
    Cls,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub project: Option<ProjectMeta>,
    pub embedding: Option<EmbeddingConfig>,
    pub index: Option<IndexConfig>,
    pub docs: Option<DocsConfig>,
}

/// `[docs]`: what `codesage doctor --docs` sweeps by default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocsConfig {
    /// Glob patterns (repo-relative) for markdown files the docs check skips.
    pub exclude_patterns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexConfig {
    pub exclude_patterns: Option<Vec<String>>,
    /// Whether the live filesystem watcher auto-starts for this project.
    /// `None` is treated as enabled; set `false` to opt out.
    pub watch: Option<bool>,
}
