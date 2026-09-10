use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use codesage_graph::{CompleteRiskRanking, top_risk_ranking_with_policy};
use codesage_protocol::work::{StopReason, WorkControl};
use codesage_storage::Database;
use codesage_storage::db::connection_path_still_matches_open_file;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::Notify;

use super::diagnostics::{Diagnostics, ExecutionTicket, RequestTicket};
use super::work::{AdmissionError, ExecutionLease, WorkClass, WorkCoordinator};

const MAX_PROJECTS: usize = 32;
const MAX_PAYLOAD: usize = 2 * 1024 * 1024;
const MAX_CONFIG: u64 = 1024 * 1024;
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct CacheUnavailable(anyhow::Error);

impl std::fmt::Display for CacheUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "risk cache unavailable: {}", self.0)
    }
}

impl std::error::Error for CacheUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub(crate) fn is_cache_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<CacheUnavailable>())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Generation {
    identity: FileIdentity,
    epoch: u64,
    version: i64,
    config: Option<String>,
    recurrence: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn read(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        ensure!(metadata.is_file(), "index is not a regular file");
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

struct Observer {
    connection: Connection,
    identity: FileIdentity,
    epoch: u64,
}

#[derive(Default)]
struct ProjectSlot {
    observer: Mutex<Option<Observer>>,
    #[cfg(test)]
    recurrence_override: Mutex<Option<bool>>,
    #[cfg(test)]
    around_observer_open: Option<Arc<dyn Fn(bool) -> Result<()> + Send + Sync>>,
}

impl ProjectSlot {
    fn generation(
        &self,
        project: &Path,
        db_path: &Path,
        control: &WorkControl,
    ) -> Result<Generation> {
        control.check()?;
        let config = match config_digest(project) {
            Ok(config) => config,
            Err(error) => {
                control.check()?;
                return Err(CacheUnavailable(error).into());
            }
        };
        let identity = FileIdentity::read(db_path)?;
        let mut observer = self.observer.lock();
        let reopen = match observer.as_ref() {
            None => true,
            Some(observer) => {
                observer.identity != identity
                    || !connection_path_still_matches_open_file(&observer.connection, db_path)?
            }
        };
        if reopen {
            *observer = None;
            #[cfg(test)]
            if let Some(hook) = &self.around_observer_open {
                hook(false)?;
            }
            let connection = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            #[cfg(test)]
            if let Some(hook) = &self.around_observer_open {
                hook(true)?;
            }
            connection.busy_timeout(Duration::from_millis(100))?;
            require_attested(
                FileIdentity::read(db_path)? == identity
                    && connection_path_still_matches_open_file(&connection, db_path)?,
            )?;
            *observer = Some(Observer {
                connection,
                identity,
                epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            });
        }
        let observer = observer.as_ref().context("missing index observer")?;
        let version = observer
            .connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))?;
        require_attested(
            FileIdentity::read(db_path)? == identity
                && connection_path_still_matches_open_file(&observer.connection, db_path)?,
        )?;
        control.check()?;
        let recurrence = recurrence_policy(
            std::env::var("CODESAGE_COUPLING_RECURRENCE")
                .ok()
                .as_deref(),
        );
        #[cfg(test)]
        let recurrence = self.recurrence_override.lock().unwrap_or(recurrence);
        Ok(Generation {
            identity,
            epoch: observer.epoch,
            version,
            config,
            recurrence,
        })
    }
}

fn require_attested(attested: bool) -> Result<()> {
    if !attested {
        return Err(CacheUnavailable(anyhow::anyhow!(
            "opened index identity could not be attested"
        ))
        .into());
    }
    Ok(())
}

fn recurrence_policy(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false"))
}

