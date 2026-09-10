use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use codesage_protocol::work::{StopReason, WeakWorkControl, WorkControl};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkClass {
    Interactive,
    Analysis,
    Native,
}

impl WorkClass {
    fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Analysis => 1,
            Self::Native => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct LaneLimits {
    pub running: usize,
    pub running_per_project: usize,
    pub queued: usize,
    pub queued_per_project: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct WorkLimits {
    pub requests: usize,
    pub requests_per_project: usize,
    pub interactive: LaneLimits,
    pub analysis: LaneLimits,
    pub native: LaneLimits,
}

impl Default for WorkLimits {
    fn default() -> Self {
        Self {
            requests: 64,
            requests_per_project: 32,
            interactive: LaneLimits {
                running: 2,
                running_per_project: 1,
                queued: 64,
                queued_per_project: 64,
            },
            analysis: LaneLimits {
                running: 2,
                running_per_project: 1,
                queued: 32,
                queued_per_project: 32,
            },
            native: LaneLimits {
                running: 1,
                running_per_project: 1,
                queued: 16,
                queued_per_project: 16,
            },
        }
    }
}

impl WorkLimits {
    fn lane(self, class: WorkClass) -> LaneLimits {
        match class {
            WorkClass::Interactive => self.interactive,
            WorkClass::Analysis => self.analysis,
            WorkClass::Native => self.native,
        }
    }

    fn validate(self) -> Result<(), AdmissionError> {
        if self.requests == 0
            || self.requests_per_project == 0
            || self.requests_per_project > self.requests
        {
            return Err(AdmissionError::InvalidLimits);
        }
        for lane in [self.interactive, self.analysis, self.native] {
            if lane.running == 0
                || lane.running_per_project == 0
                || lane.running_per_project > lane.running
                || lane.queued == 0
                || lane.queued_per_project == 0
                || lane.queued_per_project > lane.queued
            {
                return Err(AdmissionError::InvalidLimits);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionError {
    Saturated,
    Shutdown,
    Stopped(StopReason),
    InvalidLimits,
    ProjectAlreadyAttached,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saturated => f.write_str("daemon work capacity is saturated"),
            Self::Shutdown => f.write_str("daemon work admission is shut down"),
            Self::Stopped(reason) => write!(f, "daemon work stopped: {reason:?}"),
            Self::InvalidLimits => f.write_str("invalid daemon work limits"),
            Self::ProjectAlreadyAttached => {
                f.write_str("request already belongs to another project")
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

#[derive(Clone)]
pub(crate) struct WorkCoordinator {
    inner: Arc<Inner>,
}

struct Inner {
    limits: WorkLimits,
    state: Mutex<State>,
    changed: Arc<Notify>,
    #[cfg(test)]
    admission_hook: Mutex<Option<AdmissionHook>>,
}

#[cfg(test)]
type AdmissionHook = Arc<dyn Fn(bool) + Send + Sync>;

#[cfg(test)]
impl Inner {
    fn admission_checkpoint(&self, admitted: bool) {
        let hook = self.admission_hook.lock().clone();
        if let Some(hook) = hook {
            hook(admitted);
        }
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    next_id: u64,
    requests: HashMap<u64, WeakWorkControl>,
    projects: HashMap<PathBuf, ProjectCounts>,
    queued: VecDeque<Pending>,
    running: HashMap<u64, Running>,
    last_project: [Option<PathBuf>; 3],
}

#[derive(Default)]
struct ProjectCounts {
    requests: usize,
    queued: [usize; 3],
    running: [usize; 3],
}

struct Pending {
    id: u64,
    project: PathBuf,
    class: WorkClass,
    control: WeakWorkControl,
    phase: Arc<AtomicU8>,
}

const PENDING: u8 = 0;
const ADMITTED: u8 = 1;
const STOPPED: u8 = 2;

struct Running {
    project: PathBuf,
    class: WorkClass,
    control: WeakWorkControl,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkSnapshot {
    pub closed: bool,
    pub requests: usize,
    pub projects: usize,
    pub queued: [usize; 3],
    pub running: [usize; 3],
    pub limits: WorkLimits,
}

pub(crate) struct RequestLease {
    inner: Arc<Inner>,
    id: u64,
    project: Option<PathBuf>,
}

pub(crate) struct ExecutionLease {
    inner: Arc<Inner>,
    id: u64,
}

impl fmt::Debug for ExecutionLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionLease")
            .field("id", &self.id)
            .finish()
    }
}

struct PendingLease {
    inner: Arc<Inner>,
    id: u64,
}

impl WorkCoordinator {
    pub fn new(limits: WorkLimits) -> Result<Self, AdmissionError> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State::default()),
                changed: Arc::new(Notify::new()),
                #[cfg(test)]
                admission_hook: Mutex::new(None),
            }),
        })
    }

    pub fn try_request(&self, control: &WorkControl) -> Result<RequestLease, AdmissionError> {
        check_control(control)?;
        let mut state = self.inner.state.lock();
        if state.closed {
            return Err(AdmissionError::Shutdown);
        }
        if state.requests.len() == self.inner.limits.requests {
            return Err(AdmissionError::Saturated);
        }
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(AdmissionError::Saturated)?;
        let id = state.next_id;
        state.requests.insert(id, control.downgrade());
        Ok(RequestLease {
            inner: self.inner.clone(),
            id,
            project: None,
        })
    }

    pub async fn acquire_execution(
        &self,
        project: &Path,
        class: WorkClass,
        control: &WorkControl,
    ) -> Result<Arc<ExecutionLease>, AdmissionError> {
        check_control(control)?;
        let changed = self.inner.changed.clone();
        let phase = Arc::new(AtomicU8::new(PENDING));
        let callback_phase = phase.clone();
        let _registration = control.on_cancel(Arc::new(move || {
            let _ = callback_phase.compare_exchange(
                PENDING,
                STOPPED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            changed.notify_waiters();
        }));
        let pending = {
            let mut state = self.inner.state.lock();
            if state.closed {
                return Err(AdmissionError::Shutdown);
            }
            let lane = self.inner.limits.lane(class);
            let queued = state
                .queued
                .iter()
                .filter(|entry| entry.class == class)
                .count();
            let project_queued = state
                .projects
                .get(project)
                .map_or(0, |p| p.queued[class.index()]);
            if queued >= lane.queued || project_queued >= lane.queued_per_project {
                return Err(AdmissionError::Saturated);
            }
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or(AdmissionError::Saturated)?;
            let id = state.next_id;
            state.projects.entry(project.to_owned()).or_default().queued[class.index()] += 1;
            state.queued.push_back(Pending {
                id,
                project: project.to_owned(),
                class,
                control: control.downgrade(),
                phase: phase.clone(),
            });
            PendingLease {
                inner: self.inner.clone(),
                id,
            }
        };
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            check_control(control)?;
            {
                let mut state = self.inner.state.lock();
                if state.closed {
                    return Err(AdmissionError::Shutdown);
                }
                if state.eligible(class, self.inner.limits.lane(class)) == Some(pending.id) {
                    #[cfg(test)]
                    self.inner.admission_checkpoint(false);
                    if phase
                        .compare_exchange(PENDING, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        continue;
                    }
                    #[cfg(test)]
                    self.inner.admission_checkpoint(true);
                    let position = state
                        .queued
                        .iter()
                        .position(|entry| entry.id == pending.id)
                        .expect("eligible work remains queued under the same lock");
                    let entry = state
                        .queued
                        .remove(position)
                        .expect("queued position exists");
                    let project_counts = state
                        .projects
                        .get_mut(&entry.project)
                        .expect("queued work owns a project record");
                    project_counts.queued[class.index()] -= 1;
                    project_counts.running[class.index()] += 1;
                    state.last_project[class.index()] = Some(entry.project.clone());
                    state.running.insert(
                        entry.id,
                        Running {
                            project: entry.project,
                            class,
                            control: entry.control,
                        },
                    );
                    self.inner.changed.notify_waiters();
                    return Ok(Arc::new(ExecutionLease {
                        inner: self.inner.clone(),
                        id: pending.id,
                    }));
                }
            }
            match control.deadline() {
                Some(deadline) => {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(deadline.into()) => {
                            control.cancel(StopReason::DeadlineExceeded);
                        }
                    }
                }
                None => notified.await,
            }
        }
    }

    pub fn shutdown(&self) {
        let controls = {
            let mut state = self.inner.state.lock();
            state.closed = true;
            state
                .requests
                .values()
                .filter_map(WeakWorkControl::upgrade)
                .chain(
                    state
                        .queued
                        .iter()
                        .filter_map(|entry| entry.control.upgrade()),
                )
                .chain(
                    state
                        .running
                        .values()
                        .filter_map(|entry| entry.control.upgrade()),
                )
                .collect::<Vec<_>>()
        };
        for control in controls {
            control.cancel(StopReason::Shutdown);
        }
        self.inner.changed.notify_waiters();
    }

    pub fn snapshot(&self) -> WorkSnapshot {
        let state = self.inner.state.lock();
        let mut queued = [0; 3];
        let mut running = [0; 3];
        for entry in &state.queued {
            queued[entry.class.index()] += 1;
        }
        for entry in state.running.values() {
            running[entry.class.index()] += 1;
        }
        WorkSnapshot {
            closed: state.closed,
            requests: state.requests.len(),
            projects: state.projects.len(),
            queued,
            running,
            limits: self.inner.limits,
        }
    }
}

fn check_control(control: &WorkControl) -> Result<(), AdmissionError> {
    control
        .check()
        .map_err(|stopped| AdmissionError::Stopped(stopped.reason))
}

impl State {
    fn eligible(&self, class: WorkClass, limits: LaneLimits) -> Option<u64> {
        if self
            .running
            .values()
            .filter(|entry| entry.class == class)
            .count()
            >= limits.running
        {
            return None;
        }
        let eligible = |entry: &&Pending| {
            entry.class == class
                && self.projects.get(&entry.project).is_some_and(|counts| {
                    counts.running[class.index()] < limits.running_per_project
                })
                && entry.phase.load(Ordering::Acquire) == PENDING
                && entry.control.upgrade().is_some()
        };
        self.queued
            .iter()
            .filter(eligible)
            .find(|entry| self.last_project[class.index()].as_ref() != Some(&entry.project))
            .or_else(|| self.queued.iter().find(eligible))
            .map(|entry| entry.id)
    }

