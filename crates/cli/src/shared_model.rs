//! Locking for model sessions the daemon pools by model key across projects.

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

/// A pooled embedder that is locked only while an internal batch runs, so a
/// watcher pass does not hold it across file reads and database writes.
pub(crate) struct SharedEmbedder(pub(crate) Arc<Mutex<Embedder>>);

impl TextEmbedder for SharedEmbedder {
    fn bind_fingerprint(&mut self, expected: &SemanticFingerprint) -> Result<()> {
        TextEmbedder::bind_fingerprint(&mut *self.0.lock(), expected)
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        embed_shared(
            &self.0,
            texts,
            |embedder, texts| embedder.plan_batches(texts),
            |embedder, batch, stats| embedder.embed_planned_batch(batch, stats),
        )
    }
}

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