fn config_digest(project: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(project.join(".codesage/config.toml"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    ensure!(
        file.metadata()?.is_file(),
        "project config is not a regular file"
    );
    file.take(MAX_CONFIG + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG,
        "project config exceeds cache probe limit"
    );
    Ok(Some(codesage_parser::discover::content_hash(&bytes)))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheDisposition {
    Hit,
    Owner,
    Shared,
}

#[derive(Clone)]
pub(crate) struct CachedRanking {
    pub ranking: Arc<CompleteRiskRanking>,
    pub generation: Generation,
    pub stable: bool,
    #[cfg(test)]
    pub disposition: CacheDisposition,
}

pub(crate) struct CacheRequest<'a> {
    pub coordinator: &'a WorkCoordinator,
    pub diagnostics: &'a Diagnostics,
    pub request: &'a RequestTicket,
    pub producer_deadline: Instant,
}

struct Ready {
    ranking: Arc<CompleteRiskRanking>,
    generation: Generation,
    bytes: usize,
}

struct Flight {
    id: u64,
    generation: Generation,
    control: WorkControl,
    notify: Notify,
    state: Mutex<FlightState>,
    execution: Arc<ExecutionTicket>,
}

struct FlightState {
    waiters: usize,
    abandoned: bool,
    result: Option<std::result::Result<CachedRanking, Arc<anyhow::Error>>>,
    retired: bool,
}

struct ProjectEntry {
    slot: Arc<ProjectSlot>,
    ready: Option<Ready>,
    flight: Option<Arc<Flight>>,
    touched: u64,
}

#[derive(Default)]
struct CacheState {
    projects: HashMap<PathBuf, ProjectEntry>,
    sequence: u64,
    bytes: usize,
    retired: usize,
}

#[derive(Clone, Default)]
pub(crate) struct OverviewCache {
    state: Arc<Mutex<CacheState>>,
    #[cfg(test)]
    before_compute: Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
    #[cfg(test)]
    before_database_open: Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
    #[cfg(test)]
    before_generation: Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
}

impl OverviewCache {
    fn slot(&self, project: &Path) -> Result<Arc<ProjectSlot>> {
        let mut state = self.state.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        let mut evicted = None;
        if let Some(entry) = state.projects.get_mut(project) {
            entry.touched = sequence;
            return Ok(entry.slot.clone());
        }
        if state.projects.len() >= MAX_PROJECTS {
            let victim = state
                .projects
                .iter()
                .filter(|(_, entry)| entry.flight.is_none() && Arc::strong_count(&entry.slot) == 1)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(path, _)| path.clone());
            let Some(victim) = victim else {
                return Err(AdmissionError::Saturated.into());
            };
            evicted = state.projects.remove(&victim);
            if let Some(entry) = &evicted {
                state.bytes -= entry.ready.as_ref().map_or(0, |ready| ready.bytes);
            }
        }
        let slot = Arc::new(ProjectSlot::default());
        state.projects.insert(
            project.to_owned(),
            ProjectEntry {
                slot: slot.clone(),
                ready: None,
                flight: None,
                touched: sequence,
            },
        );
        drop(state);
        drop(evicted);
        Ok(slot)
    }

    pub(crate) async fn generation(
        &self,
        project: &Path,
        db_path: &Path,
        control: &WorkControl,
        context: &CacheRequest<'_>,
    ) -> Result<Generation> {
        let ticket = Arc::new(
            context
                .diagnostics
                .begin_execution("overview_generation", project.to_str()),
        );
        ticket.set_class("interactive");
        context.request.link_execution_ticket(&ticket);
        let mut consumer = ProbeConsumer {
            ticket: ticket.clone(),
            control: control.clone(),
            transferred: false,
            completed: false,
            outcome: "abandoned",
        };
        let lease = match context
            .coordinator
            .acquire_execution(project, WorkClass::Interactive, control)
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                consumer.outcome = if error == AdmissionError::Saturated {
                    "saturated"
                } else if error == AdmissionError::Shutdown {
                    "shutdown"
                } else {
                    "error"
                };
                consumer.completed = true;
                return Err(error.into());
            }
        };
        let slot = match self.slot(project) {
            Ok(slot) => slot,
            Err(error) => {
                consumer.outcome = "error";
                consumer.completed = true;
                return Err(error);
            }
        };
        let project = project.to_owned();
        let db_path = db_path.to_owned();
        let control = control.clone();
        let weak_ticket = Arc::downgrade(&ticket);
        let registration = control.on_cancel(Arc::new(move || {
            if let Some(ticket) = weak_ticket.upgrade() {
                ticket.set_consumers(0);
            }
        }));
        let resources = Arc::new(ProbeResources {
            _lease: lease,
            ticket,
            control: control.clone(),
            outcome: Mutex::new("error"),
            _registration: registration,
        });
        let lease: Arc<dyn Send + Sync> = resources.clone();
        control.set_work_lease(Arc::downgrade(&lease));
        drop(lease);
        #[cfg(test)]
        let hook = self.before_generation.clone();
        consumer.transferred = true;
        let result = tokio::task::spawn_blocking(move || {
            resources.ticket.set_phase("running");
            let _scope = control.enter();
            let result = (|| {
                control.check()?;
                #[cfg(test)]
                if let Some(hook) = hook {
                    hook()?;
                }
                slot.generation(&project, &db_path, &control)
            })();
            *resources.outcome.lock() = match &result {
                Ok(_) => "success",
                Err(error)
                    if error
                        .downcast_ref::<rusqlite::Error>()
                        .is_some_and(|error| {
                            matches!(
                                error.sqlite_error_code(),
                                Some(
                                    rusqlite::ErrorCode::DatabaseBusy
                                        | rusqlite::ErrorCode::DatabaseLocked
                                )
                            )
                        }) =>
                {
                    "database_busy"
                }
                Err(_) => "error",
            };
            result
        })
        .await
        .context("generation probe worker panicked");
        consumer.completed = true;
        result?
    }

    pub(crate) fn generation_in_execution(
        &self,
        project: &Path,
        db_path: &Path,
        control: &WorkControl,
    ) -> Result<Generation> {
        self.slot(project)?.generation(project, db_path, control)
    }

    pub(crate) async fn get(
        &self,
        project: &Path,
        db_path: &Path,
        waiter: &WorkControl,
        context: CacheRequest<'_>,
    ) -> Result<CachedRanking> {
        let CacheRequest {
            coordinator,
            diagnostics,
            request,
            producer_deadline,
        } = context;
        let slot = self.slot(project)?;
        let generation = self.generation(project, db_path, waiter, &context).await?;
        waiter.check()?;
        let (flight, owner) = {
            let mut state = self.state.lock();
            state.sequence += 1;
            let id = state.sequence;
            let entry = state
                .projects
                .get_mut(project)
                .context("risk cache project disappeared")?;
            if let Some(ready) = &entry.ready
                && ready.generation == generation
            {
                request.set_reuse("hit");
                return Ok(CachedRanking {
                    ranking: ready.ranking.clone(),
                    generation,
                    stable: true,
                    #[cfg(test)]
                    disposition: CacheDisposition::Hit,
                });
            }
            let existing = entry
                .flight
                .as_ref()
                .filter(|flight| flight.generation == generation && !flight.state.lock().abandoned)
                .cloned();
            if let Some(flight) = existing {
                flight.state.lock().waiters += 1;
                (flight, false)
            } else {
                let execution =
                    Arc::new(diagnostics.begin_execution("overview_ranking", project.to_str()));
                execution.set_class("analysis");
                let flight = Arc::new(Flight {
                    id,
                    generation: generation.clone(),
                    control: WorkControl::new(Some(producer_deadline)),
                    notify: Notify::new(),
                    state: Mutex::new(FlightState {
                        waiters: 1,
                        abandoned: false,
                        result: None,
                        retired: false,
                    }),
                    execution,
                });
                let previous = entry.flight.replace(flight.clone());
                if let Some(previous) = previous {
                    previous.state.lock().retired = true;
                    state.retired += 1;
                }
                (flight, true)
            }
        };
        request.link_execution_ticket(&flight.execution);
        request.set_reuse(if owner { "miss" } else { "shared" });
        request.set_phase("shared_wait");
        flight.execution.set_consumers(flight.state.lock().waiters);
        let _guard = WaiterGuard {
            cache: self.clone(),
            project: project.to_owned(),
            flight: flight.clone(),
            diagnostics: diagnostics.clone(),
        };
        self.update_gauges(diagnostics);
        if owner {
            self.spawn(
                project.to_owned(),
                db_path.to_owned(),
                slot,
                flight.clone(),
                coordinator.clone(),
                diagnostics.clone(),
            );
        }
        let cancelled = Arc::new(Notify::new());
        let notify = cancelled.clone();
        let _registration = waiter.on_cancel(Arc::new(move || notify.notify_one()));
        loop {
            let notification = flight.notify.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            waiter.check()?;
            if let Some(result) = &flight.state.lock().result {
                return match result {
                    Ok(result) => Ok(CachedRanking {
                        #[cfg(test)]
                        disposition: if owner {
                            CacheDisposition::Owner
                        } else {
                            CacheDisposition::Shared
                        },
                        ..result.clone()
                    }),
                    Err(error) => Err(SharedFailure(error.clone()).into()),
                };
            }
            let deadline = waiter
                .deadline()
                .map(tokio::time::Instant::from_std)
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86400));
            tokio::select! {
                _ = notification => {},
                _ = cancelled.notified() => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        }
    }

    fn spawn(
        &self,
        project: PathBuf,
        db_path: PathBuf,
        slot: Arc<ProjectSlot>,
        flight: Arc<Flight>,
        coordinator: WorkCoordinator,
        diagnostics: Diagnostics,
    ) {
        let cache = self.clone();
        tokio::spawn(async move {
            let result = async {
                let lease = coordinator
                    .acquire_execution(&project, WorkClass::Analysis, &flight.control)
                    .await?;
                let project = project.clone();
                let flight = flight.clone();
                #[cfg(test)]
                let hook = cache.before_compute.clone();
                #[cfg(test)]
                let before_open = cache.before_database_open.clone();
                tokio::task::spawn_blocking(move || {
                    let lease: Arc<dyn Send + Sync> = lease;
                    flight.control.set_work_lease(Arc::downgrade(&lease));
                    let _lease = lease;
                    let _scope = flight.control.enter();
                    flight.execution.set_phase("running");
                    flight.control.check()?;
                    let before = slot.generation(&project, &db_path, &flight.control)?;
                    #[cfg(test)]
                    if let Some(hook) = before_open {
                        hook()?;
                    }
                    let db = Database::open_read_only_strict(&db_path)?;
                    let snapshot = db.read_snapshot()?;
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook()?;
                    }
                    let ranking = Arc::new(top_risk_ranking_with_policy(&db, before.recurrence)?);
                    let after = match slot.generation(&project, &db_path, &flight.control) {
                        Ok(generation) => Some(generation),
                        Err(error) if is_cache_unavailable(&error) => None,
                        Err(error) => return Err(error),
                    };
                    let attested = db.path_still_matches_open_file(&db_path)?;
                    drop(snapshot);
                    flight.control.check()?;
                    Ok::<_, anyhow::Error>(CachedRanking {
                        ranking,
                        generation: before.clone(),
                        stable: attested
                            && after.as_ref().is_some_and(|after| *after == before)
                            && before == flight.generation,
                        #[cfg(test)]
                        disposition: CacheDisposition::Owner,
                    })
                })
                .await
                .context("risk cache producer panicked")?
            }
            .await;
            let outcome = match &result {
                Ok(_) => "success",
                Err(error) => flight.control.reason().map_or_else(
                    || {
                        if error.downcast_ref::<AdmissionError>()
                            == Some(&AdmissionError::Saturated)
                        {
                            "saturated"
                        } else if error.downcast_ref::<AdmissionError>()
                            == Some(&AdmissionError::Shutdown)
                        {
                            "shutdown"
                        } else if error.is::<codesage_graph::IncompleteRiskRanking>() {
                            "incomplete"
                        } else if error
                            .downcast_ref::<rusqlite::Error>()
                            .is_some_and(|error| {
                                matches!(
                                    error.sqlite_error_code(),
                                    Some(
                                        rusqlite::ErrorCode::DatabaseBusy
                                            | rusqlite::ErrorCode::DatabaseLocked
                                    )
                                )
                            })
                        {
                            "database_busy"
                        } else if error
                            .downcast_ref::<tokio::task::JoinError>()
                            .is_some_and(|error| error.is_panic())
                        {
                            "panic"
                        } else {
                            "error"
                        }
                    },
                    StopReason::as_str,
                ),
            };
            flight.execution.finish(outcome);
            cache.complete(&project, &flight, result);
            cache.update_gauges(&diagnostics);
        });
    }

    fn update_gauges(&self, diagnostics: &Diagnostics) {
        let state = self.state.lock();
        diagnostics.set_gauge(
            "cache_entries",
            state
                .projects
                .values()
                .filter(|entry| entry.ready.is_some())
                .count() as u64,
        );
        diagnostics.set_gauge("cache_bytes", state.bytes as u64);
        diagnostics.set_gauge("cache_observers", state.projects.len() as u64);
        diagnostics.set_gauge(
            "single_flights",
            state
                .projects
                .values()
                .filter(|entry| entry.flight.is_some())
                .count() as u64,
        );
        diagnostics.set_gauge("retired_flights", state.retired as u64);
    }

    fn complete(&self, project: &Path, flight: &Flight, result: Result<CachedRanking>) {
        let mut state = self.state.lock();
        let mut status = flight.state.lock();
        if status.retired {
            state.retired -= 1;
            status.retired = false;
        }
        if !status.abandoned
            && flight.control.reason().is_none()
            && let Ok(result) = &result
        {
            let bytes = ranking_bytes(&result.ranking);
            let current = state
                .projects
                .get(project)
                .and_then(|entry| entry.flight.as_ref())
                .is_some_and(|current| current.id == flight.id);
            if result.stable && current && bytes <= MAX_PAYLOAD {
                let old = state
                    .projects
                    .get_mut(project)
                    .and_then(|entry| entry.ready.take())
                    .map_or(0, |ready| ready.bytes);
                state.bytes -= old;
                while state.bytes + bytes > MAX_PAYLOAD {
                    let victim = state
                        .projects
                        .iter()
                        .filter(|(_, entry)| entry.ready.is_some())
                        .min_by_key(|(_, entry)| entry.touched)
                        .map(|(path, _)| path.clone());
                    let Some(victim) = victim else {
                        break;
                    };
                    if let Some(ready) = state
                        .projects
                        .get_mut(&victim)
                        .and_then(|entry| entry.ready.take())
                    {
                        state.bytes -= ready.bytes;
                    }
                }
                if let Some(entry) = state.projects.get_mut(project) {
                    entry.ready = Some(Ready {
                        ranking: result.ranking.clone(),
                        generation: result.generation.clone(),
                        bytes,
                    });
                    state.bytes += bytes;
                }
            }
        }
        status.result = Some(result.map_err(Arc::new));
        if let Some(entry) = state.projects.get_mut(project)
            && entry
                .flight
                .as_ref()
                .is_some_and(|current| current.id == flight.id)
        {
            entry.flight = None;
        }
        drop(status);
        drop(state);
        flight.notify.notify_waiters();
    }
}