    fn prune_project(&mut self, project: &Path) {
        if self.projects.get(project).is_some_and(|counts| {
            counts.requests == 0 && counts.queued == [0; 3] && counts.running == [0; 3]
        }) {
            self.projects.remove(project);
        }
    }
}

impl RequestLease {
    pub fn attach_project(&mut self, project: &Path) -> Result<(), AdmissionError> {
        if let Some(attached) = &self.project {
            return if attached == project {
                Ok(())
            } else {
                Err(AdmissionError::ProjectAlreadyAttached)
            };
        }
        let mut state = self.inner.state.lock();
        if state.closed {
            return Err(AdmissionError::Shutdown);
        }
        if state
            .projects
            .get(project)
            .is_some_and(|counts| counts.requests >= self.inner.limits.requests_per_project)
        {
            return Err(AdmissionError::Saturated);
        }
        state
            .projects
            .entry(project.to_owned())
            .or_default()
            .requests += 1;
        self.project = Some(project.to_owned());
        Ok(())
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.requests.remove(&self.id);
        if let Some(project) = &self.project {
            if let Some(counts) = state.projects.get_mut(project) {
                counts.requests -= 1;
            }
            state.prune_project(project);
        }
    }
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        if let Some(position) = state.queued.iter().position(|entry| entry.id == self.id) {
            let entry = state
                .queued
                .remove(position)
                .expect("queued position exists");
            if let Some(counts) = state.projects.get_mut(&entry.project) {
                counts.queued[entry.class.index()] -= 1;
            }
            state.prune_project(&entry.project);
        }
        self.inner.changed.notify_waiters();
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        if let Some(entry) = state.running.remove(&self.id) {
            if let Some(counts) = state.projects.get_mut(&entry.project) {
                counts.running[entry.class.index()] -= 1;
            }
            state.prune_project(&entry.project);
        }
        self.inner.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn coordinator() -> WorkCoordinator {
        let lane = LaneLimits {
            running: 1,
            running_per_project: 1,
            queued: 4,
            queued_per_project: 2,
        };
        WorkCoordinator::new(WorkLimits {
            requests: 4,
            requests_per_project: 2,
            interactive: lane,
            analysis: lane,
            native: lane,
        })
        .unwrap()
    }

