//! Locking for model sessions the daemon pools by model key across projects.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_embed::model::{BatchPlan, EmbedStats, Embedder};
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

/// Take the pooled model per internal batch and hand it to the next waiter
/// between batches, so one long embedding shares the model with other
/// holders instead of keeping it until it finishes.
#[cfg(test)]
pub(crate) fn run_plan_fairly<T>(
    model: &Mutex<T>,
    plan: &BatchPlan,
    texts: &[&str],
    stats: &mut EmbedStats,
    lock_wait: &mut Duration,
    mut embed: impl FnMut(&mut T, &[&str], &mut EmbedStats) -> Result<Vec<Vec<f32>>>,
) -> Result<Vec<Vec<f32>>> {
    plan.run(texts, stats, |batch, stats| {
        let waiting = Instant::now();
        let mut guard = model_lock(model)?;
        *lock_wait += waiting.elapsed();
        let result = embed(&mut guard, batch, stats);
        MutexGuard::unlock_fair(guard);
        result
    })
}

/// Fair per-batch scheduler for a pre-tokenized [`BatchPlan`], avoiding both
/// re-tokenization and holding the model lock across tokenization.
pub(crate) fn run_encoded_plan_fairly(
    model: &Mutex<Embedder>,
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
pub(crate) struct SharedEmbedder {
    model: Arc<Mutex<Embedder>>,
    batch_size: Option<NonZeroUsize>,
}

impl SharedEmbedder {
    pub(crate) fn new(model: Arc<Mutex<Embedder>>, batch_size: Option<NonZeroUsize>) -> Self {
        Self { model, batch_size }
    }
}

impl TextEmbedder for SharedEmbedder {
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
/// Plan under a brief lock, then embed each batch under its own lock.
fn embed_shared<T>(
    model: &Mutex<T>,
    texts: &[&str],
    plan: impl FnOnce(&T, &[&str]) -> BatchPlan,
    embed: impl FnMut(&mut T, &[&str], &mut EmbedStats) -> Result<Vec<Vec<f32>>>,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let plan = plan(&model.lock(), texts);
    let mut lock_wait = Duration::ZERO;
    run_plan_fairly(
        model,
        &plan,
        texts,
        &mut EmbedStats::default(),
        &mut lock_wait,
        embed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn concurrent_holders_interleave_per_batch() {
        let model: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let texts = ["a", "b", "c"];
        let request = |label: &'static str, started: Option<mpsc::SyncSender<()>>| {
            let model = Arc::clone(&model);
            std::thread::spawn(move || {
                embed_shared(
                    &model,
                    &texts,
                    |_, texts| BatchPlan::new(texts, 1, usize::MAX, usize::MAX),
                    |log, batch, _| {
                        log.push(label);
                        if let Some(started) = &started {
                            let _ = started.try_send(());
                        }
                        std::thread::sleep(Duration::from_millis(30));
                        Ok(batch.iter().map(|_| vec![0.0]).collect())
                    },
                )
                .unwrap()
            })
        };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let first = request("first", Some(started_tx));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = request("second", None);
        first.join().unwrap();
        second.join().unwrap();

        let log = model.lock().clone();
        assert_eq!(log.len(), 6);
        let second_starts = log.iter().position(|&l| l == "second").unwrap();
        let first_ends = log.iter().rposition(|&l| l == "first").unwrap();
        assert!(
            second_starts < first_ends,
            "the waiting holder must get a batch before the first finishes: {log:?}"
        );
    }
}
