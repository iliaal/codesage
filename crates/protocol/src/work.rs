//! Synchronous work cancellation shared by database, graph, and native workers.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    ClientCancelled = 1,
    ConnectionClosed,
    DeadlineExceeded,
    NoConsumers,
    Shutdown,
    Incomplete,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientCancelled | Self::ConnectionClosed | Self::NoConsumers => "cancelled",
            Self::DeadlineExceeded => "timeout",
            Self::Shutdown => "shutdown",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkStopped {
    pub reason: StopReason,
}

impl std::fmt::Display for WorkStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "work stopped: {:?}", self.reason)
    }
}

impl std::error::Error for WorkStopped {}

type Callback = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    reason: AtomicU8,
    deadline: Option<Instant>,
    next_callback: AtomicU64,
    callbacks: Mutex<BTreeMap<u64, Callback>>,
    lease: Mutex<Option<Weak<dyn Send + Sync>>>,
}

#[derive(Clone)]
pub struct WorkControl(Arc<Inner>);

#[derive(Clone)]
pub struct WeakWorkControl(Weak<Inner>);

impl WeakWorkControl {
    pub fn upgrade(&self) -> Option<WorkControl> {
        self.0.upgrade().map(WorkControl)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<WorkControl>> = const { RefCell::new(None) };
}

impl WorkControl {
    pub fn downgrade(&self) -> WeakWorkControl {
        WeakWorkControl(Arc::downgrade(&self.0))
    }
    pub fn new(deadline: Option<Instant>) -> Self {
        Self(Arc::new(Inner {
            reason: AtomicU8::new(0),
            deadline,
            next_callback: AtomicU64::new(1),
            callbacks: Mutex::new(BTreeMap::new()),
            lease: Mutex::new(None),
        }))
    }

    pub fn cancel(&self, reason: StopReason) {
        if self
            .0
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let callbacks: Vec<_> = self
                .0
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect();
            for callback in callbacks {
                callback();
            }
        }
    }

    pub fn reason(&self) -> Option<StopReason> {
        if self
            .0
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.cancel(StopReason::DeadlineExceeded);
        }
        match self.0.reason.load(Ordering::Acquire) {
            1 => Some(StopReason::ClientCancelled),
            2 => Some(StopReason::ConnectionClosed),
            3 => Some(StopReason::DeadlineExceeded),
            4 => Some(StopReason::NoConsumers),
            5 => Some(StopReason::Shutdown),
            6 => Some(StopReason::Incomplete),
            _ => None,
        }
    }

    pub fn check(&self) -> Result<(), WorkStopped> {
        match self.reason() {
            Some(reason) => Err(WorkStopped { reason }),
            None => Ok(()),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.0.deadline
    }

    pub fn enter(&self) -> WorkScope {
        let previous = CURRENT.with(|slot| slot.replace(Some(self.clone())));
        WorkScope {
            previous,
            marker: PhantomData,
        }
    }

    pub fn on_cancel(&self, callback: Callback) -> CancelRegistration {
        let id = self.0.next_callback.fetch_add(1, Ordering::Relaxed);
        {
            let mut callbacks = self.0.callbacks.lock().unwrap_or_else(|e| e.into_inner());
            if self.0.reason.load(Ordering::Acquire) == 0 {
                callbacks.insert(id, callback.clone());
            } else {
                drop(callbacks);
                callback();
            }
        }
        CancelRegistration {
            inner: Arc::downgrade(&self.0),
            id,
        }
    }

    pub fn set_work_lease(&self, lease: Weak<dyn Send + Sync>) {
        *self.0.lease.lock().unwrap_or_else(|e| e.into_inner()) = Some(lease);
    }

    /// Clone before spawning a nested worker and retain until that worker exits.
    pub fn worker_lease(&self) -> Option<Arc<dyn Send + Sync>> {
        self.0
            .lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
    }
}

pub struct CancelRegistration {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
        }
    }
}

pub struct WorkScope {
    previous: Option<WorkControl>,
    marker: PhantomData<Rc<()>>,
}

impl Drop for WorkScope {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

pub fn current() -> Option<WorkControl> {
    CURRENT.with(|slot| slot.borrow().clone())
}

pub fn checkpoint() -> Result<(), WorkStopped> {
    current().map_or(Ok(()), |control| control.check())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn cancellation_is_sticky_and_callbacks_unregister() {
        let control = WorkControl::new(None);
        let called = Arc::new(AtomicUsize::new(0));
        let count = called.clone();
        let registration = control.on_cancel(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        drop(registration);
        control.cancel(StopReason::ClientCancelled);
        control.cancel(StopReason::Shutdown);
        assert_eq!(called.load(Ordering::Relaxed), 0);
        let count = called.clone();
        let _late = control.on_cancel(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(called.load(Ordering::Relaxed), 1);
        assert_eq!(
            control.check().unwrap_err().reason,
            StopReason::ClientCancelled
        );
    }

    #[test]
    fn deadline_and_nested_scopes_restore_control() {
        let outer = WorkControl::new(None);
        let _outer_scope = outer.enter();
        {
            let expired = WorkControl::new(Some(Instant::now()));
            let _inner_scope = expired.enter();
            assert_eq!(
                checkpoint().unwrap_err().reason,
                StopReason::DeadlineExceeded
            );
        }
        checkpoint().unwrap();
        outer.cancel(StopReason::Shutdown);
        assert_eq!(checkpoint().unwrap_err().reason, StopReason::Shutdown);
    }

    #[test]
    fn nested_worker_holds_lease_until_actual_exit() {
        let control = WorkControl::new(None);
        let lease: Arc<dyn Send + Sync> = Arc::new(());
        let weak_lease = Arc::downgrade(&lease);
        control.set_work_lease(weak_lease.clone());
        let weak_control = control.downgrade();
        let nested = control.worker_lease().unwrap();
        drop(lease);
        assert!(weak_lease.upgrade().is_some());
        drop(nested);
        assert!(weak_lease.upgrade().is_none());
        drop(control);
        assert!(weak_control.upgrade().is_none());
    }

    #[test]
    fn registration_racing_cancellation_calls_once() {
        for _ in 0..100 {
            let control = WorkControl::new(None);
            let called = Arc::new(AtomicUsize::new(0));
            let count = called.clone();
            let cancel = control.clone();
            let thread = std::thread::spawn(move || cancel.cancel(StopReason::Shutdown));
            let _registration = control.on_cancel(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }));
            thread.join().unwrap();
            assert_eq!(called.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn cancellation_callbacks_can_reenter_control() {
        let control = WorkControl::new(None);
        let weak = control.downgrade();
        let called = Arc::new(AtomicUsize::new(0));
        let count = called.clone();
        let _registration = control.on_cancel(Arc::new(move || {
            let control = weak.upgrade().unwrap();
            control.cancel(StopReason::Shutdown);
            let count = count.clone();
            let _nested = control.on_cancel(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }));
        }));
        control.cancel(StopReason::ClientCancelled);
        assert_eq!(called.load(Ordering::Relaxed), 1);
        assert_eq!(control.reason(), Some(StopReason::ClientCancelled));
    }
}
