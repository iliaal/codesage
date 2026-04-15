use anyhow::{Context, Result};
use ort::session::Session;
use tokenizers::Tokenizer;

use crate::config::MAX_SEQ_LENGTH;
use crate::model::init_ort_dylib;

const RERANK_BATCH: usize = 32;

pub struct Reranker {
    session: Session,
    tokenizer: Tokenizer,
    has_token_type_ids: bool,
}

impl Reranker {
    pub fn new(model: &str, device: &str) -> Result<Self> {
        init_ort_dylib();
        eprintln!("Loading reranker model: {model}");

        let api =
            hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
        let repo = api.model(model.to_string());

        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("failed to download tokenizer.json")?;
        let model_path = repo
            .get("onnx/model.onnx")
            .context("failed to download onnx/model.onnx")?;

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LENGTH,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let mut builder = Session::builder()?;

        if device == "gpu" || device == "cuda" {
            #[cfg(feature = "cuda")]
            {
                super::model::preload_cuda_libs();
                builder = builder
                    .with_execution_providers([
                        ort::execution_providers::CUDAExecutionProvider::default()
                            .build()
                            .error_on_failure(),
                    ])
                    .map_err(|e| anyhow::anyhow!("CUDA provider failed to register: {e}"))?;
            }
        }

        let session = builder.commit_from_file(&model_path)?;

        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");

        eprintln!("Reranker loaded (token_type_ids={has_token_type_ids})");

        Ok(Self {
            session,
            tokenizer,
            has_token_type_ids,
        })
    }

    pub fn score_pairs(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_scores = Vec::with_capacity(documents.len());

        for batch_start in (0..documents.len()).step_by(RERANK_BATCH) {
            let batch_end = (batch_start + RERANK_BATCH).min(documents.len());
            let batch = &documents[batch_start..batch_end];
            let scores = self.score_batch(query, batch)?;
            all_scores.extend(scores);
        }

        Ok(all_scores)
    }

    fn score_batch(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        let pairs: Vec<(String, String)> = documents
            .iter()
            .map(|doc| (query.to_string(), doc.to_string()))
            .collect();

        let pair_refs: Vec<(&str, &str)> = pairs.iter().map(|(q, d)| (q.as_str(), d.as_str())).collect();

        let encodings = self
            .tokenizer
            .encode_batch(pair_refs, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        let mut input_ids = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask = Vec::with_capacity(batch_size * seq_len);
        let mut token_type_ids_vec = Vec::with_capacity(batch_size * seq_len);

        for enc in &encodings {
            input_ids.extend(enc.get_ids().iter().map(|&id| id as i64));
            attention_mask.extend(enc.get_attention_mask().iter().map(|&m| m as i64));
            token_type_ids_vec.extend(enc.get_type_ids().iter().map(|&t| t as i64));
        }

        let ids_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], input_ids))?;
        let mask_tensor = ort::value::Tensor::from_array(([batch_size, seq_len], attention_mask))?;

        let outputs = if self.has_token_type_ids {
            let type_tensor =
                ort::value::Tensor::from_array(([batch_size, seq_len], token_type_ids_vec))?;
            self.session.run(ort::inputs![
                "input_ids" => ids_tensor,
                "token_type_ids" => type_tensor,
                "attention_mask" => mask_tensor,
            ])?
        } else {
            self.session.run(ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
            ])?
        };

        let (_shape, logits) = outputs[0].try_extract_tensor::<f32>()?;

        let scores: Vec<f32> = (0..batch_size).map(|i| logits[i]).collect();
        Ok(scores)
    }
}
