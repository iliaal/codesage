use anyhow::{Result, bail};
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
    #[serde(default = "default_embedding_backend")]
    pub backend: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub dimensions: Option<usize>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "cpu".to_string(),
            reranker: None,
            backend: default_embedding_backend(),
            base_url: None,
            dimensions: None,
            api_key_env: None,
        }
    }
}

impl EmbeddingConfig {
    pub fn backend_kind(&self) -> Result<EmbeddingBackendKind> {
        EmbeddingBackendKind::parse(&self.backend)
    }

    pub fn provider_base_url(&self, kind: EmbeddingBackendKind) -> String {
        self.base_url
            .as_deref()
            .unwrap_or_else(|| kind.default_base_url())
            .trim_end_matches('/')
            .to_string()
    }

    pub fn storage_model_id(&self) -> Result<String> {
        match self.backend_kind()? {
            EmbeddingBackendKind::Onnx => Ok(self.model.clone()),
            kind @ (EmbeddingBackendKind::Ollama | EmbeddingBackendKind::OpenAiCompatible) => {
                let base_url = self.provider_base_url(kind);
                match self.dimensions {
                    Some(dim) => Ok(format!(
                        "{}:{base_url}:{}:{dim}",
                        kind.as_storage_prefix(),
                        self.model
                    )),
                    None => Ok(format!(
                        "{}:{base_url}:{}",
                        kind.as_storage_prefix(),
                        self.model
                    )),
                }
            }
        }
    }

    pub fn cache_key(&self) -> Result<String> {
        Ok(format!(
            "{}|device={}|api_key_env={}",
            self.storage_model_id()?,
            self.device,
            self.api_key_env.as_deref().unwrap_or("")
        ))
    }

    pub fn pooling_strategy(&self) -> PoolingStrategy {
        if self.model.contains("bge-") {
            PoolingStrategy::Cls
        } else {
            PoolingStrategy::Mean
        }
    }
}

fn default_embedding_backend() -> String {
    "onnx".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingBackendKind {
    Onnx,
    Ollama,
    OpenAiCompatible,
}

impl EmbeddingBackendKind {
    pub fn parse(raw: &str) -> Result<Self> {
        match normalize_backend(raw).as_str() {
            "onnx" | "hf" | "huggingface" => Ok(Self::Onnx),
            "ollama" => Ok(Self::Ollama),
            "openai" | "openai-compatible" | "openai_compatible" | "llama-cpp" | "llama_cpp" => {
                Ok(Self::OpenAiCompatible)
            }
            other => bail!(
                "unsupported embedding backend {other:?}; expected one of: onnx, ollama, openai-compatible"
            ),
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Onnx => "",
            Self::Ollama => "http://localhost:11434",
            Self::OpenAiCompatible => "http://localhost:8080",
        }
    }

    pub fn as_storage_prefix(self) -> &'static str {
        match self {
            Self::Onnx => "onnx",
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }
}

fn normalize_backend(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_preserves_onnx_model_id() {
        let cfg = EmbeddingConfig::default();

        assert_eq!(cfg.backend_kind().unwrap(), EmbeddingBackendKind::Onnx);
        assert_eq!(cfg.storage_model_id().unwrap(), cfg.model);
    }

    #[test]
    fn ollama_storage_model_id_includes_provider_and_base_url() {
        let cfg = EmbeddingConfig {
            model: "embeddinggemma".to_string(),
            backend: "ollama".to_string(),
            base_url: Some("http://localhost:11434/".to_string()),
            ..EmbeddingConfig::default()
        };

        assert_eq!(
            cfg.storage_model_id().unwrap(),
            "ollama:http://localhost:11434:embeddinggemma"
        );
    }

    #[test]
    fn openai_compatible_storage_model_id_includes_dimensions_when_configured() {
        let cfg = EmbeddingConfig {
            model: "nomic-embed-text".to_string(),
            backend: "llama-cpp".to_string(),
            base_url: Some("http://localhost:8080/v1/".to_string()),
            dimensions: Some(768),
            ..EmbeddingConfig::default()
        };

        assert_eq!(
            cfg.storage_model_id().unwrap(),
            "openai-compatible:http://localhost:8080/v1:nomic-embed-text:768"
        );
    }

    #[test]
    fn invalid_backend_fails_loudly() {
        let cfg = EmbeddingConfig {
            backend: "remote-mystery".to_string(),
            ..EmbeddingConfig::default()
        };

        let err = cfg.backend_kind().unwrap_err();
        assert!(err.to_string().contains("unsupported embedding backend"));
    }
}
