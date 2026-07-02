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
        score_in_batches(documents, RERANK_BATCH, |batch| {
            self.score_batch(query, batch)
        })
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

/// Split `documents` into `batch_size` chunks, score each with `score_batch`,
/// and concatenate the scores in input order. A batch result whose length
/// doesn't match its batch is an error: extending anyway would silently
/// attach scores to the wrong documents downstream, and letting the mismatch
/// surface as an index panic later would hang the MCP client.
fn score_in_batches<F>(
    documents: &[&str],
    batch_size: usize,
    mut score_batch: F,
) -> Result<Vec<f32>>
where
    F: FnMut(&[&str]) -> Result<Vec<f32>>,
{
    if documents.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_scores = Vec::with_capacity(documents.len());

    for batch in documents.chunks(batch_size) {
        let scores = score_batch(batch)?;
        anyhow::ensure!(
            scores.len() == batch.len(),
            "reranker returned {} scores for a batch of {} documents",
            scores.len(),
            batch.len()
        );
        all_scores.extend(scores);
    }

    Ok(all_scores)
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
    if batch_size == 0 {
        return Vec::new();
    }
    // `num_labels` is the size of the last dim ONLY for a rank-≥2 tensor. A
    // rank-1 `[batch]` output — some community re-exports squeeze the
    // `[batch, 1]` regression head — has a last dim equal to `batch_size`;
    // treating that as `num_labels` makes the row-slicing loop below read past
    // the end of `logits` and panic. In the daemon a tool-handler panic is
    // swallowed by rmcp and the client hangs, so detect the single-score case
    // by rank, not by `shape.last()`.
    let num_labels = if shape.len() <= 1 {
        1
    } else {
        shape.last().copied().filter(|n| *n > 0).unwrap_or(1) as usize
    };
    // A single-label head (or a malformed tensor whose element count doesn't
    // cover `batch * num_labels`) is read as one raw score per row. `get` keeps
    // a short/garbled tensor from panicking the handler.
    if num_labels <= 1 || logits.len() < batch_size * num_labels {
        return (0..batch_size)
            .map(|i| logits.get(i).copied().unwrap_or(0.0))
            .collect();
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
    use super::{extract_relevance_scores, score_in_batches};

    #[test]
    fn single_label_head_returns_raw_logits() {
        let logits = vec![0.9, -0.2, 1.3];
        let scores = extract_relevance_scores(&[3, 1], &logits, 3);
        assert_eq!(scores, vec![0.9, -0.2, 1.3]);
    }

    #[test]
    fn rank1_batch_output_returns_raw_logits_without_panicking() {
        // A squeezed `[batch]` regression head. Pre-fix, `num_labels` was read
        // as `shape.last()` == batch_size (3), and the row loop sliced
        // logits[3..6] on a 3-element tensor → out-of-bounds panic.
        let logits = vec![0.9, -0.2, 1.3];
        let scores = extract_relevance_scores(&[3], &logits, 3);
        assert_eq!(scores, vec![0.9, -0.2, 1.3]);
    }

    #[test]
    fn malformed_short_tensor_does_not_panic() {
        // A multi-label shape whose element count doesn't cover batch*num_labels
        // must degrade, not panic the handler (which would hang the MCP client).
        let logits = vec![0.5]; // shape claims [2,2] = 4 elements, only 1 present
        let scores = extract_relevance_scores(&[2, 2], &logits, 2);
        assert_eq!(scores.len(), 2);
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

    #[test]
    fn score_in_batches_splits_at_batch_size_and_preserves_order() {
        let docs: Vec<String> = (0..33).map(|i| format!("doc{i}")).collect();
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();

        let mut batch_lens = Vec::new();
        let scores = score_in_batches(&doc_refs, 32, |batch| {
            batch_lens.push(batch.len());
            batch
                .iter()
                .map(|d| Ok(d.trim_start_matches("doc").parse::<f32>().unwrap()))
                .collect()
        })
        .unwrap();

        assert_eq!(batch_lens, vec![32, 1]);
        let expected: Vec<f32> = (0..33).map(|i| i as f32).collect();
        assert_eq!(scores, expected);
    }

    #[test]
    fn score_in_batches_empty_input_never_calls_closure() {
        let scores = score_in_batches(&[], 32, |_| -> anyhow::Result<Vec<f32>> {
            panic!("closure must not run on empty input")
        })
        .unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn score_in_batches_wrong_length_batch_is_an_error() {
        let docs = ["a", "b", "c"];
        let err = score_in_batches(&docs, 2, |batch| Ok(vec![0.0; batch.len() + 1]))
            .expect_err("a wrong-length batch result must not be concatenated");
        assert!(
            err.to_string().contains("scores"),
            "unexpected error: {err}"
        );
    }
}
