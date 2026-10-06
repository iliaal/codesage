//! Ties one ONNX Runtime run to the calling thread's `WorkControl`.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;
use codesage_protocol::work::{self, WorkControl};
use ort::session::{RunOptions, Session, SessionInputValue, SessionOutputs};

/// Run `session` and hand its outputs to `read`. Under a scoped
/// `WorkControl` the run gets its own `RunOptions`, and a cancellation sets
/// its terminate flag, so a stopped request leaves the graph at the next node
/// instead of finishing a run nobody is waiting for. Without a control (CLI
/// indexing, the watcher) this is a plain `Session::run`.
pub(crate) fn run_session<T>(
    session: &mut Session,
    inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)>,
    read: impl FnOnce(&SessionOutputs<'_>) -> Result<T>,
) -> Result<T> {
    let Some(control) = work::current() else {
        let outputs = session.run(inputs)?;
        return read(&outputs);
    };
    let options = Arc::new(RunOptions::new()?);
    let terminator = Arc::clone(&options);
    let outputs = run_guarded(
        &control,
        move || {
            if let Err(error) = terminator.terminate() {
                tracing::warn!(%error, "could not set the ONNX Runtime terminate flag");
            }
        },
        || Ok(session.run_with_options(inputs, &*options)?),
    )?;
    read(&outputs)
}

/// Call `terminate` if `control` is cancelled while `run` executes, and
/// report a run that failed after cancellation as `WorkStopped`, since the
/// failure is then the termination rather than a model fault.
fn run_guarded<T>(
    control: &WorkControl,
    terminate: impl Fn() + Send + Sync + 'static,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    control.check()?;
    let _registration = control.on_cancel(Arc::new(terminate));
    // `on_cancel` runs the callback at once for a cancellation that landed
    // after the first check; stop here rather than start a terminated run.
    control.check()?;
    match run() {
        Ok(value) => Ok(value),
        Err(error) => {
            match control.check() {
                Err(stopped) => Err(anyhow::Error::new(stopped)
                    .context(format!("native inference stopped: {error:#}"))),
                Ok(()) => Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::work::{StopReason, WorkStopped};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + Sync + 'static) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        (count, move || {
            seen.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn cancel_during_run_terminates_and_reports_work_stopped() {
        let control = WorkControl::new(None);
        let (terminated, terminate) = counter();
        let error = run_guarded(&control, terminate, || -> Result<()> {
            control.cancel(StopReason::DeadlineExceeded);
            assert_eq!(terminated.load(Ordering::SeqCst), 1);
            anyhow::bail!("Exiting due to terminate flag being set to true.")
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkStopped>().unwrap().reason,
            StopReason::DeadlineExceeded
        );
        assert!(format!("{error:#}").contains("terminate flag"));
    }

    #[test]
    fn cancelled_control_never_starts_the_run() {
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let (_, terminate) = counter();
        let error = run_guarded(&control, terminate, || -> Result<()> {
            panic!("a cancelled request must not start native inference")
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkStopped>().unwrap().reason,
            StopReason::ClientCancelled
        );
    }

    #[test]
    fn finished_run_unregisters_and_keeps_model_errors() {
        let control = WorkControl::new(None);
        let (terminated, terminate) = counter();
        let error = run_guarded(&control, terminate, || -> Result<()> {
            anyhow::bail!("shape mismatch")
        })
        .unwrap_err();
        assert!(error.downcast_ref::<WorkStopped>().is_none());
        control.cancel(StopReason::ClientCancelled);
        assert_eq!(terminated.load(Ordering::SeqCst), 0);
    }
}
