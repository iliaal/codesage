//! Locking for model sessions the daemon pools by model key across projects.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_embed::model::{BatchPlan, BatchPlanner, EmbedStats, Embedder, Encoding};
use codesage_graph::{SemanticFingerprint, TextEmbedder};
use parking_lot::{Mutex, MutexGuard};

/// Lock a pooled model, giving up when the current `WorkControl` stops.
pub(crate) fn model_lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    loop {
        codesage_protocol::work::checkpoint()?;
        if let Some(guard) = mutex.try_lock_for(Duration::from_millis(25)) {
            codesage_protocol::work::checkpoint()?;
            return Ok(guard);
        }
    }
}

/// Pooled embedding model interface used by [`run_encoded_plan_fairly`] and
/// [`SharedEmbedder`] so concurrent callers share one model fairly per batch.
pub(crate) trait EncodedBatchModel {
    fn planner(&self) -> BatchPlanner;
    fn planner_with_limit(&self, batch_size: NonZeroUsize) -> BatchPlanner;
    fn embed_encoded_batch(
        &mut self,
        encodings: &[&Encoding],
        stats: &mut EmbedStats,
    ) -> Result<Vec<Vec<f32>>>;
}

impl EncodedBatchModel for Embedder {
    fn planner(&self) -> BatchPlanner {
        Embedder::planner(self)
    }

    fn planner_with_limit(&self, batch_size: NonZeroUsize) -> BatchPlanner {
        Embedder::planner_with_limit(self, batch_size)
    }

    fn embed_encoded_batch(
        &mut self,
        encodings: &[&Encoding],
        stats: &mut EmbedStats,
    ) -> Result<Vec<Vec<f32>>> {
        Embedder::embed_encoded_batch(self, encodings, stats)
    }
}

/// Fair per-batch scheduler for a pre-tokenized [`BatchPlan`], avoiding both
/// re-tokenization and holding the model lock across tokenization.
pub(crate) fn run_encoded_plan_fairly<M: EncodedBatchModel>(
    model: &Mutex<M>,
    plan: &BatchPlan,
    stats: &mut EmbedStats,
    lock_wait: &mut Duration,
) -> Result<Vec<Vec<f32>>> {
    plan.run_encoded(stats, |batch, stats| {
        let waiting = Instant::now();
        let mut guard = model_lock(model)?;
        *lock_wait += waiting.elapsed();
        let result = guard.embed_encoded_batch(batch, stats);
        MutexGuard::unlock_fair(guard);
        result
    })
}

/// A pooled embedder that is locked only while an internal batch runs, so a
/// watcher pass does not hold it across file reads, tokenization, and DB writes.
pub(crate) struct SharedEmbedder<M = Embedder> {
    model: Arc<Mutex<M>>,
    batch_size: Option<NonZeroUsize>,
}

impl<M> SharedEmbedder<M> {
    pub(crate) fn new(model: Arc<Mutex<M>>, batch_size: Option<NonZeroUsize>) -> Self {
        Self { model, batch_size }
    }
}

