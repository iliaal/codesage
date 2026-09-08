#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(not(unix))]
use anyhow::{Result, bail};

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsString,
        fs::{self, OpenOptions},
        io::{self, Write},
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        pin::Pin,
        process::{Child, Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context as TaskContext, Poll},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, bail};
    use rmcp::ServiceExt;
    use tokio::{
        io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, copy},
        net::{UnixListener, UnixStream},
        signal::unix::{SignalKind, signal},
        task::JoinSet,
        time::sleep,
    };

    use crate::mcp::{CodeSageServer, CodeSageServerState};

    const START_TIMEOUT: Duration = Duration::from_secs(5);
    const RETRY_DELAY: Duration = Duration::from_millis(25);

    /// Match the waiter's three-round cap while allowing a live child a slow start.
    const SPAWNER_WAIT_CAP: Duration = START_TIMEOUT.saturating_mul(3);

    /// Leave room within `daemon stop`'s 10-second SIGTERM window for cleanup.
    const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

    /// A panicking timestamp writer leaves the previous value intact, so poison recovery is safe.
    fn lock_clock(mutex: &Mutex<Instant>) -> std::sync::MutexGuard<'_, Instant> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Await every task in `clients`, bounded by `timeout`. Returns how many
    /// tasks were still in flight when the bound expired (those are aborted);
    /// `0` means a clean drain.
    async fn drain_client_tasks(clients: &mut JoinSet<()>, timeout: Duration) -> usize {
        if clients.is_empty() {
            return 0;
        }
        let all_done = async { while clients.join_next().await.is_some() {} };
        if tokio::time::timeout(timeout, all_done).await.is_err() {
            let remaining = clients.len();
            clients.abort_all();
            return remaining;
        }
        0
    }

    /// Release model memory when no clients remain. `CODESAGE_DAEMON_IDLE_TIMEOUT_SECS=0`
    /// disables idle exit.
    const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);

    /// Zero disables idle exit; invalid values retain the default.
    fn daemon_idle_timeout() -> Duration {
        match std::env::var("CODESAGE_DAEMON_IDLE_TIMEOUT_SECS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) => Duration::from_secs(secs),
                Err(_) => {
                    tracing::warn!(
                        value = %raw,
                        "invalid CODESAGE_DAEMON_IDLE_TIMEOUT_SECS (want integer seconds); \
                         using default {:?}",
                        DEFAULT_IDLE_TIMEOUT
                    );
                    DEFAULT_IDLE_TIMEOUT
                }
            },
            Err(_) => DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// Evict unused models even while clients stay connected. `CODESAGE_MODEL_IDLE_SECS=0`
    /// disables model eviction, not daemon idle exit.
    const DEFAULT_MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(900);

    fn model_idle_timeout() -> Duration {
        match std::env::var("CODESAGE_MODEL_IDLE_SECS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) => Duration::from_secs(secs),
                Err(_) => {
                    tracing::warn!(
                        value = %raw,
                        "invalid CODESAGE_MODEL_IDLE_SECS (want integer seconds); using default {:?}",
                        DEFAULT_MODEL_IDLE_TIMEOUT
                    );
                    DEFAULT_MODEL_IDLE_TIMEOUT
                }
            },
            Err(_) => DEFAULT_MODEL_IDLE_TIMEOUT,
        }
    }

    /// glibc retains freed ORT host buffers after model eviction; return unused pages to the OS.
    fn free_retained_heap() {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        // SAFETY: malloc_trim is thread-safe and has no preconditions.
        unsafe {
            libc::malloc_trim(0);
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DaemonPaths {
        runtime_dir: PathBuf,
        socket: PathBuf,
        lock: PathBuf,
        pid: PathBuf,
        log: PathBuf,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct DaemonPid {
        pid: i32,
        start_time_ticks: Option<u64>,
    }

    impl DaemonPaths {
        fn for_current_exe(runtime_dir: Option<PathBuf>) -> Result<Self> {
            let exe = std::env::current_exe().context("resolving current executable")?;
            Self::for_exe(runtime_dir.unwrap_or_else(default_runtime_dir), &exe)
        }

        fn for_exe(runtime_dir: PathBuf, exe: &Path) -> Result<Self> {
            let key = daemon_key_for_exe(exe)?;
            let socket = runtime_dir.join(format!("mcp-{key}.sock"));
            std::os::unix::net::SocketAddr::from_pathname(&socket).with_context(|| {
                format!(
                    "invalid daemon socket pathname {} ({} bytes); choose a shorter runtime \
                     directory with CODESAGE_DAEMON_RUNTIME_DIR, XDG_RUNTIME_DIR, or --runtime-dir",
                    socket.display(),
                    socket.as_os_str().len(),
                )
            })?;
            Ok(Self {
                socket,
                lock: runtime_dir.join(format!("mcp-{key}.lock")),
                pid: runtime_dir.join(format!("mcp-{key}.pid")),
                log: runtime_dir.join(format!("mcp-{key}.log")),
                runtime_dir,
            })
        }
    }

    struct StartLock {
        path: PathBuf,
    }

    impl StartLock {
        fn try_acquire(path: &Path) -> Result<Option<Self>> {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("writing {}", path.display()))?;
                    Ok(Some(Self {
                        path: path.to_path_buf(),
                    }))
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
                Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
    }

    impl Drop for StartLock {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    /// Find a listening daemon for this executable's identity without starting one.
    pub(crate) fn running_daemon_socket() -> Option<PathBuf> {
        let paths = DaemonPaths::for_current_exe(None).ok()?;
        std::os::unix::net::UnixStream::connect(&paths.socket)
            .is_ok()
            .then_some(paths.socket)
    }

    pub(crate) async fn run_mcp_shim(
        runtime_dir: Option<PathBuf>,
        default_project: Option<String>,
    ) -> Result<()> {
        let paths = DaemonPaths::for_current_exe(runtime_dir)?;
        prepare_runtime_dir(&paths.runtime_dir)?;
        let stream = ensure_daemon(&paths).await?;
        proxy_stdio(stream, default_project).await
    }

    /// `codesage daemon status` — print the running daemon's pid + socket
    /// path, or report "not running". Exit code 0 if running, 1 if not.
    pub(crate) async fn run_daemon_status(runtime_dir: Option<PathBuf>) -> Result<()> {
        let paths = existing_daemon_paths(runtime_dir)?;
        let Some(pid_record) = read_daemon_pid_file(&paths.pid) else {
            println!("not running (no pid file at {})", paths.pid.display());
            std::process::exit(1);
        };
        if !daemon_pid_file_matches(&paths, pid_record) {
            println!(
                "not running (pid file references stale or non-daemon pid {}; left over from a previous run)",
                pid_record.pid
            );
            std::process::exit(1);
        }
        let pid = pid_record.pid;
        // PID liveness alone does not establish that the daemon accepts connections.
        let socket_reachable = UnixStream::connect(&paths.socket).await.is_ok();
        println!("running");
        println!("  pid:    {}", pid);
        println!("  socket: {}", paths.socket.display());
        println!(
            "  reachable: {}",
            if socket_reachable {
                "yes"
            } else {
                "no (pid alive but socket not accepting connections)"
            }
        );
        println!("  log:    {}", paths.log.display());
        Ok(())
    }

    /// `codesage daemon stop` — SIGTERM the running daemon and wait
    /// (bounded) for it to exit + clean up its socket/pid files.
    pub(crate) async fn run_daemon_stop(runtime_dir: Option<PathBuf>) -> Result<()> {
        let paths = existing_daemon_paths(runtime_dir)?;
        let Some(pid_record) = read_daemon_pid_file(&paths.pid) else {
            println!("not running (no pid file at {})", paths.pid.display());
            return Ok(());
        };
        if !daemon_pid_file_matches(&paths, pid_record) {
            println!(
                "not running (pid file references stale or non-daemon pid {}); cleaning stale files",
                pid_record.pid
            );
            let _ = fs::remove_file(&paths.pid);
            let _ = fs::remove_file(&paths.socket);
            return Ok(());
        }
        let pid = pid_record.pid;
        // SAFETY: kill is async-signal-safe.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            bail!("failed to SIGTERM pid {}: {}", pid, err);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !pid_alive(pid) {
                println!("stopped daemon (pid {})", pid);
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "daemon (pid {}) did not exit within 10s of SIGTERM; \
             send SIGKILL manually if needed",
            pid
        )
    }

    pub(crate) async fn run_daemon(runtime_dir: Option<PathBuf>) -> Result<()> {
        let paths = DaemonPaths::for_current_exe(runtime_dir)?;
        prepare_runtime_dir(&paths.runtime_dir)?;

        if UnixStream::connect(&paths.socket).await.is_ok() {
            bail!(
                "codesage MCP daemon is already listening at {}",
                paths.socket.display()
            );
        }
        if paths.socket.exists() {
            fs::remove_file(&paths.socket)
                .with_context(|| format!("removing stale socket {}", paths.socket.display()))?;
        }

        // Restrict permissions at bind time to avoid a world-readable window before chmod.
        let prev_umask = unsafe { libc::umask(0o077) };
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("binding {}", paths.socket.display()));
        unsafe { libc::umask(prev_umask) };
        let listener = listener?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", paths.socket.display()))?;
        write_daemon_pid(&paths.pid)?;

        // Reap older builds only after our runtime files exist; otherwise models remain duplicated.
        reap_stale_version_daemons(&paths);

        tracing::info!(socket = %paths.socket.display(), "codesage MCP daemon listening");
        let state = Arc::new(CodeSageServerState::new());
        let our_uid = unsafe { libc::getuid() };

        // Open client connections prevent daemon idle exit, so evict unused models separately.
        let model_idle = model_idle_timeout();
        if !model_idle.is_zero() {
            let state_evict = state.clone();
            let evict_poll = model_idle
                .min(Duration::from_secs(60))
                .max(Duration::from_secs(1));
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(evict_poll);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    let s = state_evict.clone();
                    // An ORT Session drop can block on CUDA teardown; keep it
                    // off the async worker threads.
                    let evicted = tokio::task::spawn_blocking(move || {
                        let n = s.evict_idle_models(model_idle);
                        if n > 0 {
                            free_retained_heap();
                        }
                        n
                    })
                    .await
                    .unwrap_or(0);
                    if evicted > 0 {
                        tracing::info!(evicted, "evicted idle pooled models");
                    }
                }
            });
        }

        let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

        let idle_timeout = daemon_idle_timeout();
        let active = Arc::new(AtomicUsize::new(0));
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        // Byte activity is diagnostic only: reaping a quiet connected client kills its MCP session.
        let daemon_last_byte = Arc::new(Mutex::new(Instant::now()));
        let mut silence_reported = false;
        let idle_poll = if idle_timeout.is_zero() {
            Duration::from_secs(3600)
        } else {
            idle_timeout
                .min(Duration::from_secs(60))
                .max(Duration::from_secs(1))
        };
        let mut idle_tick = tokio::time::interval(idle_poll);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Retain tasks so shutdown can drain in-flight responses.
        let mut clients: JoinSet<()> = JoinSet::new();

        let shutdown_reason = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    // Retry accept failures without dropping existing clients; delay prevents a persistent-error spin.
                    let (stream, _) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                socket = %paths.socket.display(),
                                "transient error accepting MCP daemon connection; continuing"
                            );
                            sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };

                    // Peer credentials also protect against a runtime directory with permissive access.
                    match stream.peer_cred() {
                        Ok(cred) if cred.uid() != our_uid => {
                            tracing::warn!(
                                peer_uid = cred.uid(),
                                our_uid,
                                "refusing MCP daemon connection from foreign UID"
                            );
                            drop(stream);
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to read peer_cred; refusing connection");
                            drop(stream);
                            continue;
                        }
                    }

                    let server = CodeSageServer::with_state(state.clone());
                    active.fetch_add(1, Ordering::SeqCst);
                    *lock_clock(&last_activity) = Instant::now();
                    *lock_clock(&daemon_last_byte) = Instant::now();
                    let active_for_conn = active.clone();
                    let active_for_stop = active.clone();
                    let last_activity_for_conn = last_activity.clone();
                    let last_byte_for_conn = daemon_last_byte.clone();
                    let state_for_conn = state.clone();
                    clients.spawn(async move {
                        if let Err(e) = serve_client(server, stream, last_byte_for_conn).await {
                            tracing::debug!(error = %e, "MCP daemon client connection ended");
                        }
                        let remaining = active_for_conn.fetch_sub(1, Ordering::SeqCst) - 1;
                        // Reset the idle clock on disconnect so the timeout
                        // measures continuous idleness, not uptime.
                        *lock_clock(&last_activity_for_conn) = Instant::now();
                        // Stop watchers off-runtime because joining can block. Recheck the client count
                        // under the registry lock so a new connection cancels the stop.
                        if remaining == 0 {
                            let state = state_for_conn.clone();
                            let stop = tokio::task::spawn_blocking(move || {
                                state.shutdown_watchers_if_no_client(
                                    crate::mcp::WATCHER_STOP_WAIT,
                                    &active_for_stop,
                                )
                            })
                            .await;
                            if let Err(e) = stop {
                                tracing::warn!(error = %e, "watcher stop task failed");
                            }
                        }
                    });
                }
                // `Some` disables this arm when the set is empty.
                Some(_) = clients.join_next() => {}
                _ = idle_tick.tick() => {
                    if !idle_timeout.is_zero() {
                        let connected = active.load(Ordering::SeqCst);
                        if connected == 0 {
                            if lock_clock(&last_activity).elapsed() >= idle_timeout {
                                break "idle";
                            }
                        } else {
                            let silent = lock_clock(&daemon_last_byte).elapsed();
                            // Log once per silent window; new bytes re-arm the report.
                            if silent >= idle_timeout && !silence_reported {
                                silence_reported = true;
                                tracing::info!(
                                    connected,
                                    silent_secs = silent.as_secs(),
                                    client_idle_max_secs = client_idle_max().as_secs(),
                                    "daemon held open by connected but silent clients; \
                                     pools stay warm until they disconnect or hit the \
                                     per-connection idle ceiling"
                                );
                            } else if silent < idle_timeout {
                                silence_reported = false;
                            }
                        }
                    }
                }
                _ = sigterm.recv() => break "SIGTERM",
                _ = sigint.recv() => break "SIGINT",
            }
        };
        tracing::info!(
            reason = shutdown_reason,
            "codesage MCP daemon shutting down"
        );

        // Give watchers a bounded drain before process exit reaps their remaining threads.
        let still_stopping = state.shutdown_all_watchers(SHUTDOWN_DRAIN);
        if still_stopping > 0 {
            tracing::warn!(
                still_stopping,
                "watchers still draining after {:?}; process exit will reap them",
                SHUTDOWN_DRAIN
            );
        }

        // Keep the listener bound while draining so late shims cannot race a replacement daemon.
        let in_flight = clients.len();
        let abandoned = if in_flight > 0 {
            tracing::info!(in_flight, "draining in-flight client connections");
            let still_in_flight = drain_client_tasks(&mut clients, SHUTDOWN_DRAIN).await;
            if still_in_flight > 0 {
                tracing::warn!(
                    still_in_flight,
                    "client connections still in flight after {:?}; aborting them",
                    SHUTDOWN_DRAIN
                );
            }
            still_in_flight
        } else {
            0
        };

        // Best-effort cleanup. The runtime dir is left in place because
        // other daemon keys may share it.
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.pid);

        // Runtime drop joins running `spawn_blocking` work, which abort_all cannot cancel.
        // Exit directly so wedged ONNX/SQLite calls cannot hold shutdown past its deadline.
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                "exiting with aborted connections whose blocking tool tasks may still be running; abandoning them"
            );
        }
        std::process::exit(0)
    }

    /// Transport inactivity ceiling, measured from the last client byte, not connection start.
    /// rmcp tool dispatch is private, so this also bounds hung requests without per-tool timeouts.
    /// `CODESAGE_CLIENT_IDLE_MAX_SECS=0` disables the ceiling.
    const DEFAULT_CLIENT_IDLE_MAX: Duration = Duration::from_secs(4 * 3600);

    /// Zero disables the ceiling; invalid values retain the default.
    fn client_idle_max() -> Duration {
        match std::env::var("CODESAGE_CLIENT_IDLE_MAX_SECS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) => Duration::from_secs(secs),
                Err(_) => {
                    tracing::warn!(
                        value = %raw,
                        "invalid CODESAGE_CLIENT_IDLE_MAX_SECS (want integer seconds); \
                         using default {:?}",
                        DEFAULT_CLIENT_IDLE_MAX
                    );
                    DEFAULT_CLIENT_IDLE_MAX
                }
            },
            Err(_) => DEFAULT_CLIENT_IDLE_MAX,
        }
    }

    /// Track client reads so active sessions extend their lifetime while hung requests time out.
    struct ActivityStream {
        inner: UnixStream,
        last_activity: Arc<Mutex<Instant>>,
        /// Diagnostic activity across clients; separate from this connection's idle ceiling.
        daemon_last_byte: Arc<Mutex<Instant>>,
    }

    impl AsyncRead for ActivityStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let before = buf.filled().len();
            let r = Pin::new(&mut this.inner).poll_read(cx, buf);
            if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
                let now = Instant::now();
                *lock_clock(&this.last_activity) = now;
                *lock_clock(&this.daemon_last_byte) = now;
            }
            r
        }
    }

    impl AsyncWrite for ActivityStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    async fn serve_client(
        server: CodeSageServer,
        stream: UnixStream,
        daemon_last_byte: Arc<Mutex<Instant>>,
    ) -> Result<()> {
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let tracked = ActivityStream {
            inner: stream,
            last_activity: last_activity.clone(),
            daemon_last_byte,
        };
        let idle_max = client_idle_max();

        // Bound pre-initialize silence too: the post-handshake idle check cannot reap these peers.
        // Zero disables both ceilings.
        let serve_fut = server.serve(tracked);
        let service = if idle_max.is_zero() {
            serve_fut
                .await
                .map_err(|e| anyhow::anyhow!("MCP daemon server error: {e}"))?
        } else {
            match tokio::time::timeout(idle_max, serve_fut).await {
                Ok(res) => res.map_err(|e| anyhow::anyhow!("MCP daemon server error: {e}"))?,
                Err(_) => {
                    tracing::warn!(
                        "MCP client never completed the initialize handshake within {:?}; dropping",
                        idle_max
                    );
                    return Ok(());
                }
            }
        };
        let wait = service.waiting();
        tokio::pin!(wait);

        if idle_max.is_zero() {
            return match wait.await {
                Ok(_) => Ok(()),
                Err(e) => Err(anyhow::anyhow!("MCP daemon server stopped: {e}")),
            };
        }

        let poll = idle_max
            .min(Duration::from_secs(60))
            .max(Duration::from_secs(1));
        let mut idle_tick = tokio::time::interval(poll);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                res = &mut wait => {
                    return match res {
                        Ok(_) => Ok(()),
                        Err(e) => Err(anyhow::anyhow!("MCP daemon server stopped: {e}")),
                    };
                }
                _ = idle_tick.tick() => {
                    let idle = lock_clock(&last_activity).elapsed();
                    if idle >= idle_max {
                        tracing::warn!(
                            "MCP client idle for {:?} (>= {:?}); dropping",
                            idle, idle_max
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn ensure_daemon(paths: &DaemonPaths) -> Result<UnixStream> {
        if let Ok(stream) = UnixStream::connect(&paths.socket).await {
            return Ok(stream);
        }

        // Reclaim a start lock only after its holder dies; live holders may still be starting.
        for attempt in 0..3 {
            match StartLock::try_acquire(&paths.lock)? {
                Some(_lock) => {
                    if attempt > 0 {
                        tracing::warn!(
                            attempt = attempt + 1,
                            "acquired daemon start lock after recovery"
                        );
                    }
                    remove_stale_socket(paths).await?;
                    let mut child = spawn_daemon(paths)?;
                    return wait_for_spawned_daemon(
                        &mut child,
                        paths,
                        START_TIMEOUT,
                        SPAWNER_WAIT_CAP,
                    )
                    .await;
                }
                None => {
                    match wait_for_socket(&paths.socket, START_TIMEOUT, paths).await {
                        Ok(stream) => return Ok(stream),
                        Err(wait_err) => {
                            // Never unlink a live holder's lock: competing shims could both become starters.
                            match clean_stale_lock(&paths.lock)? {
                                LockCleanup::HolderDead => {
                                    tracing::warn!(
                                        attempt = attempt + 1,
                                        lock = %paths.lock.display(),
                                        "daemon lock holder appears dead; reclaiming"
                                    );
                                    continue;
                                }
                                LockCleanup::HolderAlive => {
                                    if attempt + 1 == 3 {
                                        bail!(
                                            "codesage MCP daemon did not become ready at {} \
                                             within {:?} (lock holder still alive); \
                                             see {} for daemon-side errors: {}",
                                            paths.socket.display(),
                                            START_TIMEOUT,
                                            paths.log.display(),
                                            wait_err
                                        );
                                    }
                                    tracing::debug!(
                                        attempt = attempt + 1,
                                        "daemon lock holder still alive; waiting for socket"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        bail!(
            "codesage MCP daemon failed to start after multiple attempts; see {}",
            paths.log.display()
        )
    }

    enum LockCleanup {
        HolderAlive,
        HolderDead,
    }

    /// Retain a live holder's lock; remove dead or malformed records so acquisition can retry.
    fn clean_stale_lock(lock_path: &Path) -> Result<LockCleanup> {
        let contents = match fs::read_to_string(lock_path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(LockCleanup::HolderDead);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", lock_path.display())),
        };
        let pid: i32 = match contents.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => {
                let _ = fs::remove_file(lock_path);
                return Ok(LockCleanup::HolderDead);
            }
        };
        if pid_alive(pid) {
            return Ok(LockCleanup::HolderAlive);
        }
        let _ = fs::remove_file(lock_path);
        Ok(LockCleanup::HolderDead)
    }

    /// `kill(pid, 0)` sends no signal; EPERM still proves the process exists.
    fn pid_alive(pid: i32) -> bool {
        // SAFETY: kill is async-signal-safe and side-effect-free for sig=0.
        let r = unsafe { libc::kill(pid, 0) };
        if r == 0 {
            return true;
        }
        matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
    }

    /// Rotate on spawn, retaining recent failures across repeated daemon crashes.
    const LOG_ROTATE_AT_BYTES: u64 = 4 * 1024 * 1024;
    const LOG_KEEP_GENERATIONS: usize = 3;

    fn spawn_daemon(paths: &DaemonPaths) -> Result<Child> {
        let exe = std::env::current_exe().context("resolving current executable")?;
        rotate_log_if_large(&paths.log);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log)
            .with_context(|| format!("opening daemon log {}", paths.log.display()))?;
        let mut cmd = Command::new(exe);
        cmd.arg("daemon")
            .arg("--runtime-dir")
            .arg(&paths.runtime_dir);
        // Set before the child's first allocation: glibc's default arena count retains
        // model-load scratch across ORT/tokio threads. Preserve operator overrides.
        if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
            cmd.env("MALLOC_ARENA_MAX", "2");
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(
                log.try_clone().context("cloning daemon log for stdout")?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting codesage MCP daemon")
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SpawnerWaitDecision {
        /// Child alive, cap not reached: wait another socket round.
        KeepWaiting,
        /// Child exited: the daemon died during startup, fail now.
        ChildExited,
        /// Child alive but the total cap elapsed: wedged, give up.
        CapExceeded,
    }

    /// Retry decision after one failed socket-wait round on the spawner
    /// path. A dead child wins over the cap: its exit is the informative
    /// failure and waiting longer cannot help.
    fn spawner_wait_decision(
        child_alive: bool,
        waited: Duration,
        cap: Duration,
    ) -> SpawnerWaitDecision {
        if !child_alive {
            SpawnerWaitDecision::ChildExited
        } else if waited >= cap {
            SpawnerWaitDecision::CapExceeded
        } else {
            SpawnerWaitDecision::KeepWaiting
        }
    }

    /// Retry while the child lives, up to `cap`. Stale same-key PID files can end a
    /// socket round early before the child binds and replaces them.
    async fn wait_for_spawned_daemon(
        child: &mut Child,
        paths: &DaemonPaths,
        round: Duration,
        cap: Duration,
    ) -> Result<UnixStream> {
        let started = Instant::now();
        loop {
            let wait_err = match wait_for_socket(&paths.socket, round, paths).await {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };
            let exit = child
                .try_wait()
                .context("checking spawned daemon liveness")?;
            match spawner_wait_decision(exit.is_none(), started.elapsed(), cap) {
                SpawnerWaitDecision::KeepWaiting => {
                    tracing::info!(
                        waited = ?started.elapsed(),
                        "spawned daemon not ready yet but still alive; waiting another round"
                    );
                }
                SpawnerWaitDecision::ChildExited => {
                    let status = exit.map_or_else(|| "unknown".to_string(), |s| s.to_string());
                    return Err(wait_err.context(format!(
                        "spawned codesage daemon exited during startup ({status}); \
                         see {} for the failure",
                        paths.log.display()
                    )));
                }
                SpawnerWaitDecision::CapExceeded => {
                    return Err(wait_err.context(format!(
                        "spawned codesage daemon still alive but not ready after {:?}; \
                         see {} for daemon-side progress",
                        started.elapsed(),
                        paths.log.display()
                    )));
                }
            }
        }
    }

    fn rotate_log_if_large(log: &Path) {
        let Ok(meta) = fs::metadata(log) else {
            return;
        };
        if meta.len() < LOG_ROTATE_AT_BYTES {
            return;
        }
        // Rotate oldest first to preserve generations. Logging failures must not block startup.
        for n in (1..LOG_KEEP_GENERATIONS).rev() {
            let src = generation_path(log, n);
            let dst = generation_path(log, n + 1);
            let _ = fs::rename(&src, &dst);
        }
        let _ = fs::rename(log, generation_path(log, 1));
    }

    fn generation_path(log: &Path, n: usize) -> PathBuf {
        let mut name = log
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        name.push(format!(".{n}"));
        log.with_file_name(name)
    }

    async fn remove_stale_socket(paths: &DaemonPaths) -> Result<()> {
        if UnixStream::connect(&paths.socket).await.is_ok() {
            return Ok(());
        }
        if paths.socket.exists() {
            fs::remove_file(&paths.socket)
                .with_context(|| format!("removing stale socket {}", paths.socket.display()))
        } else {
            Ok(())
        }
    }

    /// Fail early when the recorded daemon exits; include its log path for diagnostics.
    async fn wait_for_socket(
        path: &Path,
        timeout: Duration,
        paths: &DaemonPaths,
    ) -> Result<UnixStream> {
        let deadline = Instant::now() + timeout;
        let mut last_alive_check = Instant::now();
        loop {
            let error = match UnixStream::connect(path).await {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for codesage MCP daemon at {}: {} \
                     (daemon stdout/stderr at {})",
                    path.display(),
                    error,
                    paths.log.display()
                );
            }
            if Instant::now().duration_since(last_alive_check) >= Duration::from_millis(100) {
                if let Some(pid_record) = read_daemon_pid_file(&paths.pid)
                    && !daemon_pid_file_matches(paths, pid_record)
                {
                    bail!(
                        "codesage MCP daemon (pid {}) exited before becoming ready or no longer matches its pid file; \
                         see {} for the failure",
                        pid_record.pid,
                        paths.log.display()
                    );
                }
                last_alive_check = Instant::now();
            }
            sleep(RETRY_DELAY).await;
        }
    }

    fn write_daemon_pid(pid_path: &Path) -> Result<()> {
        let pid = i32::try_from(std::process::id()).context("current pid does not fit i32")?;
        let record = DaemonPid {
            pid,
            start_time_ticks: process_start_time_ticks(pid),
        };
        let contents = match record.start_time_ticks {
            Some(start_time_ticks) => {
                format!(
                    "pid={}\nstart_time_ticks={}\n",
                    record.pid, start_time_ticks
                )
            }
            None => format!("{}\n", record.pid),
        };
        fs::write(pid_path, contents).with_context(|| format!("writing {}", pid_path.display()))
    }

    fn read_daemon_pid_file(pid_path: &Path) -> Option<DaemonPid> {
        parse_daemon_pid_file(&fs::read_to_string(pid_path).ok()?)
    }

    fn parse_daemon_pid_file(contents: &str) -> Option<DaemonPid> {
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(pid) = trimmed.parse::<i32>() {
            if pid > 0 {
                return Some(DaemonPid {
                    pid,
                    start_time_ticks: None,
                });
            }
            return None;
        }

        let mut pid = None;
        let mut start_time_ticks = None;
        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "pid" => {
                    pid = value.trim().parse::<i32>().ok().filter(|pid| *pid > 0);
                }
                "start_time_ticks" => {
                    start_time_ticks = value.trim().parse::<u64>().ok();
                }
                _ => {}
            }
        }
        Some(DaemonPid {
            pid: pid?,
            start_time_ticks,
        })
    }

    fn daemon_pid_file_matches(paths: &DaemonPaths, record: DaemonPid) -> bool {
        pid_alive(record.pid) && daemon_pid_matches_process(paths, record)
    }

    /// Reclaim model memory from older builds sharing this runtime directory.
    /// Validate each sibling against its own paths, then require a strictly earlier
    /// start time. Missing start times must never permit signalling a recycled PID.
    fn reap_stale_version_daemons(paths: &DaemonPaths) {
        let our_start = i32::try_from(std::process::id())
            .ok()
            .and_then(process_start_time_ticks);
        let Ok(entries) = fs::read_dir(&paths.runtime_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let sibling = entry.path();
            if sibling == paths.pid {
                continue; // our own pid file
            }
            if !sibling
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_sibling_daemon_pidfile)
            {
                continue;
            }
            let Some(sibling_paths) = paths_from_pid_file(&sibling) else {
                continue;
            };
            let Some(record) = read_daemon_pid_file(&sibling) else {
                continue;
            };
            let validated = daemon_pid_file_matches(&sibling_paths, record);
            match sibling_reap_action(validated, our_start, record.start_time_ticks) {
                SiblingReapAction::CleanupFiles => {
                    cleanup_sibling_runtime_files(&sibling);
                }
                SiblingReapAction::Leave => {
                    tracing::debug!(
                        sibling_pid = record.pid,
                        pidfile = %sibling.display(),
                        "leaving live sibling daemon alone (started later, or start-time comparison unavailable)"
                    );
                }
                SiblingReapAction::Reap => {
                    // SAFETY: kill is async-signal-safe.
                    let rc = unsafe { libc::kill(record.pid, libc::SIGTERM) };
                    if rc == 0 {
                        tracing::info!(
                            stale_pid = record.pid,
                            pidfile = %sibling.display(),
                            "reaped stale-version codesage daemon to reclaim its model memory"
                        );
                    } else {
                        tracing::warn!(
                            stale_pid = record.pid,
                            error = %io::Error::last_os_error(),
                            "failed to SIGTERM stale-version codesage daemon"
                        );
                    }
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SiblingReapAction {
        /// Dead or recycled pid: remove its leftover runtime files.
        CleanupFiles,
        /// Live sibling we must not touch.
        Leave,
        /// Live stale-version daemon that provably started before us.
        Reap,
    }

    /// Require validation against the sibling's paths and comparable start times.
    /// Strict ordering prevents racing daemons from reaping each other.
    fn sibling_reap_action(
        validated: bool,
        our_start: Option<u64>,
        their_start: Option<u64>,
    ) -> SiblingReapAction {
        if !validated {
            return SiblingReapAction::CleanupFiles;
        }
        match (our_start, their_start) {
            (Some(ours), Some(theirs)) if theirs < ours => SiblingReapAction::Reap,
            _ => SiblingReapAction::Leave,
        }
    }

    fn is_sibling_daemon_pidfile(name: &str) -> bool {
        name.starts_with("mcp-") && name.ends_with(".pid")
    }

    fn cleanup_sibling_runtime_files(pidfile: &Path) {
        let _ = fs::remove_file(pidfile);
        for ext in ["sock", "lock"] {
            let _ = fs::remove_file(pidfile.with_extension(ext));
        }
    }

    fn daemon_pid_matches_process(paths: &DaemonPaths, record: DaemonPid) -> bool {
        #[cfg(target_os = "linux")]
        {
            if let Some(expected_start) = record.start_time_ticks {
                return process_start_time_ticks(record.pid) == Some(expected_start);
            }
            legacy_pid_looks_like_daemon(record.pid, paths)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = record;
            daemon_socket_reachable(paths)
        }
    }

    #[cfg(target_os = "linux")]
    fn process_start_time_ticks(pid: i32) -> Option<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, after_comm) = stat.rsplit_once(") ")?;
        after_comm.split_whitespace().nth(19)?.parse().ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn process_start_time_ticks(_pid: i32) -> Option<u64> {
        None
    }

    #[cfg(not(target_os = "linux"))]
    fn daemon_socket_reachable(paths: &DaemonPaths) -> bool {
        std::os::unix::net::UnixStream::connect(&paths.socket).is_ok()
    }

    #[cfg(target_os = "linux")]
    fn legacy_pid_looks_like_daemon(pid: i32, paths: &DaemonPaths) -> bool {
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        if !args.iter().any(|arg| arg == "daemon") {
            return false;
        }
        args.windows(2).any(|pair| {
            pair[0] == "--runtime-dir" && Path::new(pair[1].as_str()) == paths.runtime_dir.as_path()
        }) || args.iter().any(|arg| {
            arg.strip_prefix("--runtime-dir=")
                .is_some_and(|dir| Path::new(dir) == paths.runtime_dir.as_path())
        })
    }

    /// Exit status for a finished stdio-proxy direction: a copy error means
    /// the far side died mid-stream, so exiting 0 would let a dead daemon
    /// look like a clean EOF to the client.
    fn proxy_exit_code<E>(res: &Result<(), E>) -> i32 {
        match res {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    async fn proxy_stdio(stream: UnixStream, default_project: Option<String>) -> Result<()> {
        let (mut socket_read, mut socket_write) = tokio::io::split(stream);
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

        let stdin_to_socket = async {
            let res = if let Some(dp) = default_project.as_ref() {
                crate::mcp::pump_lines_injecting(&mut stdin, &mut socket_write, dp.clone()).await
            } else {
                copy(&mut stdin, &mut socket_write).await.map(|_| ())
            };
            let _ = socket_write.shutdown().await;
            res
        };
        let socket_to_stdout = async {
            let copy_res = copy(&mut socket_read, &mut stdout).await;
            let _ = stdout.flush().await;
            copy_res.map(|_| ())
        };
        tokio::pin!(stdin_to_socket);
        tokio::pin!(socket_to_stdout);

        // `try_join!` would hang on open stdin after the daemon disconnects.
        // Exit directly: dropping tokio's runtime joins the blocking stdin reader,
        // whose read syscall cannot be cancelled by dropping its future.
        tokio::select! {
            res = &mut socket_to_stdout => {
                if let Err(e) = &res {
                    tracing::warn!(error = %e, "MCP daemon connection closed with error");
                }
                std::process::exit(proxy_exit_code(&res));
            }
            res = &mut stdin_to_socket => {
                if let Err(e) = &res {
                    tracing::warn!(error = %e, "MCP client stdin closed with error");
                }
                let code = proxy_exit_code(&res);
                // Stdin EOF: drain in-flight server response, then exit.
                let _ = socket_to_stdout.await;
                std::process::exit(code);
            }
        }
    }

    fn validate_runtime_dir(path: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        // Another user can pre-stage the shared-/tmp path. Reject symlinks and foreign
        // ownership before creating sockets or trusting PID files.
        let meta =
            fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "refusing to use runtime dir {}: it is a symlink \
                 (possible cross-user attack on a shared /tmp)",
                path.display()
            );
        }
        if !meta.file_type().is_dir() {
            bail!(
                "refusing to use runtime dir {}: it is not a directory",
                path.display()
            );
        }
        // SAFETY: getuid is async-signal-safe and always succeeds.
        let our_uid = unsafe { libc::getuid() };
        if meta.uid() != our_uid {
            bail!(
                "refusing to use runtime dir {}: owned by uid {} but we are uid {}",
                path.display(),
                meta.uid(),
                our_uid
            );
        }
        Ok(())
    }

    fn prepare_runtime_dir(path: &Path) -> Result<()> {
        fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        validate_runtime_dir(path)?;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
        Ok(())
    }

    fn runtime_dir_exists_and_is_valid(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                validate_runtime_dir(path)?;
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
        }
    }

    /// `status`/`stop` also inspect fallbacks: the spawning shim may have a different XDG environment.
    fn candidate_runtime_dirs() -> Vec<PathBuf> {
        candidate_runtime_dirs_from(
            std::env::var_os("CODESAGE_DAEMON_RUNTIME_DIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            &std::env::temp_dir(),
        )
    }

    fn candidate_runtime_dirs_from(
        override_dir: Option<OsString>,
        xdg_runtime_dir: Option<OsString>,
        system_tmp: &Path,
    ) -> Vec<PathBuf> {
        // An empty XDG_RUNTIME_DIR must fall through, not create `codesage/` in the checkout.
        let nonempty = |var: Option<OsString>| var.filter(|v| !v.is_empty());
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = nonempty(override_dir) {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(dir) = nonempty(xdg_runtime_dir) {
            dirs.push(PathBuf::from(dir).join("codesage"));
        }
        // UID/USER variables differ across launchers; getuid keeps runtime paths consistent.
        let uid = unsafe { libc::getuid() };
        let tmp = PathBuf::from("/tmp").join(format!("codesage-{uid}"));
        let legacy_tmp = system_tmp.join(format!("codesage-{uid}"));
        if !dirs.contains(&tmp) {
            dirs.push(tmp.clone());
        }
        if legacy_tmp != tmp && !dirs.contains(&legacy_tmp) {
            dirs.push(legacy_tmp);
        }
        dirs
    }

    pub(crate) fn default_runtime_dir() -> PathBuf {
        candidate_runtime_dirs()
            .into_iter()
            .next()
            .expect("candidate_runtime_dirs always yields the /tmp fallback")
    }

    /// Honor an explicit directory; otherwise prefer this binary's key, then a live
    /// older build. Retain a canonical fallback for the "not running" diagnostic.
    fn existing_daemon_paths(runtime_dir: Option<PathBuf>) -> Result<DaemonPaths> {
        let exe = std::env::current_exe().context("resolving current executable")?;
        let explicit_runtime_dir = runtime_dir.is_some();
        let dirs = match runtime_dir {
            Some(dir) => vec![dir],
            None => candidate_runtime_dirs(),
        };

        let mut fallback: Option<DaemonPaths> = None;
        let mut valid_dirs: Vec<&PathBuf> = Vec::new();
        for dir in &dirs {
            match runtime_dir_exists_and_is_valid(dir) {
                Ok(_) => valid_dirs.push(dir),
                Err(err) if explicit_runtime_dir => return Err(err),
                Err(_) => continue,
            }

            let paths = match DaemonPaths::for_exe(dir.clone(), &exe) {
                Ok(paths) => paths,
                Err(err) if explicit_runtime_dir => return Err(err),
                Err(_) => continue,
            };
            if paths.pid.exists() {
                return Ok(paths);
            }
            if fallback.is_none() {
                fallback = Some(paths);
            }
        }

        // A rebuild changes the executable key; still find the previous build for status/stop.
        for dir in valid_dirs {
            if let Some(paths) = scan_live_daemon(dir) {
                return Ok(paths);
            }
        }

        fallback.ok_or_else(|| anyhow::anyhow!("no candidate runtime dir resolved"))
    }

    /// Recover a different build's runtime paths from its `mcp-<key>.pid` filename.
    fn paths_from_pid_file(pid_path: &Path) -> Option<DaemonPaths> {
        let runtime_dir = pid_path.parent()?.to_path_buf();
        let name = pid_path.file_name()?.to_str()?;
        let key = name.strip_prefix("mcp-")?.strip_suffix(".pid")?;
        if key.is_empty() {
            return None;
        }
        Some(DaemonPaths {
            socket: runtime_dir.join(format!("mcp-{key}.sock")),
            lock: runtime_dir.join(format!("mcp-{key}.lock")),
            pid: runtime_dir.join(format!("mcp-{key}.pid")),
            log: runtime_dir.join(format!("mcp-{key}.log")),
            runtime_dir,
        })
    }

    /// Prefer the newest live PID file when several executable keys remain.
    fn scan_live_daemon(dir: &Path) -> Option<DaemonPaths> {
        let mut newest: Option<(SystemTime, DaemonPaths)> = None;
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !(name.starts_with("mcp-") && name.ends_with(".pid")) {
                continue;
            }
            let Some(paths) = paths_from_pid_file(&path) else {
                continue;
            };
            let Some(pid_record) = read_daemon_pid_file(&paths.pid) else {
                continue;
            };
            if !daemon_pid_file_matches(&paths, pid_record) {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(best, _)| mtime > *best) {
                newest = Some((mtime, paths));
            }
        }
        newest.map(|(_, paths)| paths)
    }

    fn daemon_key_for_exe(exe: &Path) -> Result<String> {
        let meta = fs::metadata(exe).with_context(|| format!("reading {}", exe.display()))?;
        let modified = meta
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut hasher = Fnv64::new();
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(exe.to_string_lossy().as_bytes());
        hasher.update(&meta.dev().to_le_bytes());
        hasher.update(&meta.ino().to_le_bytes());
        hasher.update(&meta.len().to_le_bytes());
        hasher.update(&modified.as_secs().to_le_bytes());
        hasher.update(&modified.subsec_nanos().to_le_bytes());
        Ok(format!(
            "{}-{:016x}",
            env!("CARGO_PKG_VERSION"),
            hasher.finish()
        ))
    }

    /// FNV-1a keeps runtime keys deterministic across toolchains, unlike DefaultHasher.
    struct Fnv64 {
        state: u64,
    }

    impl Fnv64 {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;

        fn new() -> Self {
            Self {
                state: Self::OFFSET,
            }
        }

        fn update(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.state ^= u64::from(b);
                self.state = self.state.wrapping_mul(Self::PRIME);
            }
        }

        fn finish(self) -> u64 {
            self.state
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn pathname_capacity() -> usize {
            (1..1024)
                .find(|&len| {
                    std::os::unix::net::SocketAddr::from_pathname("a".repeat(len)).is_err()
                })
                .unwrap()
                - 1
        }

        #[test]
        fn daemon_socket_path_capacity_matches_platform_boundary() {
            let dir = tempfile::tempdir_in("/tmp").unwrap();
            let exe = dir.path().join("exe");
            fs::write(&exe, "test").unwrap();
            let suffix = format!("/mcp-{}.sock", daemon_key_for_exe(&exe).unwrap());
            let padding = pathname_capacity() - dir.path().as_os_str().len() - suffix.len() - 1;
            let runtime = dir.path().join("a".repeat(padding));
            let paths = DaemonPaths::for_exe(runtime.clone(), &exe).unwrap();
            assert_eq!(paths.socket.as_os_str().len(), pathname_capacity());
            fs::create_dir(&runtime).unwrap();
            let _listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
            let overlong = dir.path().join("a".repeat(padding + 1));
            let err = DaemonPaths::for_exe(overlong.clone(), &exe).unwrap_err();
            let diagnostic = format!("{err:#}");
            assert!(diagnostic.contains(&overlong.display().to_string()));
            assert!(diagnostic.contains("CODESAGE_DAEMON_RUNTIME_DIR"));
            assert!(diagnostic.contains("XDG_RUNTIME_DIR"));
        }

        #[test]
        fn daemon_socket_path_capacity_counts_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("exe");
            fs::write(&exe, "test").unwrap();
            let runtime = PathBuf::from("é".repeat(pathname_capacity() / 2));
            assert!(runtime.to_str().unwrap().chars().count() < pathname_capacity());
            assert!(DaemonPaths::for_exe(runtime, &exe).is_err());
        }

        #[tokio::test]
        async fn shim_retains_socket_path_error_before_runtime_creation() {
            let dir = tempfile::tempdir().unwrap();
            let runtime = dir.path().join("a".repeat(pathname_capacity()));
            let err = run_mcp_shim(Some(runtime.clone()), None).await.unwrap_err();
            let diagnostic = format!("{err:#}");
            assert!(diagnostic.contains("socket pathname"), "{diagnostic}");
            assert!(diagnostic.contains("CODESAGE_DAEMON_RUNTIME_DIR"));
            assert!(diagnostic.contains(&runtime.display().to_string()));
            assert!(!runtime.exists());
        }

        #[test]
        fn daemon_paths_are_scoped_by_executable_metadata() {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("codesage-test-bin");
            fs::write(&exe, "first").unwrap();

            let first = DaemonPaths::for_exe(dir.path().join("runtime"), &exe).unwrap();
            fs::write(&exe, "second-version").unwrap();
            let second = DaemonPaths::for_exe(dir.path().join("runtime"), &exe).unwrap();

            assert_ne!(first.socket, second.socket);
            assert_eq!(first.runtime_dir, second.runtime_dir);
        }

        #[test]
        fn default_runtime_dir_treats_empty_env_vars_as_unset() {
            let candidates = candidate_runtime_dirs_from(
                Some(OsString::new()),
                Some(OsString::new()),
                &std::env::temp_dir(),
            );
            let resolved = candidates.first().expect("fallback candidate");
            assert!(
                resolved.is_absolute(),
                "empty env vars must fall through to an absolute /tmp fallback, got {}",
                resolved.display()
            );
            assert!(
                resolved.starts_with("/tmp"),
                "expected /tmp fallback, got {}",
                resolved.display()
            );
        }

        #[test]
        fn fallback_runtime_dir_does_not_depend_on_tmpdir() {
            let scratch = tempfile::tempdir().unwrap();
            let candidates = candidate_runtime_dirs_from(None, None, scratch.path());
            let uid = unsafe { libc::getuid() };
            let expected = PathBuf::from("/tmp").join(format!("codesage-{uid}"));
            assert_eq!(
                candidates.first(),
                Some(&expected),
                "canonical fallback must be stable across different TMPDIR values: {candidates:?}"
            );
        }

        #[test]
        fn prepare_runtime_dir_sets_private_permissions() {
            let dir = tempfile::tempdir().unwrap();
            let runtime_dir = dir.path().join("codesage-runtime");

            prepare_runtime_dir(&runtime_dir).unwrap();

            let mode = fs::metadata(&runtime_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }

        #[test]
        fn prepare_runtime_dir_refuses_symlinked_dir() {
            let dir = tempfile::tempdir().unwrap();
            let real_target = dir.path().join("victim");
            fs::create_dir(&real_target).unwrap();
            let link = dir.path().join("codesage-runtime");
            std::os::unix::fs::symlink(&real_target, &link).unwrap();

            let err = prepare_runtime_dir(&link).unwrap_err();
            assert!(
                err.to_string().contains("symlink"),
                "expected a symlink-refusal error, got: {err}"
            );
        }

        #[test]
        fn existing_daemon_paths_refuses_symlinked_runtime_dir_with_pid_file() {
            let dir = tempfile::tempdir().unwrap();
            let attacker_target = dir.path().join("attacker-runtime");
            fs::create_dir(&attacker_target).unwrap();
            let link = dir.path().join("codesage-runtime");
            std::os::unix::fs::symlink(&attacker_target, &link).unwrap();

            let exe = std::env::current_exe().unwrap();
            let paths = DaemonPaths::for_exe(link.clone(), &exe).unwrap();
            fs::write(&paths.pid, std::process::id().to_string()).unwrap();

            let err = existing_daemon_paths(Some(link)).unwrap_err();
            assert!(
                err.to_string().contains("symlink"),
                "expected a symlink-refusal error, got: {err}"
            );
        }

        #[test]
        fn paths_from_pid_file_round_trips_key() {
            let exe = std::env::current_exe().unwrap();
            let runtime = std::path::Path::new("/tmp/codesage-test-runtime");
            let original = DaemonPaths::for_exe(runtime.to_path_buf(), &exe).unwrap();

            let reconstructed = paths_from_pid_file(&original.pid).expect("parse pid path");
            assert_eq!(reconstructed.socket, original.socket);
            assert_eq!(reconstructed.pid, original.pid);
            assert_eq!(reconstructed.lock, original.lock);
            assert_eq!(reconstructed.log, original.log);
            assert_eq!(reconstructed.runtime_dir, original.runtime_dir);

            assert!(paths_from_pid_file(std::path::Path::new("/tmp/other.pid")).is_none());
            assert!(paths_from_pid_file(std::path::Path::new("/tmp/mcp-.pid")).is_none());
        }

        #[test]
        fn scan_live_daemon_skips_plain_pid_file_for_non_daemon_process() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("mcp-9.9.9-deadbeefdeadbeef.pid");
            fs::write(&pid_file, std::process::id().to_string()).unwrap();

            assert!(scan_live_daemon(dir.path()).is_none());
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn scan_live_daemon_requires_matching_structured_start_time() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("mcp-9.9.9-deadbeefdeadbeef.pid");
            let pid = i32::try_from(std::process::id()).unwrap();
            let start_time = process_start_time_ticks(pid).unwrap();

            fs::write(
                &pid_file,
                format!("pid={pid}\nstart_time_ticks={start_time}\n"),
            )
            .unwrap();
            let found = scan_live_daemon(dir.path()).expect("matching pid record should be live");
            assert_eq!(found.pid, pid_file);

            fs::write(
                &pid_file,
                format!("pid={pid}\nstart_time_ticks={}\n", start_time + 1),
            )
            .unwrap();
            assert!(
                scan_live_daemon(dir.path()).is_none(),
                "reused or mismatched pid must not be treated as a live daemon"
            );
        }

        #[test]
        fn start_lock_is_exclusive_and_released_on_drop() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("daemon.lock");

            let lock = StartLock::try_acquire(&lock_path).unwrap();
            assert!(lock.is_some());
            assert!(StartLock::try_acquire(&lock_path).unwrap().is_none());
            drop(lock);
            assert!(StartLock::try_acquire(&lock_path).unwrap().is_some());
        }

        #[test]
        fn clean_stale_lock_keeps_lock_when_pid_alive() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("daemon.lock");
            fs::write(&lock_path, std::process::id().to_string()).unwrap();
            assert!(matches!(
                clean_stale_lock(&lock_path).unwrap(),
                LockCleanup::HolderAlive
            ));
            assert!(lock_path.exists(), "lock must NOT be removed when alive");
        }

        #[test]
        fn clean_stale_lock_removes_when_pid_dead() {
            // Avoid PID 1: EPERM from kill(1, 0) means alive. Use a likely unallocated PID.
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("daemon.lock");
            fs::write(&lock_path, "2147483646").unwrap();
            assert!(matches!(
                clean_stale_lock(&lock_path).unwrap(),
                LockCleanup::HolderDead
            ));
            assert!(!lock_path.exists(), "dead-PID lock must be removed");
        }

        #[test]
        fn clean_stale_lock_handles_missing_file() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("daemon.lock");
            assert!(matches!(
                clean_stale_lock(&lock_path).unwrap(),
                LockCleanup::HolderDead
            ));
        }

        #[test]
        fn clean_stale_lock_treats_garbled_contents_as_stale() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("daemon.lock");
            fs::write(&lock_path, "not-a-pid\n").unwrap();
            assert!(matches!(
                clean_stale_lock(&lock_path).unwrap(),
                LockCleanup::HolderDead
            ));
            assert!(!lock_path.exists());
        }

        #[test]
        fn rotate_log_rotates_when_oversize() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("daemon.log");
            fs::write(&log, vec![b'x'; (LOG_ROTATE_AT_BYTES + 1) as usize]).unwrap();
            rotate_log_if_large(&log);
            assert!(!log.exists(), "log should have been renamed");
            assert!(
                generation_path(&log, 1).exists(),
                "rotated copy should be at daemon.log.1"
            );
        }

        #[test]
        fn rotate_log_keeps_multiple_generations() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("daemon.log");
            let oversize = vec![b'x'; (LOG_ROTATE_AT_BYTES + 1) as usize];

            fs::write(&log, &oversize).unwrap();
            fs::write(&log, b"GEN1").unwrap();
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            fs::write(&log, b"GEN2-current").unwrap();
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            for n in 1..LOG_KEEP_GENERATIONS {
                assert!(
                    generation_path(&log, n).exists(),
                    "generation .{n} should exist"
                );
            }
            assert!(
                !generation_path(&log, LOG_KEEP_GENERATIONS + 1).exists(),
                "should not retain .{} generation",
                LOG_KEEP_GENERATIONS + 1
            );
        }

        #[test]
        fn rotate_log_noop_when_small() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("daemon.log");
            fs::write(&log, b"tiny\n").unwrap();
            rotate_log_if_large(&log);
            assert!(log.exists(), "small log should not be rotated");
            assert!(!log.with_extension("log.prev").exists());
        }

        #[test]
        fn rotate_log_noop_when_absent() {
            let dir = tempfile::tempdir().unwrap();
            rotate_log_if_large(&dir.path().join("missing.log"));
        }

        #[test]
        fn sibling_daemon_pidfile_filter_matches_only_pid_files() {
            assert!(is_sibling_daemon_pidfile("mcp-0.11.0-2808e07c624d3082.pid"));
            assert!(is_sibling_daemon_pidfile("mcp-0.9.0-6017ca4d7764e95e.pid"));
            assert!(!is_sibling_daemon_pidfile(
                "mcp-0.11.0-2808e07c624d3082.sock"
            ));
            assert!(!is_sibling_daemon_pidfile(
                "mcp-0.11.0-2808e07c624d3082.log"
            ));
            assert!(!is_sibling_daemon_pidfile(
                "mcp-0.11.0-2808e07c624d3082.log.1"
            ));
            assert!(!is_sibling_daemon_pidfile("watch.disabled"));
        }

        #[test]
        fn cleanup_sibling_runtime_files_removes_socket_and_lock_despite_dotted_version() {
            let dir = tempfile::tempdir().unwrap();
            let stem = "mcp-0.11.0-2808e07c624d3082";
            let pid = dir.path().join(format!("{stem}.pid"));
            let sock = dir.path().join(format!("{stem}.sock"));
            let lock = dir.path().join(format!("{stem}.lock"));
            let log = dir.path().join(format!("{stem}.log"));
            for p in [&pid, &sock, &lock, &log] {
                fs::write(p, "x").unwrap();
            }

            cleanup_sibling_runtime_files(&pid);

            assert!(!pid.exists(), "pid file should be removed");
            assert!(!sock.exists(), "socket file should be removed");
            assert!(!lock.exists(), "lock file should be removed");
            assert!(log.exists(), "log file should be left for diagnostics");
        }

        #[tokio::test]
        async fn drain_client_tasks_waits_for_in_flight_connections() {
            let mut clients = JoinSet::new();
            let done = Arc::new(AtomicUsize::new(0));
            for _ in 0..3 {
                let done = done.clone();
                clients.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    done.fetch_add(1, Ordering::SeqCst);
                });
            }

            let still_in_flight = drain_client_tasks(&mut clients, Duration::from_secs(5)).await;

            assert_eq!(still_in_flight, 0, "all connections finish within bound");
            assert_eq!(
                done.load(Ordering::SeqCst),
                3,
                "shutdown must wait for every in-flight connection to complete"
            );
        }

        #[tokio::test]
        async fn drain_client_tasks_bounds_the_wait_on_stuck_connections() {
            let mut clients = JoinSet::new();
            clients.spawn(async {
                std::future::pending::<()>().await;
            });

            let started = Instant::now();
            let still_in_flight =
                drain_client_tasks(&mut clients, Duration::from_millis(100)).await;

            assert_eq!(
                still_in_flight, 1,
                "the stuck connection must be reported as still in flight"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "drain must not hang past its bound"
            );
        }

        #[tokio::test]
        async fn drain_client_tasks_is_noop_when_no_connections() {
            let mut clients: JoinSet<()> = JoinSet::new();
            assert_eq!(
                drain_client_tasks(&mut clients, Duration::from_secs(5)).await,
                0
            );
        }

        #[tokio::test]
        async fn drain_reports_wedged_blocking_work_without_joining_it() {
            // Aborting a connection cannot interrupt an already-running blocking task.
            let mut clients = JoinSet::new();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let entered = Arc::new(AtomicUsize::new(0));
            let entered_task = entered.clone();
            clients.spawn(async move {
                let _ = tokio::task::spawn_blocking(move || {
                    entered_task.fetch_add(1, Ordering::SeqCst);
                    let _ = release_rx.recv();
                })
                .await;
            });
            // Ensure abort races running work, not a queued task.
            while entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            let started = Instant::now();
            let still_in_flight =
                drain_client_tasks(&mut clients, Duration::from_millis(100)).await;

            assert_eq!(
                still_in_flight, 1,
                "the wedged connection must be reported as abandoned"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "drain must not wait for the wedged blocking body"
            );
            // Test runtime drop must be able to join the blocking pool.
            release_tx.send(()).unwrap();
        }

        fn test_daemon_paths(dir: &Path) -> DaemonPaths {
            DaemonPaths {
                socket: dir.join("mcp-test.sock"),
                lock: dir.join("mcp-test.lock"),
                pid: dir.join("mcp-test.pid"),
                log: dir.join("mcp-test.log"),
                runtime_dir: dir.to_path_buf(),
            }
        }

        fn reap(mut child: Child) {
            let _ = child.kill();
            let _ = child.wait();
        }

        #[test]
        fn spawner_wait_decision_table() {
            use SpawnerWaitDecision::*;
            let cap = Duration::from_secs(15);
            assert_eq!(
                spawner_wait_decision(true, Duration::from_secs(5), cap),
                KeepWaiting
            );
            assert_eq!(spawner_wait_decision(true, cap, cap), CapExceeded);
            assert_eq!(
                spawner_wait_decision(true, Duration::from_secs(20), cap),
                CapExceeded
            );
            assert_eq!(
                spawner_wait_decision(false, Duration::ZERO, cap),
                ChildExited
            );
            assert_eq!(
                spawner_wait_decision(false, Duration::from_secs(20), cap),
                ChildExited
            );
        }

        #[tokio::test]
        async fn spawner_keeps_waiting_while_child_alive_and_binds_late() {
            // Bind after the first wait round while the child remains alive.
            let dir = tempfile::tempdir().unwrap();
            let paths = test_daemon_paths(dir.path());
            let mut child = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();

            let socket = paths.socket.clone();
            let binder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                std::os::unix::net::UnixListener::bind(&socket).unwrap()
            });

            let res = wait_for_spawned_daemon(
                &mut child,
                &paths,
                Duration::from_millis(50),
                Duration::from_secs(10),
            )
            .await;
            let _listener = binder.join().unwrap();
            reap(child);
            assert!(
                res.is_ok(),
                "late bind with a live child must succeed: {:?}",
                res.err()
            );
        }

        #[tokio::test]
        async fn spawner_fails_immediately_when_child_died() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_daemon_paths(dir.path());
            let mut child = Command::new("true").stdin(Stdio::null()).spawn().unwrap();
            child.wait().unwrap();

            let started = Instant::now();
            let err = wait_for_spawned_daemon(
                &mut child,
                &paths,
                Duration::from_millis(100),
                Duration::from_secs(30),
            )
            .await
            .unwrap_err();

            assert!(
                format!("{err:#}").contains("exited during startup"),
                "want child-exit failure, got: {err:#}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a dead child must fail after one round, not the full cap"
            );
        }

        #[tokio::test]
        async fn spawner_gives_up_at_cap_when_child_wedged() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_daemon_paths(dir.path());
            let mut child = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();

            let started = Instant::now();
            let err = wait_for_spawned_daemon(
                &mut child,
                &paths,
                Duration::from_millis(50),
                Duration::from_millis(150),
            )
            .await
            .unwrap_err();
            reap(child);

            assert!(
                format!("{err:#}").contains("still alive but not ready"),
                "want cap-exceeded failure, got: {err:#}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the cap must bound the total wait"
            );
        }

        #[test]
        fn sibling_reap_dead_or_recycled_pid_cleans_files() {
            assert_eq!(
                sibling_reap_action(false, Some(100), Some(50)),
                SiblingReapAction::CleanupFiles
            );
            assert_eq!(
                sibling_reap_action(false, None, None),
                SiblingReapAction::CleanupFiles
            );
        }

        #[test]
        fn sibling_reap_requires_both_start_times() {
            assert_eq!(
                sibling_reap_action(true, None, None),
                SiblingReapAction::Leave
            );
            assert_eq!(
                sibling_reap_action(true, Some(100), None),
                SiblingReapAction::Leave
            );
            assert_eq!(
                sibling_reap_action(true, None, Some(50)),
                SiblingReapAction::Leave
            );
        }

        #[test]
        fn sibling_reap_only_strictly_older_daemons() {
            assert_eq!(
                sibling_reap_action(true, Some(100), Some(50)),
                SiblingReapAction::Reap
            );
            assert_eq!(
                sibling_reap_action(true, Some(100), Some(100)),
                SiblingReapAction::Leave
            );
            assert_eq!(
                sibling_reap_action(true, Some(50), Some(100)),
                SiblingReapAction::Leave
            );
        }

        #[test]
        fn proxy_copy_error_exits_nonzero_not_clean_eof() {
            assert_eq!(proxy_exit_code::<anyhow::Error>(&Ok(())), 0);
            assert_eq!(
                proxy_exit_code(&Err(anyhow::anyhow!("connection reset"))),
                1,
                "a dead daemon must not look like a clean EOF"
            );
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::{
    default_runtime_dir, run_daemon, run_daemon_status, run_daemon_stop, run_mcp_shim,
    running_daemon_socket,
};

/// No daemon exists off Unix, so no CLI command can borrow its session.
#[cfg(not(unix))]
pub(crate) fn running_daemon_socket() -> Option<PathBuf> {
    None
}

#[cfg(not(unix))]
pub(crate) async fn run_mcp_shim(
    runtime_dir: Option<PathBuf>,
    default_project: Option<String>,
) -> Result<()> {
    if runtime_dir.is_some() {
        bail!("--runtime-dir is Unix-only; codesage MCP daemon is not supported on this platform");
    }
    crate::mcp::run_mcp_server(default_project).await
}

#[cfg(not(unix))]
pub(crate) async fn run_daemon(_runtime_dir: Option<PathBuf>) -> Result<()> {
    bail!("codesage MCP daemon requires Unix domain sockets")
}

#[cfg(not(unix))]
pub(crate) async fn run_daemon_status(_runtime_dir: Option<PathBuf>) -> Result<()> {
    bail!("codesage MCP daemon requires Unix domain sockets")
}

#[cfg(not(unix))]
pub(crate) async fn run_daemon_stop(_runtime_dir: Option<PathBuf>) -> Result<()> {
    bail!("codesage MCP daemon requires Unix domain sockets")
}

/// Where per-user runtime state lives when there is no daemon to co-locate with.
#[cfg(not(unix))]
pub(crate) fn default_runtime_dir() -> PathBuf {
    std::env::temp_dir().join("codesage")
}
