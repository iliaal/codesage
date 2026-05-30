use anyhow::Result;
use ort::session::Session;
use std::sync::atomic::{AtomicBool, Ordering};
use tokenizers::Tokenizer;

use crate::model::load_onnx_session;

const RERANK_BATCH: usize = 32;

pub struct Reranker {
    session: Session,
    tokenizer: Tokenizer,
    has_token_type_ids: bool,
    shape_logged: AtomicBool,
}

impl Reranker {
    pub fn new(model: &str, device: &str) -> Result<Self> {
        tracing::info!(%model, "loading reranker model");
        let (session, tokenizer, has_token_type_ids) = load_onnx_session(model, device)?;
        tracing::info!(token_type_ids = has_token_type_ids, "reranker loaded");

        Ok(Self {
            session,
            tokenizer,
            has_token_type_ids,
            shape_logged: AtomicBool::new(false),
        })
    }

    pub fn score_pairs(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_scores = Vec::with_capacity(documents.len());

        for batch in documents.chunks(RERANK_BATCH) {
            all_scores.extend(self.score_batch(query, batch)?);
        }

        Ok(all_scores)
    }

    fn score_batch(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        let pair_refs: Vec<(&str, &str)> = documents.iter().map(|doc| (query, *doc)).collect();

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

        let (shape, logits) = outputs[0].try_extract_tensor::<f32>()?;
        if !self.shape_logged.swap(true, Ordering::Relaxed) {
            tracing::info!(output_shape = ?&shape[..], "reranker output shape detected");
        }
        Ok(extract_relevance_scores(&shape[..], logits, batch_size))
    }
}

/// Pull one relevance score per query/document pair out of the cross-encoder
/// output tensor.
///
/// Cross-encoders fall into two shapes. `ms-marco-*` regression heads emit
/// `[batch, 1]` and the single column is the raw relevance score. Two-class
/// (and occasionally NLI-style three-class) heads emit `[batch, num_labels]`;
/// the positive-relevant class lives in the last column by convention. The
/// previous version of this function read `logits[i]` over the flat tensor,
/// which happens to be correct for `num_labels == 1` and silently scrambles
/// scores for everything else.
fn extract_relevance_scores(shape: &[i64], logits: &[f32], batch_size: usize) -> Vec<f32> {
    let num_labels = shape.last().copied().filter(|n| *n > 0).unwrap_or(1) as usize;
    if num_labels <= 1 {
        return (0..batch_size).map(|i| logits[i]).collect();
    }
    let mut out = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
        let row = &logits[i * num_labels..(i + 1) * num_labels];
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut sum = 0.0f32;
        for &x in row {
            sum += (x - max).exp();
        }
        let pos = row[num_labels - 1] - max;
        out.push(pos.exp() / sum);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::extract_relevance_scores;

    #[test]
    fn single_label_head_returns_raw_logits() {
        let logits = vec![0.9, -0.2, 1.3];
        let scores = extract_relevance_scores(&[3, 1], &logits, 3);
        assert_eq!(scores, vec![0.9, -0.2, 1.3]);
    }

    #[test]
    fn binary_classifier_head_returns_positive_class_softmax() {
        // Row 0: [neg=2.0, pos=0.0] → very low relevance.
        // Row 1: [neg=0.0, pos=2.0] → very high relevance.
        // Previous (buggy) implementation would have returned [2.0, 0.0],
        // which is row 0's neg logit and row 1's neg logit — both wrong.
        let logits = vec![2.0, 0.0, 0.0, 2.0];
        let scores = extract_relevance_scores(&[2, 2], &logits, 2);
        assert!(scores[0] < 0.15, "expected low pos prob, got {}", scores[0]);
        assert!(
            scores[1] > 0.85,
            "expected high pos prob, got {}",
            scores[1]
        );
    }

    #[test]
    fn three_class_head_takes_last_column_softmax() {
        // [contradict=0, neutral=0, entail=3] — the entail column is last by
        // NLI convention; softmax over the row should dominate.
        let logits = vec![0.0, 0.0, 3.0];
        let scores = extract_relevance_scores(&[1, 3], &logits, 1);
        assert!(scores[0] > 0.85);
    }
}