struct ProbeResources {
    _lease: Arc<ExecutionLease>,
    ticket: Arc<ExecutionTicket>,
    control: WorkControl,
    outcome: Mutex<&'static str>,
    _registration: codesage_protocol::work::CancelRegistration,
}

impl Drop for ProbeResources {
    fn drop(&mut self) {
        let outcome = self
            .control
            .reason()
            .map(StopReason::as_str)
            .unwrap_or_else(|| {
                if std::thread::panicking() {
                    "panic"
                } else {
                    *self.outcome.lock()
                }
            });
        self.ticket.finish(outcome);
    }
}

struct ProbeConsumer {
    ticket: Arc<ExecutionTicket>,
    control: WorkControl,
    transferred: bool,
    completed: bool,
    outcome: &'static str,
}

impl Drop for ProbeConsumer {
    fn drop(&mut self) {
        if !self.completed {
            self.control.cancel(StopReason::NoConsumers);
            self.ticket.set_consumers(0);
        }
        if !self.transferred {
            self.ticket.finish(
                self.control
                    .reason()
                    .map_or(self.outcome, StopReason::as_str),
            );
        }
    }
}

fn ranking_bytes(ranking: &CompleteRiskRanking) -> usize {
    ranking.allocated_bytes()
}

struct WaiterGuard {
    cache: OverviewCache,
    project: PathBuf,
    flight: Arc<Flight>,
    diagnostics: Diagnostics,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut cache = self.cache.state.lock();
        let mut state = self.flight.state.lock();
        state.waiters -= 1;
        self.flight.execution.set_consumers(state.waiters);
        let abandon = state.waiters == 0 && state.result.is_none();
        if abandon {
            state.abandoned = true;
            if !state.retired {
                state.retired = true;
                cache.retired += 1;
            }
            if let Some(entry) = cache.projects.get_mut(&self.project)
                && entry
                    .flight
                    .as_ref()
                    .is_some_and(|current| current.id == self.flight.id)
            {
                entry.flight = None;
            }
        }
        drop(state);
        drop(cache);
        if abandon {
            self.flight.control.cancel(StopReason::NoConsumers);
        }
        self.cache.update_gauges(&self.diagnostics);
    }
}

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);