impl<M: EncodedBatchModel + TextEmbedder> TextEmbedder for SharedEmbedder<M> {
    fn bind_fingerprint(&mut self, expected: &SemanticFingerprint) -> Result<()> {
        TextEmbedder::bind_fingerprint(&mut *self.model.lock(), expected)
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let planner = {
            let guard = self.model.lock();
            match self.batch_size {
                Some(limit) => guard.planner_with_limit(limit),
                None => guard.planner(),
            }
        };
        let plan = planner.plan(texts)?;
        let mut lock_wait = Duration::ZERO;
        run_encoded_plan_fairly(
            &self.model,
            &plan,
            &mut EmbedStats::default(),
            &mut lock_wait,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_embed::model::Tokenizer;
    use std::sync::mpsc;

    fn test_word_tokenizer() -> Arc<Tokenizer> {
        Arc::new(
            Tokenizer::from_bytes(
                br#"{"version":"1.0","model":{"type":"WordLevel","vocab":{"[UNK]":0,"a":1,"b":2,"c":3,"d":4},"unk_token":"[UNK]"},"pre_tokenizer":{"type":"Whitespace"}}"#,
            )
            .unwrap(),
        )
    }

    struct FakeEncodedModel {
        tokenizer: Arc<Tokenizer>,
        dim: usize,
        default_batch_size: NonZeroUsize,
        batch_delay: Duration,
        started_tx: Option<mpsc::SyncSender<()>>,
        wait_for_contender: Option<mpsc::Receiver<()>>,
        /// First token ID of each batch's leading item, paired with batch row count.
        log: Vec<(u32, usize)>,
    }

    impl FakeEncodedModel {
        fn new(default_batch_size: usize, batch_delay: Duration) -> Self {
            Self {
                tokenizer: test_word_tokenizer(),
                dim: 384,
                default_batch_size: NonZeroUsize::new(default_batch_size).unwrap(),
                batch_delay,
                started_tx: None,
                wait_for_contender: None,
                log: Vec::new(),
            }
        }
    }

    impl EncodedBatchModel for FakeEncodedModel {
        fn planner(&self) -> BatchPlanner {
            BatchPlanner::for_dim(
                Arc::clone(&self.tokenizer),
                self.dim,
                self.default_batch_size,
            )
        }

        fn planner_with_limit(&self, batch_size: NonZeroUsize) -> BatchPlanner {
            BatchPlanner::for_dim(Arc::clone(&self.tokenizer), self.dim, batch_size)
        }

        fn embed_encoded_batch(
            &mut self,
            encodings: &[&Encoding],
            stats: &mut EmbedStats,
        ) -> Result<Vec<Vec<f32>>> {
            let tag = encodings
                .first()
                .and_then(|enc| enc.get_ids().first().copied())
                .unwrap_or(0);
            self.log.push((tag, encodings.len()));
            if let Some(tx) = self.started_tx.take() {
                let _ = tx.try_send(());
            }
            if let Some(rx) = self.wait_for_contender.take() {
                rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if !self.batch_delay.is_zero() {
                std::thread::sleep(self.batch_delay);
            }
            let out: Vec<Vec<f32>> = encodings
                .iter()
                .map(|enc| {
                    stats.tokens += enc.len();
                    let first_id = enc.get_ids().first().copied().unwrap_or(0) as f32;
                    vec![first_id, enc.len() as f32]
                })
                .collect();
            Ok(out)
        }
    }

    impl TextEmbedder for FakeEncodedModel {
        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            let plan = self.planner().plan(texts)?;
            plan.run_encoded(&mut EmbedStats::default(), |batch, stats| {
                self.embed_encoded_batch(batch, stats)
            })
        }
    }

    #[test]
    fn run_encoded_plan_fairly_interleaves_concurrent_callers_and_preserves_order() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (contender_tx, contender_rx) = mpsc::sync_channel(1);
        let mut fake = FakeEncodedModel::new(1, Duration::from_millis(60));
        fake.started_tx = Some(started_tx);
        fake.wait_for_contender = Some(contender_rx);
        let tokenizer = Arc::clone(&fake.tokenizer);
        let model = Arc::new(Mutex::new(fake));

        // Unsorted lengths force BatchPlan to execute in descending-footprint
        // order (3, 2, 1 and 4, 2, 1), proving input order is restored.
        let first_texts = ["a", "a a a", "a a"];
        let second_texts = ["b b", "b", "b b b b"];
        let first_plan =
            BatchPlanner::for_dim(Arc::clone(&tokenizer), 384, NonZeroUsize::new(1).unwrap())
                .plan(&first_texts)
                .unwrap();
        let second_plan =
            BatchPlanner::for_dim(Arc::clone(&tokenizer), 384, NonZeroUsize::new(1).unwrap())
                .plan(&second_texts)
                .unwrap();

        let first_model = Arc::clone(&model);
        let first = std::thread::spawn(move || {
            let mut stats = EmbedStats::default();
            let mut lock_wait = Duration::ZERO;
            let out =
                run_encoded_plan_fairly(&first_model, &first_plan, &mut stats, &mut lock_wait)
                    .unwrap();
            (out, stats, lock_wait)
        });

        let second_model = Arc::clone(&model);
        let second = std::thread::spawn(move || {
            // Wait until `first` holds the lock in batch 1, then signal right
            // before entering `run_encoded_plan_fairly` so thread-spawn delay
            // under heavy host load cannot let `first` finish uncontended.
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            contender_tx.send(()).unwrap();
            let mut stats = EmbedStats::default();
            let mut lock_wait = Duration::ZERO;
            let out =
                run_encoded_plan_fairly(&second_model, &second_plan, &mut stats, &mut lock_wait)
                    .unwrap();
            (out, stats, lock_wait)
        });

        let (first_out, first_stats, _) = first.join().unwrap();
        let (second_out, second_stats, second_wait) = second.join().unwrap();

        assert_eq!(
            first_out,
            vec![vec![1.0, 1.0], vec![1.0, 3.0], vec![1.0, 2.0]],
            "first caller output must preserve input order"
        );
        assert_eq!(
            second_out,
            vec![vec![2.0, 2.0], vec![2.0, 1.0], vec![2.0, 4.0]],
            "second caller output must preserve input order"
        );
        assert_eq!(
            (
                first_stats.planned_batches,
                first_stats.completed_batches,
                first_stats.tokens
            ),
            (3, 3, 6)
        );
        assert_eq!(
            (
                second_stats.planned_batches,
                second_stats.completed_batches,
                second_stats.tokens
            ),
            (3, 3, 7)
        );
        assert!(
            second_wait > Duration::ZERO,
            "second caller should record lock contention while first holds batch 1"
        );

        let log = model.lock().log.clone();
        assert_eq!(
            log.len(),
            6,
            "both callers must run 3 batches each: {log:?}"
        );
        let second_starts = log.iter().position(|&(tag, _)| tag == 2).unwrap();
        let first_ends = log.iter().rposition(|&(tag, _)| tag == 1).unwrap();
        assert!(
            second_starts < first_ends,
            "waiting caller must acquire a batch before the first finishes: {log:?}"
        );
    }

    #[test]
    fn shared_embedder_honors_per_project_planner_with_limit_override() {
        let model = Arc::new(Mutex::new(FakeEncodedModel::new(8, Duration::ZERO)));
        let texts = ["a", "b b", "c c c", "d", "a a a a"];

        let mut capped = SharedEmbedder::new(Arc::clone(&model), NonZeroUsize::new(2));
        let capped_out = capped.embed_batch(&texts).unwrap();
        assert_eq!(
            capped_out,
            vec![
                vec![1.0, 1.0],
                vec![2.0, 2.0],
                vec![3.0, 3.0],
                vec![4.0, 1.0],
                vec![1.0, 4.0],
            ]
        );
        let capped_sizes: Vec<usize> = model.lock().log.drain(..).map(|(_, size)| size).collect();
        assert_eq!(
            capped_sizes,
            vec![2, 2, 1],
            "Some(2) override must cap every internal batch at 2 rows"
        );

        let mut default_caller = SharedEmbedder::new(Arc::clone(&model), None);
        let default_out = default_caller.embed_batch(&texts).unwrap();
        assert_eq!(default_out, capped_out);
        let default_sizes: Vec<usize> = model.lock().log.drain(..).map(|(_, size)| size).collect();
        assert_eq!(
            default_sizes,
            vec![5],
            "None must use the shared model's default batch cap (8)"
        );

        let mut mid = SharedEmbedder::new(Arc::clone(&model), NonZeroUsize::new(3));
        let mid_out = mid.embed_batch(&texts).unwrap();
        assert_eq!(mid_out, capped_out);
        let mid_sizes: Vec<usize> = model.lock().log.drain(..).map(|(_, size)| size).collect();
        assert_eq!(
            mid_sizes,
            vec![3, 2],
            "distinct projects sharing one session must each apply their own limit"
        );
    }
}
