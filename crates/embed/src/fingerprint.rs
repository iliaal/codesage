//! Identity of the vectors a chunk table holds.
//!
//! A stored embedding is reusable only by a run that would have produced the
//! same bytes: same model, same pinned model files, same dimension, same
//! pooling, same chunker. The table name (`chunks_<model>_<dim>`) carries two
//! of those, so a pooling change or a same-name model revision used to reuse
//! every text-identical vector — including under `--full`, which is the one
//! command a user runs to repair exactly that.

use std::fmt;

use crate::chunk::{CHUNKER_VERSION, ChunkConfig};
use crate::config::EmbeddingConfig;
use crate::model::pinned_model_identity;

/// Opaque, comparable identity of an embedding setup. Persisted beside the
/// chunk table and compared byte-for-byte before any stored vector is reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticFingerprint(String);

impl SemanticFingerprint {
    /// Derive the fingerprint from the configured model and the dimension the
    /// table was opened for. Cheap: reads the static pin table, never loads
    /// the model.
    pub fn compute(config: &EmbeddingConfig, dim: usize) -> Self {
        let (revision, onnx) = match pinned_model_identity(&config.model) {
            Some(pin) => (pin.revision, pin.onnx_sha256),
            None => ("unpinned", "unpinned"),
        };
        let chunk = ChunkConfig::default();
        let pooling = match config.pooling_strategy() {
            crate::config::PoolingStrategy::Mean => "mean",
            crate::config::PoolingStrategy::Cls => "cls",
        };
        Self(format!(
            "v1;model={};revision={revision};onnx={onnx};dim={dim};pooling={pooling};\
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PoolingStrategy;

    #[test]
    fn pooling_change_alters_the_fingerprint() {
        let mut config = EmbeddingConfig::default();
        let mean = SemanticFingerprint::compute(&config, 384);
        config.pooling = Some(PoolingStrategy::Cls);
        let cls = SemanticFingerprint::compute(&config, 384);
        assert_ne!(mean, cls);
        assert!(mean.as_str().contains("pooling=mean"), "{mean}");
        assert!(cls.as_str().contains("pooling=cls"), "{cls}");
    }

    #[test]
    fn pinned_model_carries_its_revision_and_file_digest() {
        let config = EmbeddingConfig::default();
        let fp = SemanticFingerprint::compute(&config, 384);
        let pin = pinned_model_identity(&config.model).expect("default model is pinned");
        assert!(fp.as_str().contains(pin.revision), "{fp}");
        assert!(fp.as_str().contains(pin.onnx_sha256), "{fp}");
        assert!(fp.as_str().contains("dim=384"), "{fp}");
        assert!(
            fp.as_str().contains(&format!("chunker={CHUNKER_VERSION}")),
            "{fp}"
        );
    }

    #[test]
    fn dimension_and_model_are_part_of_the_identity() {
        let config = EmbeddingConfig::default();
        assert_ne!(
            SemanticFingerprint::compute(&config, 384),
            SemanticFingerprint::compute(&config, 768)
        );
        let mut other = config.clone();
        other.model = "custom/model".to_string();
        let fp = SemanticFingerprint::compute(&other, 384);
        assert!(fp.as_str().contains("revision=unpinned"), "{fp}");
        assert_ne!(fp, SemanticFingerprint::compute(&config, 384));
    }
}
