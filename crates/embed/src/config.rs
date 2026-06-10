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
pub const BATCH_SIZE: usize = 64;

/// Whether a configured `device` string requests the CUDA / GPU execution path.
/// Case-insensitive so `"GPU"` / `"CUDA"` take the GPU path like `"gpu"`.
pub fn wants_cuda(device: &str) -> bool {
    matches!(device.trim().to_ascii_lowercase().as_str(), "gpu" | "cuda")
}

/// Validate a configured `device` string. Accepts `cpu` / `gpu` / `cuda`
/// (case-insensitive); errors on anything else.
///
/// Without this, any unrecognized value — `"GPU"` before the case fix,
/// `"cuda:0"`, or a typo — made [`wants_cuda`] false and silently ran on CPU
/// with no error and no warning: the exact silent-CPU-fallback failure this
/// crate goes out of its way to make loud elsewhere (the `/proc/self/maps`
/// guard in `model.rs`). Validating up front turns a near-miss into an
/// actionable error instead of a 10x-slower run.
pub fn validate_device(device: &str) -> Result<(), String> {
    match device.trim().to_ascii_lowercase().as_str() {
        "cpu" | "gpu" | "cuda" => Ok(()),
        other => Err(format!(
            "unknown device {other:?} in .codesage/config.toml: expected one of \"cpu\", \"gpu\", \"cuda\""
        )),
    }
}

#[cfg(test)]
mod device_tests {
    use super::{validate_device, wants_cuda};

    #[test]
    fn validate_device_accepts_known_values_any_case() {
        for d in ["cpu", "gpu", "cuda", "GPU", "Cuda", " cpu "] {
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
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "cpu".to_string(),
            reranker: None,
            pooling: None,
        }
    }
}

impl EmbeddingConfig {
    pub fn pooling_strategy(&self) -> PoolingStrategy {
        if let Some(p) = self.pooling {
            return p;
        }
        if self.model.contains("bge-") {
            PoolingStrategy::Cls
        } else {
            PoolingStrategy::Mean
        }
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
}
