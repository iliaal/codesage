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
