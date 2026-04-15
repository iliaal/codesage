use serde::{Deserialize, Serialize};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

pub const MAX_SEQ_LENGTH: usize = 256;
pub const BATCH_SIZE: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model: String,
    pub device: String,
    #[serde(default)]
    pub reranker: Option<String>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "cpu".to_string(),
            reranker: None,
        }
    }
}

impl EmbeddingConfig {
    pub fn pooling_strategy(&self) -> PoolingStrategy {
        if self.model.contains("bge-") {
            PoolingStrategy::Cls
        } else {
            PoolingStrategy::Mean
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
