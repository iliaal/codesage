use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

// Embed-time max tokens per sequence. Most code embedders we target
// (Jina v2 base-code, MiniLM, MS-MARCO MiniLM) accept ≥512 natively.
// The previous value (256) was chosen at a time when the index used
// MiniLM-L6 with a smaller default; it created a silent-truncation gap
// versus the char-based chunker (DEFAULT_CHUNK_SIZE=1000 ≈ 250–330
// tokens for code), so dense chunks at the long tail had their right
// edge dropped before pooling. Raising to 512 closes that gap and lets
// chunks grow to ~1500 chars with no truncation. The bench A/B that
// validated this lives in `bench/history/cap512-1500-2026-05-04.md`.
pub const MAX_SEQ_LENGTH: usize = 512;
#[cfg(not(target_vendor = "apple"))]
pub const BATCH_SIZE: usize = 64;
#[cfg(target_vendor = "apple")]
pub const BATCH_SIZE: usize = 10;
pub const MAX_BATCH_SIZE: usize = 256;

/// Whether a configured `device` string requests the CUDA / GPU execution path.
/// Case-insensitive so `"GPU"` / `"CUDA"` take the GPU path like `"gpu"`.
pub fn wants_cuda(device: &str) -> bool {
    matches!(device.trim().to_ascii_lowercase().as_str(), "gpu" | "cuda")
}

/// Whether a configured `device` string requests the CoreML execution provider.
/// CoreML accelerates ONNX inference on Apple Silicon via ONNX Runtime's CoreML EP.
pub fn wants_coreml(device: &str) -> bool {
    matches!(device.trim().to_ascii_lowercase().as_str(), "coreml")
}

/// Validate a configured `device` string. Accepts `cpu` / `gpu` / `cuda` /
/// `coreml` (case-insensitive); errors on anything else.
///
/// Without this, any unrecognized value — `"GPU"` before the case fix,
/// `"cuda:0"`, or a typo — made [`wants_cuda`] false and silently ran on CPU
/// with no error and no warning: the exact silent-CPU-fallback failure this
/// crate goes out of its way to make loud elsewhere (the `/proc/self/maps`
/// guard in `model.rs`). Validating up front turns a near-miss into an
/// actionable error instead of a 10x-slower run.
pub fn validate_device(device: &str) -> Result<(), String> {
    match device.trim().to_ascii_lowercase().as_str() {
        "cpu" | "gpu" | "cuda" | "coreml" => Ok(()),
        other => Err(format!(
            "unknown device {other:?} in .codesage/config.toml: expected one of \"cpu\", \"gpu\", \"cuda\", \"coreml\""
        )),
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
        // Pre-fix these silently ran on CPU; now they error.
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
    /// Override the pooling strategy. When omitted, falls back to a
    /// model-name heuristic (`bge-*` → CLS, everything else → Mean). The
    /// heuristic is silent and wrong for any non-`bge-` model that uses CLS
    /// pooling (intfloat/e5-*, etc.) or any `bge-` model that uses Mean —
    /// both produce semantically wrong vectors with no error. Set this
    /// explicitly when picking a non-default model.
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
    pub fn effective_batch_size(&self) -> Result<NonZeroUsize, String> {
        self.effective_batch_size_with_env(std::env::var("CODESAGE_BATCH_SIZE").ok().as_deref())
    }

    fn effective_batch_size_with_env(
        &self,
        env_value: Option<&str>,
    ) -> Result<NonZeroUsize, String> {
        if let Some(n) = self.batch_size_override {
            return validate_batch_size(n, "batch size override");
        }
        if let Some(value) = env_value {
            let parsed = value.trim().parse::<usize>().map_err(|e| {
                format!(
                    "invalid CODESAGE_BATCH_SIZE value {value:?}: expected a positive integer ({e})"
                )
            })?;
            let n = NonZeroUsize::new(parsed).ok_or_else(|| {
                format!("invalid CODESAGE_BATCH_SIZE value {value:?}: expected a positive integer")
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
        if self.model.contains("bge-") {
            PoolingStrategy::Cls
        } else {
            // Mean is correct for MiniLM/E5-style models but wrong for any
            // CLS model not named `bge-*`. Warn once so a silent pooling
            // mismatch on a custom model surfaces. fnd: CR-007.
            if !self.model.to_lowercase().contains("minilm") {
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

fn validate_batch_size(n: NonZeroUsize, source: &str) -> Result<NonZeroUsize, String> {
    if n.get() > MAX_BATCH_SIZE {
        Err(format!(
            "{source} value {} exceeds max supported batch size {MAX_BATCH_SIZE}",
            n.get()
        ))
    } else {
        Ok(n)
    }
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

        assert!(err.contains("positive integer"), "unexpected error: {err}");
    }

    #[test]
    fn effective_batch_size_rejects_invalid_env_value() {
        let cfg = EmbeddingConfig::default();

        let err = cfg.effective_batch_size_with_env(Some("abc")).unwrap_err();

        assert!(err.contains("positive integer"), "unexpected error: {err}");
    }

    #[test]
    fn effective_batch_size_rejects_oversized_config_value() {
        let cfg = EmbeddingConfig {
            batch_size: Some(nz(MAX_BATCH_SIZE + 1)),
            ..EmbeddingConfig::default()
        };

        let err = cfg.effective_batch_size_with_env(None).unwrap_err();

        assert!(
            err.contains("exceeds max supported batch size"),
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
            err.contains("exceeds max supported batch size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_batch_size_rejects_oversized_cli_override() {
        let mut cfg = EmbeddingConfig::default();
        cfg.set_batch_size_override(nz(MAX_BATCH_SIZE + 1));

        let err = cfg.effective_batch_size_with_env(None).unwrap_err();

        assert!(
            err.contains("exceeds max supported batch size"),
            "unexpected error: {err}"
        );
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