    async fn wait_queued(coordinator: &WorkCoordinator, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while coordinator.snapshot().queued.iter().sum::<usize>() != count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("work must reach the queued state");
    }

    fn enqueue(
        coordinator: &WorkCoordinator,
        project: &'static str,
        control: &WorkControl,
    ) -> tokio::task::JoinHandle<Result<Arc<ExecutionLease>, AdmissionError>> {
        let coordinator = coordinator.clone();
        let control = control.clone();
        tokio::spawn(async move {
            coordinator
                .acquire_execution(Path::new(project), WorkClass::Analysis, &control)
                .await
        })
    }

    #[test]
    fn global_admission_precedes_project_attachment() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let mut first = coordinator.try_request(&control).unwrap();
        first.attach_project(Path::new("/a")).unwrap();
        first.attach_project(Path::new("/a")).unwrap();
        assert_eq!(
            first.attach_project(Path::new("/b")),
            Err(AdmissionError::ProjectAlreadyAttached)
        );
        let mut second = coordinator.try_request(&control).unwrap();
        second.attach_project(Path::new("/a")).unwrap();
        let mut third = coordinator.try_request(&control).unwrap();
        assert_eq!(
            third.attach_project(Path::new("/a")),
            Err(AdmissionError::Saturated)
        );
        third.attach_project(Path::new("/b")).unwrap();
        let fourth = coordinator.try_request(&control).unwrap();
        assert!(matches!(
            coordinator.try_request(&control),
            Err(AdmissionError::Saturated)
        ));
        drop((first, second, third, fourth));
        assert_eq!(coordinator.snapshot().requests, 0);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn logical_waiters_do_not_consume_single_execution_slot() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let first = coordinator.try_request(&control).unwrap();
        let second = coordinator.try_request(&control).unwrap();
        let execution = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        assert_eq!(coordinator.snapshot().requests, 2);
        assert_eq!(coordinator.snapshot().running, [0, 1, 0]);
        drop((first, second, execution));
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn default_preflight_queue_accepts_sixteen_admitted_requests() {
        let coordinator = WorkCoordinator::new(WorkLimits::default()).unwrap();
        let control = WorkControl::new(None);
        let first_request = coordinator.try_request(&control).unwrap();
        let first_execution = coordinator
            .acquire_execution(Path::new("<preflight>"), WorkClass::Interactive, &control)
            .await
            .unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let control = WorkControl::new(None);
            let request = coordinator.try_request(&control).unwrap();
            let coordinator = coordinator.clone();
            tasks.spawn(async move {
                let _request = request;
                let execution = coordinator
                    .acquire_execution(Path::new("<preflight>"), WorkClass::Interactive, &control)
                    .await?;
                drop(execution);
                Ok::<_, AdmissionError>(())
            });
        }
        wait_queued(&coordinator, 16).await;
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.requests, 17);
        assert_eq!(snapshot.queued, [16, 0, 0]);
        assert_eq!(snapshot.running, [1, 0, 0]);
        assert!(snapshot.requests <= snapshot.limits.requests);
        assert!(snapshot.queued[0] <= snapshot.limits.interactive.queued);
        drop((first_request, first_execution));
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(result) = tasks.join_next().await {
                result.unwrap().unwrap();
            }
        })
        .await
        .expect("all admitted preflight work must drain");
        assert_eq!(coordinator.snapshot().requests, 0);
        assert_eq!(coordinator.snapshot().queued, [0; 3]);
        assert_eq!(coordinator.snapshot().running, [0; 3]);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn separate_lanes_preserve_interactive_capacity() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let analysis = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let native = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Native, &control)
            .await
            .unwrap();
        let interactive = tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.acquire_execution(Path::new("/a"), WorkClass::Interactive, &control),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(coordinator.snapshot().running, [1, 1, 1]);
        drop((analysis, native, interactive));
        assert_eq!(coordinator.snapshot().running, [0; 3]);
    }

    #[tokio::test]
    async fn project_rotation_prevents_hot_project_reacquisition() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let next_a = enqueue(&coordinator, "/a", &control);
        wait_queued(&coordinator, 1).await;
        let next_b = enqueue(&coordinator, "/b", &control);
        wait_queued(&coordinator, 2).await;
        drop(active);
        let active_b = tokio::time::timeout(Duration::from_secs(1), next_b)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!next_a.is_finished());
        drop(active_b);
        drop(next_a.await.unwrap().unwrap());
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn project_blocked_head_does_not_reserve_global_capacity() {
        let mut limits = coordinator().inner.limits;
        limits.analysis.running = 2;
        let coordinator = WorkCoordinator::new(limits).unwrap();
        let control = WorkControl::new(None);
        let active_a = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let next_a = enqueue(&coordinator, "/a", &control);
        wait_queued(&coordinator, 1).await;
        let active_b = tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.acquire_execution(Path::new("/b"), WorkClass::Analysis, &control),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(coordinator.snapshot().running, [0, 2, 0]);
        assert!(!next_a.is_finished());
        drop(active_a);
        drop(next_a.await.unwrap().unwrap());
        drop(active_b);
    }

    #[tokio::test]
    async fn queued_cancellation_removes_work_without_starting_it() {
        let coordinator = coordinator();
        let active_control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &active_control)
            .await
            .unwrap();
        let cancelled = WorkControl::new(None);
        let waiting = enqueue(&coordinator, "/b", &cancelled);
        wait_queued(&coordinator, 1).await;
        cancelled.cancel(StopReason::ClientCancelled);
        assert!(matches!(
            waiting.await.unwrap(),
            Err(AdmissionError::Stopped(StopReason::ClientCancelled))
        ));
        assert_eq!(coordinator.snapshot().queued, [0; 3]);
        assert_eq!(coordinator.snapshot().running, [0, 1, 0]);
        drop(active);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_and_admission_have_exactly_one_phase_winner() {
        for cancel_after_admission in [false, true] {
            let coordinator = coordinator();
            let control = WorkControl::new(None);
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            *coordinator.inner.admission_hook.lock() = Some(Arc::new(move |admitted| {
                if admitted == cancel_after_admission {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .recv_timeout(Duration::from_secs(5))
                        .expect("test must release admission boundary");
                }
            }));
            let acquiring = enqueue(&coordinator, "/a", &control);
            tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(2)))
                .await
                .unwrap()
                .expect("acquisition must reach selected phase boundary");
            control.cancel(StopReason::ClientCancelled);
            release_tx.send(()).unwrap();
            let result = acquiring.await.unwrap();
            if cancel_after_admission {
                let execution = result.unwrap();
                assert_eq!(coordinator.snapshot().running, [0, 1, 0]);
                assert!(matches!(
                    check_control(&control),
                    Err(AdmissionError::Stopped(StopReason::ClientCancelled))
                ));
                drop(execution);
            } else {
                assert!(matches!(
                    result,
                    Err(AdmissionError::Stopped(StopReason::ClientCancelled))
                ));
                assert_eq!(coordinator.snapshot().running, [0; 3]);
            }
            assert_eq!(coordinator.snapshot().queued, [0; 3]);
            assert_eq!(coordinator.snapshot().projects, 0);
        }
    }

    #[tokio::test]
    async fn dropped_acquisition_reclaims_pending_capacity() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let waiting = enqueue(&coordinator, "/b", &control);
        wait_queued(&coordinator, 1).await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert_eq!(coordinator.snapshot().queued, [0; 3]);
        drop(active);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn queue_limits_reject_excess_before_allocating_project_state() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let first = enqueue(&coordinator, "/a", &control);
        let second = enqueue(&coordinator, "/a", &control);
        wait_queued(&coordinator, 2).await;
        assert!(matches!(
            coordinator
                .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
                .await,
            Err(AdmissionError::Saturated)
        ));
        let third = enqueue(&coordinator, "/b", &control);
        let fourth = enqueue(&coordinator, "/b", &control);
        wait_queued(&coordinator, 4).await;
        assert!(matches!(
            coordinator
                .acquire_execution(Path::new("/c"), WorkClass::Analysis, &control)
                .await,
            Err(AdmissionError::Saturated)
        ));
        assert_eq!(coordinator.snapshot().projects, 2);
        for task in [first, second, third, fourth] {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        drop(active);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn deadline_includes_queue_wait() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let deadline = WorkControl::new(Some(Instant::now() + Duration::from_millis(20)));
        assert!(matches!(
            coordinator
                .acquire_execution(Path::new("/b"), WorkClass::Analysis, &deadline)
                .await,
            Err(AdmissionError::Stopped(StopReason::DeadlineExceeded))
        ));
        assert_eq!(coordinator.snapshot().queued, [0; 3]);
        drop(active);
    }

    #[tokio::test]
    async fn nested_worker_retains_native_capacity_after_cancellation() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Native, &control)
            .await
            .unwrap();
        let erased: Arc<dyn Send + Sync> = active.clone();
        control.set_work_lease(Arc::downgrade(&erased));
        drop(erased);
        let nested_worker = control.worker_lease().unwrap();
        control.cancel(StopReason::ClientCancelled);
        drop((active, control));
        assert_eq!(coordinator.snapshot().running, [0, 0, 1]);
        drop(nested_worker);
        assert_eq!(coordinator.snapshot().running, [0; 3]);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn panicking_blocking_worker_releases_its_execution_lease() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let execution = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &control)
            .await
            .unwrap();
        let result = tokio::task::spawn_blocking(move || {
            let _execution = execution;
            panic!("controlled worker panic");
        })
        .await;
        assert!(result.unwrap_err().is_panic());
        assert_eq!(coordinator.snapshot().running, [0; 3]);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[tokio::test]
    async fn shutdown_signals_work_without_releasing_live_executions() {
        let coordinator = coordinator();
        let running_control = WorkControl::new(None);
        let active = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Native, &running_control)
            .await
            .unwrap();
        let analysis = coordinator
            .acquire_execution(Path::new("/a"), WorkClass::Analysis, &running_control)
            .await
            .unwrap();
        let waiting_control = WorkControl::new(None);
        let waiting = enqueue(&coordinator, "/b", &waiting_control);
        wait_queued(&coordinator, 1).await;
        coordinator.shutdown();
        assert_eq!(running_control.reason(), Some(StopReason::Shutdown));
        assert!(waiting.await.unwrap().is_err());
        assert_eq!(waiting_control.reason(), Some(StopReason::Shutdown));
        assert!(matches!(
            coordinator.try_request(&WorkControl::new(None)),
            Err(AdmissionError::Shutdown)
        ));
        assert_eq!(coordinator.snapshot().running, [0, 1, 1]);
        drop((active, analysis));
        assert_eq!(coordinator.snapshot().running, [0; 3]);
        assert_eq!(coordinator.snapshot().projects, 0);
    }

    #[test]
    fn shutdown_cancels_logical_request_without_physical_execution() {
        let coordinator = coordinator();
        let control = WorkControl::new(None);
        let request = coordinator.try_request(&control).unwrap();
        coordinator.shutdown();
        assert_eq!(control.reason(), Some(StopReason::Shutdown));
        assert_eq!(coordinator.snapshot().requests, 1);
        assert_eq!(coordinator.snapshot().running, [0; 3]);
        drop(request);
        assert_eq!(coordinator.snapshot().requests, 0);
    }
}