impl std::fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::super::work::WorkLimits;
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Fixture {
        root: tempfile::TempDir,
        path: PathBuf,
        coordinator: WorkCoordinator,
        diagnostics: Diagnostics,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join(".codesage")).unwrap();
            let path = root.path().join(".codesage/index.db");
            drop(Database::open(&path).unwrap());
            let mut limits = WorkLimits::default();
            limits.interactive.queued = 64;
            limits.interactive.queued_per_project = 64;
            Self {
                root,
                path,
                coordinator: WorkCoordinator::new(limits).unwrap(),
                diagnostics: Diagnostics::default(),
            }
        }

        fn insert(&self, name: &str) {
            Connection::open(&self.path)
                .unwrap()
                .execute(
                    "INSERT INTO files(path,language,content_hash) VALUES(?1,'rust','x')",
                    [name],
                )
                .unwrap();
        }

        fn recurrence_sensitive() -> Self {
            use codesage_storage::db::CoChangeWrite;

            let fixture = Self::new();
            fixture.insert("target.rs");
            Connection::open(&fixture.path)
                .unwrap()
                .execute(
                    "INSERT INTO symbols(file_id,name,qualified_name,kind,line_start,line_end,col_start,col_end)
                     SELECT id,'target','target','function',1,1,0,20 FROM files WHERE path='target.rs'",
                    [],
                )
                .unwrap();
            let db = Database::open(&fixture.path).unwrap();
            db.upsert_git_file("target.rs", 1.0, 0, 40, None).unwrap();
            let pair = |weight, count, days: i64| CoChangeWrite {
                weight,
                count,
                window_mask: 1,
                first_observed_at: Some(1_750_000_000 - days * 86_400),
                last_observed_at: Some(1_750_000_000),
            };
            for i in 0..10 {
                db.upsert_git_co_change_full(
                    "target.rs",
                    &format!("src/m{i}.rs"),
                    &pair(10.0, 4, 2),
                )
                .unwrap();
            }
            db.upsert_git_co_change_full("target.rs", "tests/promoted_test.rs", &pair(9.0, 5, 200))
                .unwrap();
            assert!(
                db.co_changes_for("target.rs", 10)
                    .unwrap()
                    .iter()
                    .all(|row| row.file != "tests/promoted_test.rs")
            );
            assert_eq!(
                db.co_changes_for_ranked("target.rs", 10, 0.5).unwrap()[0].file,
                "tests/promoted_test.rs"
            );
            fixture
        }

        fn recurrence_scores(&self) -> [f64; 2] {
            let db = Database::open_read_only_strict(&self.path).unwrap();
            let scores = [false, true].map(|recurrence| {
                target_score(&top_risk_ranking_with_policy(&db, recurrence).unwrap())
            });
            assert!((scores[0] - 0.54).abs() < 1e-12);
            assert!((scores[1] - 0.41).abs() < 1e-12);
            scores
        }

        async fn get(&self, cache: &OverviewCache, control: &WorkControl) -> Result<CachedRanking> {
            let mut admission = self.coordinator.try_request(control)?;
            admission.attach_project(self.root.path())?;
            let request = self
                .diagnostics
                .begin_request("project_overview", self.root.path().to_str());
            cache
                .get(
                    self.root.path(),
                    &self.path,
                    control,
                    CacheRequest {
                        coordinator: &self.coordinator,
                        diagnostics: &self.diagnostics,
                        request: &request,
                        producer_deadline: Instant::now() + Duration::from_secs(10),
                    },
                )
                .await
        }
    }

    fn target_score(ranking: &CompleteRiskRanking) -> f64 {
        assert_eq!(ranking.rows().len(), 1);
        assert_eq!(ranking.rows()[0].file, "target.rs");
        ranking.rows()[0].score
    }

    fn hold_first() -> (
        OverviewCache,
        Arc<Notify>,
        std::sync::mpsc::Sender<()>,
        Arc<AtomicUsize>,
    ) {
        let started = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, receiver) = std::sync::mpsc::channel();
        let receiver = Mutex::new(receiver);
        let notify = started.clone();
        let count = calls.clone();
        let cache = OverviewCache {
            before_compute: Some(Arc::new(move || {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    notify.notify_one();
                    receiver
                        .lock()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                Ok(())
            })),
            ..OverviewCache::default()
        };
        (cache, started, release, calls)
    }

    async fn wait_for_waiters(cache: &OverviewCache, root: &Path, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let actual = cache
                    .state
                    .lock()
                    .projects
                    .get(root)
                    .and_then(|entry| entry.flight.as_ref())
                    .map(|flight| flight.state.lock().waiters);
                if actual == Some(count) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sixteen_overlapping_requests_compute_once_then_hit() {
        let fixture = Arc::new(Fixture::new());
        fixture.insert("src/a.rs");
        let (cache, started, release, calls) = hold_first();
        let mut requests = Vec::new();
        for _ in 0..16 {
            let fixture = fixture.clone();
            let cache = cache.clone();
            requests.push(tokio::spawn(async move {
                fixture.get(&cache, &WorkControl::new(None)).await.unwrap()
            }));
        }
        started.notified().await;
        wait_for_waiters(&cache, fixture.root.path(), 16).await;
        release.send(()).unwrap();
        let mut owners = 0;
        let mut shared = 0;
        for request in requests {
            let result = request.await.unwrap();
            assert_eq!(result.ranking.rows()[0].file, "src/a.rs");
            match result.disposition {
                CacheDisposition::Owner => owners += 1,
                CacheDisposition::Shared => shared += 1,
                CacheDisposition::Hit => panic!("producer barrier must exclude hits"),
            }
        }
        assert_eq!((owners, shared, calls.load(Ordering::SeqCst)), (1, 15, 1));
        let warm = fixture.get(&cache, &WorkControl::new(None)).await.unwrap();
        assert_eq!(warm.disposition, CacheDisposition::Hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn database_commits_and_config_edits_invalidate() {
        let fixture = Fixture::new();
        fixture.insert("src/a.rs");
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let original = fixture.get(&cache, &control).await.unwrap();
        fixture.insert("src/b.rs");
        let changed = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(changed.disposition, CacheDisposition::Owner);
        assert_ne!(changed.generation, original.generation);
        assert_eq!(changed.ranking.rows().len(), 2);
        std::fs::write(
            fixture.root.path().join(".codesage/config.toml"),
            "[project]\nname='changed'\n",
        )
        .unwrap();
        let configured = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(configured.disposition, CacheDisposition::Owner);
        assert_ne!(configured.generation, changed.generation);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_owner_preserves_surviving_waiter() {
        let fixture = Arc::new(Fixture::new());
        fixture.insert("src/a.rs");
        let (cache, started, release, calls) = hold_first();
        let owner_control = WorkControl::new(None);
        let owner = {
            let fixture = fixture.clone();
            let cache = cache.clone();
            let control = owner_control.clone();
            tokio::spawn(async move { fixture.get(&cache, &control).await })
        };
        started.notified().await;
        let survivor = {
            let fixture = fixture.clone();
            let cache = cache.clone();
            tokio::spawn(async move { fixture.get(&cache, &WorkControl::new(None)).await })
        };
        wait_for_waiters(&cache, fixture.root.path(), 2).await;
        owner_control.cancel(StopReason::ClientCancelled);
        assert!(owner.await.unwrap().is_err());
        release.send(()).unwrap();
        assert_eq!(
            survivor.await.unwrap().unwrap().ranking.rows()[0].file,
            "src/a.rs"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fifteen_cancellations_leave_one_shared_consumer_running() {
        let fixture = Arc::new(Fixture::new());
        fixture.insert("src/a.rs");
        let (cache, started, release, calls) = hold_first();
        let mut requests = Vec::new();
        let controls: Vec<_> = (0..16).map(|_| WorkControl::new(None)).collect();
        for control in &controls {
            let control = control.clone();
            let fixture = fixture.clone();
            let cache = cache.clone();
            requests.push(tokio::spawn(
                async move { fixture.get(&cache, &control).await },
            ));
        }
        started.notified().await;
        wait_for_waiters(&cache, fixture.root.path(), 16).await;
        for control in controls.iter().take(15) {
            control.cancel(StopReason::ClientCancelled);
        }
        for request in requests.drain(..15) {
            assert!(request.await.unwrap().is_err());
        }
        wait_for_waiters(&cache, fixture.root.path(), 1).await;
        assert_eq!(fixture.coordinator.snapshot().running[1], 1);
        release.send(()).unwrap();
        assert_eq!(
            requests
                .pop()
                .unwrap()
                .await
                .unwrap()
                .unwrap()
                .ranking
                .rows()[0]
                .file,
            "src/a.rs"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_producer_cannot_poison_replacement() {
        let fixture = Arc::new(Fixture::new());
        fixture.insert("src/a.rs");
        let (cache, started, release, calls) = hold_first();
        let control = WorkControl::new(None);
        let owner = {
            let fixture = fixture.clone();
            let cache = cache.clone();
            let control = control.clone();
            tokio::spawn(async move { fixture.get(&cache, &control).await })
        };
        started.notified().await;
        control.cancel(StopReason::ClientCancelled);
        assert!(owner.await.unwrap().is_err());
        assert_eq!(fixture.coordinator.snapshot().running[1], 1);
        assert_eq!(cache.state.lock().retired, 1);
        let replacement = {
            let fixture = fixture.clone();
            let cache = cache.clone();
            tokio::spawn(async move { fixture.get(&cache, &WorkControl::new(None)).await })
        };
        wait_for_waiters(&cache, fixture.root.path(), 1).await;
        release.send(()).unwrap();
        let result = replacement.await.unwrap().unwrap();
        assert_eq!(result.disposition, CacheDisposition::Owner);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.state.lock().retired, 0);
        assert_eq!(
            fixture
                .get(&cache, &WorkControl::new(None))
                .await
                .unwrap()
                .disposition,
            CacheDisposition::Hit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn indexing_during_snapshot_returns_complete_uncached_result() {
        let fixture = Arc::new(Fixture::new());
        fixture.insert("src/a.rs");
        let (cache, started, release, _) = hold_first();
        let request = {
            let fixture = fixture.clone();
            let cache = cache.clone();
            tokio::spawn(async move { fixture.get(&cache, &WorkControl::new(None)).await.unwrap() })
        };
        started.notified().await;
        fixture.insert("src/b.rs");
        release.send(()).unwrap();
        let before = request.await.unwrap();
        assert!(!before.stable);
        assert_eq!(before.ranking.rows().len(), 1);
        let after = fixture.get(&cache, &WorkControl::new(None)).await.unwrap();
        assert_eq!(after.disposition, CacheDisposition::Owner);
        assert_eq!(after.ranking.rows().len(), 2);
    }

    #[tokio::test]
    async fn errors_and_panics_are_retryable() {
        for panic in [false, true] {
            let fixture = Fixture::new();
            fixture.insert("src/a.rs");
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let cache = OverviewCache {
                before_compute: Some(Arc::new(move || {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        assert!(!panic, "injected producer panic");
                        anyhow::bail!("injected producer failure");
                    }
                    Ok(())
                })),
                ..OverviewCache::default()
            };
            let error = fixture
                .get(&cache, &WorkControl::new(None))
                .await
                .err()
                .unwrap();
            assert!(!is_cache_unavailable(&error));
            let retry = fixture.get(&cache, &WorkControl::new(None)).await.unwrap();
            assert_eq!(retry.disposition, CacheDisposition::Owner);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn oversized_success_is_returned_without_retention() {
        let fixture = Fixture::new();
        fixture.insert(&format!("{}.rs", "a".repeat(MAX_PAYLOAD)));
        let cache = OverviewCache::default();
        for _ in 0..2 {
            let result = fixture.get(&cache, &WorkControl::new(None)).await.unwrap();
            assert_eq!(result.disposition, CacheDisposition::Owner);
            assert_eq!(result.ranking.rows().len(), 1);
            assert_eq!(cache.state.lock().bytes, 0);
        }
    }

    #[tokio::test]
    async fn retained_payload_accounts_for_truncated_vector_allocation() {
        let fixture = Fixture::new();
        let writer = Connection::open(&fixture.path).unwrap();
        for index in 0..400 {
            writer
                .execute(
                    "INSERT INTO files(path,language,content_hash) VALUES(?1,'rust','x')",
                    [format!("src/{index:03}.rs")],
                )
                .unwrap();
        }
        let cache = OverviewCache::default();
        let result = fixture.get(&cache, &WorkControl::new(None)).await.unwrap();
        assert_eq!(result.ranking.rows().len(), 50);
        let strings: usize = result
            .ranking
            .rows()
            .iter()
            .map(|row| row.file.capacity())
            .sum();
        assert!(
            cache.state.lock().bytes
                >= std::mem::size_of::<CompleteRiskRanking>()
                    + 400 * std::mem::size_of::<codesage_protocol::SessionRiskEntry>()
                    + strings
        );
        assert_eq!(result.ranking.rows()[0].file, "src/000.rs");
        assert_eq!(result.ranking.rows()[49].file, "src/049.rs");
    }

    #[test]
    fn observer_eviction_never_reuses_epochs() {
        let fixture = Fixture::new();
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let first = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        for index in 0..MAX_PROJECTS {
            cache
                .slot(&fixture.root.path().join(index.to_string()))
                .unwrap();
        }
        assert_eq!(cache.state.lock().projects.len(), MAX_PROJECTS);
        let second = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        assert_ne!(first.epoch, second.epoch);
        assert_eq!(first.identity, second.identity);
    }

    #[test]
    fn pinned_projects_saturate_without_unbounded_observer_slots() {
        let cache = OverviewCache::default();
        let root = tempfile::tempdir().unwrap();
        let mut pinned = Vec::new();
        for index in 0..MAX_PROJECTS {
            pinned.push(cache.slot(&root.path().join(index.to_string())).unwrap());
        }
        let extra = root.path().join("extra");
        let error = cache.slot(&extra).err().unwrap();
        assert_eq!(
            error.downcast_ref::<AdmissionError>(),
            Some(&AdmissionError::Saturated)
        );
        assert_eq!(cache.state.lock().projects.len(), MAX_PROJECTS);
        pinned.pop();
        cache.slot(&extra).unwrap();
        assert_eq!(cache.state.lock().projects.len(), MAX_PROJECTS);
    }

    #[test]
    fn database_replacement_reopens_observer() {
        let fixture = Fixture::new();
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let first = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        let replacement = fixture.root.path().join("replacement.db");
        drop(Database::open(&replacement).unwrap());
        std::fs::rename(replacement, &fixture.path).unwrap();
        let second = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        assert_ne!(first.identity, second.identity);
        assert_ne!(first.epoch, second.epoch);
    }

    #[tokio::test]
    async fn history_and_feature_commits_invalidate_ranking() {
        let fixture = Fixture::new();
        fixture.insert("src/a.rs");
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let original = fixture.get(&cache, &control).await.unwrap();
        let writer = Connection::open(&fixture.path).unwrap();
        writer
            .execute(
                "INSERT INTO git_files(path,churn_score,total_commits) VALUES('src/a.rs',10,1)",
                [],
            )
            .unwrap();
        let history = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(history.disposition, CacheDisposition::Owner);
        assert_ne!(history.generation, original.generation);
        writer.execute("INSERT INTO features(feature_id,title,summary,kind,source,confidence,entry_path,language) VALUES('f','title','summary','library','cargo','high','src/a.rs','rust')", []).unwrap();
        let feature = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(feature.disposition, CacheDisposition::Owner);
        assert_ne!(feature.generation, history.generation);
    }

    #[test]
    fn fifo_config_is_rejected_without_waiting_for_writer() {
        let fixture = Fixture::new();
        let path = fixture.root.path().join(".codesage/config.toml");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        let error = config_digest(fixture.root.path()).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn distinct_projects_never_share_rankings() {
        let first = Fixture::new();
        let second = Fixture::new();
        first.insert("only-first.rs");
        second.insert("only-second.rs");
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let a = first.get(&cache, &control).await.unwrap();
        let b = second.get(&cache, &control).await.unwrap();
        assert_eq!(a.ranking.rows()[0].file, "only-first.rs");
        assert_eq!(b.ranking.rows()[0].file, "only-second.rs");
        assert_eq!(a.disposition, CacheDisposition::Owner);
        assert_eq!(b.disposition, CacheDisposition::Owner);
    }

    #[test]
    fn recurrence_policy_preserves_existing_exact_environment_semantics() {
        for (value, expected) in [
            (None, true),
            (Some("0"), false),
            (Some("false"), false),
            (Some("FALSE"), true),
            (Some(" false "), true),
            (Some(""), true),
        ] {
            assert_eq!(recurrence_policy(value), expected, "{value:?}");
        }
    }

    #[tokio::test]
    async fn recurrence_policy_change_invalidates_ready_ranking() {
        let fixture = Fixture::recurrence_sensitive();
        let scores = fixture.recurrence_scores();
        let cache = OverviewCache::default();
        let slot = cache.slot(fixture.root.path()).unwrap();
        let control = WorkControl::new(None);
        for recurrence in [true, false] {
            *slot.recurrence_override.lock() = Some(recurrence);
            for disposition in [CacheDisposition::Owner, CacheDisposition::Hit] {
                let result = fixture.get(&cache, &control).await.unwrap();
                assert_eq!(result.disposition, disposition);
                assert!(result.stable);
                assert_eq!(
                    target_score(&result.ranking),
                    scores[usize::from(recurrence)]
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn producer_keeps_captured_policy_without_caching_changed_generation() {
        let fixture = Arc::new(Fixture::recurrence_sensitive());
        let scores = fixture.recurrence_scores();
        let ambient = recurrence_policy(
            std::env::var("CODESAGE_COUPLING_RECURRENCE")
                .ok()
                .as_deref(),
        );
        let captured = !ambient;
        let (cache, started, release, calls) = hold_first();
        let slot = cache.slot(fixture.root.path()).unwrap();
        *slot.recurrence_override.lock() = Some(captured);
        let task_fixture = fixture.clone();
        let task_cache = cache.clone();
        let task =
            tokio::spawn(
                async move { task_fixture.get(&task_cache, &WorkControl::new(None)).await },
            );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        *slot.recurrence_override.lock() = Some(ambient);
        release.send(()).unwrap();
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.disposition, CacheDisposition::Owner);
        assert!(!result.stable);
        assert_eq!(result.generation.recurrence, captured);
        assert_eq!(target_score(&result.ranking), scores[usize::from(captured)]);

        let control = WorkControl::new(None);
        for recurrence in [captured, ambient] {
            *slot.recurrence_override.lock() = Some(recurrence);
            for disposition in [CacheDisposition::Owner, CacheDisposition::Hit] {
                let result = fixture.get(&cache, &control).await.unwrap();
                assert_eq!(result.disposition, disposition);
                assert!(result.stable);
                assert_eq!(
                    target_score(&result.ranking),
                    scores[usize::from(recurrence)]
                );
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn unsupported_config_disables_cache_without_reclassifying_database_errors() {
        let fixture = Fixture::new();
        let config = fixture.root.path().join(".codesage/config.toml");
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        std::fs::create_dir(&config).unwrap();
        let error = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap_err();
        assert!(is_cache_unavailable(&error));
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("not a regular file"))
        );
        std::fs::remove_dir(&config).unwrap();
        let missing = fixture.root.path().join("missing.db");
        let error = cache
            .generation_in_execution(fixture.root.path(), &missing, &control)
            .unwrap_err();
        assert!(!is_cache_unavailable(&error));
    }

    #[test]
    fn oversized_config_disables_cache_but_missing_config_remains_reusable() {
        let fixture = Fixture::new();
        let cache = OverviewCache::default();
        let control = WorkControl::new(None);
        let first = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        let second = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.config, None);
        std::fs::write(
            fixture.root.path().join(".codesage/config.toml"),
            vec![b' '; MAX_CONFIG as usize + 1],
        )
        .unwrap();
        let error = cache
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap_err();
        assert!(is_cache_unavailable(&error));
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("exceeds cache probe limit"))
        );
    }

    #[test]
    fn cancellation_is_never_downgraded_to_cache_unavailability() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.root.path().join(".codesage/config.toml")).unwrap();
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let error = OverviewCache::default()
            .generation_in_execution(fixture.root.path(), &fixture.path, &control)
            .unwrap_err();
        assert!(!is_cache_unavailable(&error));
        assert_eq!(
            error
                .downcast_ref::<codesage_protocol::work::WorkStopped>()
                .unwrap()
                .reason,
            StopReason::ClientCancelled
        );
    }

    #[test]
    fn observer_rejects_opening_a_different_database_during_path_aba() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        fixture.insert("only-a.rs");
        other.insert("only-b.rs");
        for path in [&fixture.path, &other.path] {
            Connection::open(path)
                .unwrap()
                .pragma_update(None, "journal_mode", "DELETE")
                .unwrap();
        }
        let path = fixture.path.clone();
        let replacement = other.path.clone();
        let saved_a = fixture.root.path().join("saved-a.db");
        let saved_b = fixture.root.path().join("saved-b.db");
        let slot = ProjectSlot {
            around_observer_open: Some(Arc::new(move |opened| {
                if !opened {
                    std::fs::rename(&path, &saved_a)?;
                    std::fs::rename(&replacement, &path)?;
                } else {
                    std::fs::rename(&path, &saved_b)?;
                    std::fs::rename(&saved_a, &path)?;
                }
                Ok(())
            })),
            ..ProjectSlot::default()
        };
        let control = WorkControl::new(None);
        let original = FileIdentity::read(&fixture.path).unwrap();
        let error = slot
            .generation(fixture.root.path(), &fixture.path, &control)
            .unwrap_err();
        assert!(is_cache_unavailable(&error));
        assert_eq!(FileIdentity::read(&fixture.path).unwrap(), original);
        assert!(slot.observer.lock().is_none());
        ProjectSlot::default()
            .generation(fixture.root.path(), &fixture.path, &control)
            .unwrap();
    }

    #[tokio::test]
    async fn producer_path_aba_returns_pinned_result_without_cache_publication() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        fixture.insert("only-a.rs");
        other.insert("only-b.rs");
        for path in [&fixture.path, &other.path] {
            Connection::open(path)
                .unwrap()
                .pragma_update(None, "journal_mode", "DELETE")
                .unwrap();
        }
        let path = fixture.path.clone();
        let replacement = other.path.clone();
        let saved_a = fixture.root.path().join("saved-a.db");
        let saved_b = fixture.root.path().join("saved-b.db");
        let restore_path = path.clone();
        let restore_a = saved_a.clone();
        let opened = Arc::new(AtomicUsize::new(0));
        let opening = opened.clone();
        let cache = OverviewCache {
            before_database_open: Some(Arc::new(move || {
                if opening.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::fs::rename(&path, &saved_a)?;
                    std::fs::rename(&replacement, &path)?;
                }
                Ok(())
            })),
            before_compute: Some(Arc::new(move || {
                if opened.load(Ordering::SeqCst) == 1 {
                    std::fs::rename(&restore_path, &saved_b)?;
                    std::fs::rename(&restore_a, &restore_path)?;
                }
                Ok(())
            })),
            ..OverviewCache::default()
        };
        let control = WorkControl::new(None);
        let first = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(first.ranking.rows()[0].file, "only-b.rs");
        assert!(!first.stable);
        assert_eq!(cache.state.lock().bytes, 0);
        let second = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(second.ranking.rows()[0].file, "only-a.rs");
        assert_eq!(second.disposition, CacheDisposition::Owner);
        assert!(second.stable);
    }

    #[test]
    fn observer_rejects_symlink_retarget_aba_with_unchanged_resolved_database() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        for path in [&fixture.path, &other.path] {
            Connection::open(path)
                .unwrap()
                .pragma_update(None, "journal_mode", "DELETE")
                .unwrap();
        }
        let actual_a = fixture.root.path().join("a.db");
        std::fs::rename(&fixture.path, &actual_a).unwrap();
        std::os::unix::fs::symlink(&actual_a, &fixture.path).unwrap();
        let path = fixture.path.clone();
        let actual_b = other.path.clone();
        let slot = ProjectSlot {
            around_observer_open: Some(Arc::new(move |opened| {
                std::fs::remove_file(&path)?;
                std::os::unix::fs::symlink(if opened { &actual_a } else { &actual_b }, &path)?;
                Ok(())
            })),
            ..ProjectSlot::default()
        };
        let original = FileIdentity::read(&fixture.path).unwrap();
        let error = slot
            .generation(fixture.root.path(), &fixture.path, &WorkControl::new(None))
            .unwrap_err();
        assert!(is_cache_unavailable(&error));
        assert_eq!(FileIdentity::read(&fixture.path).unwrap(), original);
        assert!(slot.observer.lock().is_none());
    }

    #[tokio::test]
    async fn producer_ancestor_symlink_aba_cannot_publish_another_database_ranking() {
        let mut fixture = Fixture::new();
        let other = Fixture::new();
        fixture.insert("only-a.rs");
        other.insert("only-b.rs");
        for path in [&fixture.path, &other.path] {
            Connection::open(path)
                .unwrap()
                .pragma_update(None, "journal_mode", "DELETE")
                .unwrap();
        }
        let actual_a = fixture.root.path().join(".codesage/a");
        std::fs::create_dir(&actual_a).unwrap();
        std::fs::rename(&fixture.path, actual_a.join("index.db")).unwrap();
        let link = fixture.root.path().join(".codesage/current");
        std::os::unix::fs::symlink(&actual_a, &link).unwrap();
        fixture.path = link.join("index.db");
        let actual_b = other.path.parent().unwrap().to_owned();
        let before_link = link.clone();
        let opened = Arc::new(AtomicUsize::new(0));
        let opening = opened.clone();
        let cache = OverviewCache {
            before_database_open: Some(Arc::new(move || {
                if opening.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::fs::remove_file(&before_link)?;
                    std::os::unix::fs::symlink(&actual_b, &before_link)?;
                }
                Ok(())
            })),
            before_compute: Some(Arc::new(move || {
                if opened.load(Ordering::SeqCst) == 1 {
                    std::fs::remove_file(&link)?;
                    std::os::unix::fs::symlink(&actual_a, &link)?;
                }
                Ok(())
            })),
            ..OverviewCache::default()
        };
        let control = WorkControl::new(None);
        let first = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(first.ranking.rows()[0].file, "only-b.rs");
        assert!(!first.stable);
        assert_eq!(cache.state.lock().bytes, 0);
        let second = fixture.get(&cache, &control).await.unwrap();
        assert_eq!(second.ranking.rows()[0].file, "only-a.rs");
        assert_eq!(second.disposition, CacheDisposition::Owner);
        assert!(second.stable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_generation_probe_remains_visible_until_worker_exits() {
        for enabled in [true, false] {
            let fixture = Arc::new(Fixture::new());
            let diagnostics = Diagnostics::new(enabled);
            let request = Arc::new(
                diagnostics.begin_request("project_overview", fixture.root.path().to_str()),
            );
            let control = WorkControl::new(None);
            let started = Arc::new(Notify::new());
            let notify = started.clone();
            let (release, receiver) = std::sync::mpsc::channel();
            let receiver = Mutex::new(receiver);
            let cache = OverviewCache {
                before_generation: Some(Arc::new(move || {
                    notify.notify_one();
                    receiver
                        .lock()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    Ok(())
                })),
                ..OverviewCache::default()
            };
            let task = {
                let fixture = fixture.clone();
                let request = request.clone();
                let diagnostics = diagnostics.clone();
                let control = control.clone();
                tokio::spawn(async move {
                    let mut admission = fixture.coordinator.try_request(&control).unwrap();
                    admission.attach_project(fixture.root.path()).unwrap();
                    cache
                        .get(
                            fixture.root.path(),
                            &fixture.path,
                            &control,
                            CacheRequest {
                                coordinator: &fixture.coordinator,
                                diagnostics: &diagnostics,
                                request: &request,
                                producer_deadline: Instant::now() + Duration::from_secs(10),
                            },
                        )
                        .await
                })
            };
            started.notified().await;
            assert_eq!(request.work_state(), ("running", true));
            control.cancel(StopReason::ClientCancelled);
            request.finish("cancelled");
            task.abort();
            assert!(task.await.err().unwrap().is_cancelled());
            assert_eq!(control.reason(), Some(StopReason::ClientCancelled));
            assert_eq!(fixture.coordinator.snapshot().running[0], 1);
            assert_eq!(request.work_state(), ("running", true));
            if enabled {
                let snapshot = diagnostics.snapshot(256);
                assert_eq!(snapshot["gauges"]["active_requests"], 0);
                assert_eq!(snapshot["gauges"]["active_executions"], 1);
                assert_eq!(snapshot["gauges"]["zero_consumer_executions"], 1);
                assert_eq!(
                    snapshot["active_executions"][0]["tool"],
                    "overview_generation"
                );
            }
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while fixture.coordinator.snapshot().running[0] != 0 || request.work_state().1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if enabled {
                let snapshot = diagnostics.snapshot(256);
                assert_eq!(snapshot["gauges"]["active_executions"], 0);
                assert_eq!(snapshot["gauges"]["zero_consumer_executions"], 0);
                assert_eq!(snapshot["counters"]["executions_started"], 1);
                assert_eq!(snapshot["counters"]["execution_outcomes"]["cancelled"], 1);
                assert!(snapshot["recent_executions"][0]["execution_wall_ms"].is_number());
            } else {
                assert_eq!(
                    diagnostics.snapshot(256),
                    serde_json::json!({"enabled": false})
                );
            }
        }
    }

    #[tokio::test]
    async fn queued_generation_cancellation_has_no_execution_sample() {
        let fixture = Arc::new(Fixture::new());
        let holder_control = WorkControl::new(None);
        let holder = fixture
            .coordinator
            .acquire_execution(fixture.root.path(), WorkClass::Interactive, &holder_control)
            .await
            .unwrap();
        let diagnostics = Diagnostics::default();
        let request =
            Arc::new(diagnostics.begin_request("project_overview", fixture.root.path().to_str()));
        let control = WorkControl::new(None);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let cache = OverviewCache {
            before_generation: Some(Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
            ..OverviewCache::default()
        };
        let task = {
            let fixture = fixture.clone();
            let request = request.clone();
            let diagnostics = diagnostics.clone();
            let control = control.clone();
            tokio::spawn(async move {
                cache
                    .get(
                        fixture.root.path(),
                        &fixture.path,
                        &control,
                        CacheRequest {
                            coordinator: &fixture.coordinator,
                            diagnostics: &diagnostics,
                            request: &request,
                            producer_deadline: Instant::now() + Duration::from_secs(10),
                        },
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.coordinator.snapshot().queued[0] != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.cancel(StopReason::ClientCancelled);
        assert!(task.await.unwrap().is_err());
        request.finish("cancelled");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!request.work_state().1);
        let snapshot = diagnostics.snapshot(256);
        assert_eq!(snapshot["recent_executions"][0]["outcome"], "cancelled");
        assert_eq!(
            snapshot["recent_executions"][0]["execution_wall_ms"],
            serde_json::Value::Null
        );
        assert_eq!(snapshot["gauges"]["active_executions"], 0);
        assert_eq!(snapshot["gauges"]["queued_executions"], 0);
        drop(holder);
    }
}
