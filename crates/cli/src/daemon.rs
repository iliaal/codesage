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

    /// Total cap on waiting for a daemon this shim spawned itself. One
    /// START_TIMEOUT covers a warm start, but a cold model load on a slow or
    /// contended disk can exceed it — and bailing while the child is still
    /// alive reports a spurious failure an instant before the daemon binds,
    /// leaving it running orphaned. While the spawned child lives, the wait
    /// loops in START_TIMEOUT rounds up to this cap so a truly wedged child
    /// still fails. Matches the waiter branch's worst case (3 attempts x
    /// START_TIMEOUT), so both paths give up on the same horizon.
    const SPAWNER_WAIT_CAP: Duration = START_TIMEOUT.saturating_mul(3);

    /// Bound on how long shutdown waits for in-flight client connections to
    /// finish after the accept loop stops. Long enough for a typical tool
    /// call to complete its response, short enough that `daemon stop`'s 10s
    /// SIGTERM window is never exceeded even when a parked client holds its
    /// connection open for the whole drain.
    const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

    /// Lock an activity-clock mutex, recovering from poisoning. The clock is
    /// a plain timestamp: a writer that panicked mid-store leaves the old
    /// value intact, so `into_inner` recovery (rather than an `.unwrap()`
    /// that crash-loops the daemon across the accept loop and every tick) is
    /// the correct call.
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

    /// Default idle backstop: shut the daemon down after this long with zero
    /// connected clients. The daemon is meant to be a warm pool shared across
    /// sessions, so this is generous — a 30-minute gap between queries is rare
    /// inside an active session, and the only cost of reaping is one model
    /// reload on the next query. Without it the daemon outlives every agent and
    /// pins the embedder/reranker in memory indefinitely. Override with
    /// `CODESAGE_DAEMON_IDLE_TIMEOUT_SECS`; `0` disables idle exit entirely.
    const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);

    /// Resolve the idle backstop from the environment. `Duration::ZERO` means
    /// "never reap" (the pre-backstop behavior). An unparseable value falls back
    /// to the default rather than silently disabling the backstop.
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

    /// Default per-model idle eviction timeout: drop a pooled embedder /
    /// reranker that has not been used for this long, freeing its ORT
    /// `Session` (GPU VRAM + host buffers). Generous enough that genuinely
    /// active sessions never reload, short enough that an idle warm daemon
    /// gives memory back. Override with `CODESAGE_MODEL_IDLE_SECS`; `0`
    /// disables per-model eviction (the daemon-level backstop still applies).
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

    /// Ask the allocator to return freed pages to the OS after a model
    /// eviction. glibc retains freed heap by default, so dropping an ORT
    /// `Session` frees the VRAM but leaves host RSS at its high-water mark
    /// until this runs (measured: recovers ~170 MB after a Jina-base drop).
    /// No-op off glibc.
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
            Ok(Self {
                socket: runtime_dir.join(format!("mcp-{key}.sock")),
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

    /// Socket of a daemon spawned from THIS binary that is accepting
    /// connections right now, or `None`. Never spawns one: a CLI command that
    /// finds no daemon embeds privately rather than paying a daemon start it
    /// would not wait for. The key derives from the executable's identity, so
    /// a daemon left over from an older install is not a match.
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
        // Probe the socket: a stale pid + dead socket is rare but possible
        // if the daemon was SIGKILL'd before the cleanup branch ran.
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

        // M3: bind() honors the caller's umask, leaving a brief window
        // where the new socket file could carry world or group bits before
        // set_permissions tightens it to 0o600. Tighten the umask for the
        // bind, then restore — that way the socket is born with restricted
        // permissions and the explicit set_permissions below is just
        // defense-in-depth.
        let prev_umask = unsafe { libc::umask(0o077) };
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("binding {}", paths.socket.display()));
        unsafe { libc::umask(prev_umask) };
        let listener = listener?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", paths.socket.display()))?;
        write_daemon_pid(&paths.pid)?;

        // Now that our own socket is bound and pid file written, reap any
        // daemon left over from a previous build/version sharing this runtime
        // dir. The version-keyed socket name means we just spawned fresh
        // rather than attaching to the old one; without this sweep the old
        // daemon pins a second copy of the embedder + reranker in memory
        // until its 30-minute idle backstop fires.
        reap_stale_version_daemons(&paths);

        tracing::info!(socket = %paths.socket.display(), "codesage MCP daemon listening");
        let state = Arc::new(CodeSageServerState::new());
        let our_uid = unsafe { libc::getuid() };

        // Per-model idle eviction. The whole-daemon backstop below can't fire
        // while a client connection is parked open (the common interactive
        // case), so a warm daemon otherwise pins its embedder + reranker —
        // host context + GPU VRAM — for the whole session. This drops the
        // individual models that have sat unused past the timeout, reclaiming
        // their VRAM (and, via malloc_trim, some host pages) while keeping the
        // daemon and its connections alive; the next query reloads them cold.
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

        // M6: install a shutdown signal so SIGTERM / SIGINT exit the
        // accept loop cleanly and remove socket + pid files. Without
        // this, the daemon dies abruptly leaving stale runtime files
        // and in-flight clients see broken pipes.
        let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

        // Idle backstop: track the number of connected clients and the last
        // time that count changed. When it sits at zero past the idle timeout,
        // the daemon reaps itself rather than leaking embedder/reranker memory
        // after every agent has disconnected (the failure mode three peer tools
        // — codegraph, GitNexus, repowise — all hardened independently).
        let idle_timeout = daemon_idle_timeout();
        let active = Arc::new(AtomicUsize::new(0));
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        // Last byte read from any client, stamped by every `ActivityStream`.
        // The backstop below deliberately does not reap on this — the stdio
        // shim is a raw proxy, so killing the daemon under a live-but-quiet
        // session kills that session's MCP server. It exists so the log can
        // distinguish "busy" from "held open by a parked shim", which is
        // otherwise indistinguishable from outside the process.
        let daemon_last_byte = Arc::new(Mutex::new(Instant::now()));
        let mut silence_reported = false;
        // Poll often enough to honor short idle timeouts in tests, but never
        // busier than once a minute for the production default.
        let idle_poll = if idle_timeout.is_zero() {
            Duration::from_secs(3600)
        } else {
            idle_timeout
                .min(Duration::from_secs(60))
                .max(Duration::from_secs(1))
        };
        let mut idle_tick = tokio::time::interval(idle_poll);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track connection tasks so shutdown can wait for in-flight requests
        // instead of aborting them mid-response when the runtime drops.
        let mut clients: JoinSet<()> = JoinSet::new();

        let shutdown_reason = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    // A transient accept() error (EMFILE/ENFILE under fd
                    // pressure, ECONNABORTED on a racing client hangup) must
                    // not tear down the daemon and drop every in-flight
                    // session. Log, pause briefly so we don't spin on a
                    // persistent error, and keep serving.
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

                    // M4: refuse connections whose peer UID doesn't match
                    // ours. The 0o700 runtime dir + 0o600 socket already
                    // gate this at the FS layer, but a misconfigured
                    // $CODESAGE_DAEMON_RUNTIME_DIR could open a wider
                    // path; SO_PEERCRED is cheap defense-in-depth.
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
                        // With no client left there is nobody to keep an
                        // index fresh for: stop every live watcher now rather
                        // than letting it re-embed saved files for the rest of
                        // its idle window. The next semantic query respawns it
                        // — after this stop has joined the old thread, which is
                        // why the wait runs off the async runtime and a start
                        // request meanwhile waits on the slot. The count is
                        // re-read under the registry lock right before the
                        // signal: a client that connected since this
                        // disconnect aborts the stop.
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
                // Reap finished connection tasks so the tracked set doesn't
                // grow for the daemon's lifetime. When the set is empty the
                // pattern mismatch disables this arm for the iteration.
                Some(_) = clients.join_next() => {}
                _ = idle_tick.tick() => {
                    if !idle_timeout.is_zero() {
                        let connected = active.load(Ordering::SeqCst);
                        if connected == 0 {
                            if lock_clock(&last_activity).elapsed() >= idle_timeout {
                                break "idle";
                            }
                        } else {
                            // Connections are open, so the backstop cannot
                            // fire. Report it once the silence passes the
                            // backstop window: past that point the daemon is
                            // holding its pools for clients that have said
                            // nothing, and only the per-connection ceiling
                            // (CODESAGE_CLIENT_IDLE_MAX_SECS, 4h default) will
                            // eventually release them.
                            let silent = lock_clock(&daemon_last_byte).elapsed();
                            // Latch so a parked session logs once per silent
                            // window rather than every tick for hours. The end
                            // of the window is already observable: the
                            // per-connection ceiling logs when it drops the
                            // client. Re-armed below as soon as bytes arrive.
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

        // Signal any per-project live watchers to drain and exit before we
        // tear down, waiting a bounded time for them. They share this
        // process, so leaving them running past daemon exit would orphan
        // inotify threads; one still draining at the deadline is reaped by
        // the process exit as before.
        let still_stopping = state.shutdown_all_watchers(SHUTDOWN_DRAIN);
        if still_stopping > 0 {
            tracing::warn!(
                still_stopping,
                "watchers still draining after {:?}; process exit will reap them",
                SHUTDOWN_DRAIN
            );
        }

        // Let in-flight client connections finish (bounded) before removing
        // the runtime files; returning immediately would drop the runtime and
        // abort them mid-request, so the client sees a truncated stream. The
        // listener stays bound while draining: a late shim parks in the
        // accept backlog instead of racing a replacement daemon for the
        // socket path.
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

        // Exit without dropping the tokio Runtime: its drop joins the
        // blocking pool, and abort_all cannot interrupt a spawn_blocking
        // tool body (ONNX inference, SQLite) that is already running. A
        // wedged one would hold the process past `daemon stop`'s 10s
        // SIGTERM window. Cleanup above is complete; abandon the blocking
        // work and leave.
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                "exiting with aborted connections whose blocking tool tasks may still be running; abandoning them"
            );
        }
        std::process::exit(0)
    }

    /// Default per-connection idle ceiling. A per-request timeout would
    /// require introspecting the rmcp service's tool dispatch, which is
    /// private to the crate, so we observe activity one layer down — at the
    /// transport (see [`ActivityStream`]) — and drop a connection that has
    /// gone this long without the client sending a byte. Measured from last
    /// activity, NOT connection start: an active multi-hour sweep keeps
    /// resetting the clock and is never guillotined mid-session, while a hung
    /// tool call (which produces no further client bytes) or an agent that
    /// wandered off without disconnecting is still reaped so the agent gets an
    /// error instead of an indefinite hang. Override with
    /// `CODESAGE_CLIENT_IDLE_MAX_SECS`; `0` disables the ceiling entirely.
    const DEFAULT_CLIENT_IDLE_MAX: Duration = Duration::from_secs(4 * 3600);

    /// Resolve the per-connection idle ceiling from the environment.
    /// `Duration::ZERO` disables it. An unparseable value falls back to the
    /// default rather than silently disabling the ceiling.
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

    /// Transparent wrapper around the client [`UnixStream`] that stamps
    /// `last_activity` on every non-empty read. `serve_client` reads this to
    /// measure the idle ceiling from the client's last request rather than
    /// from connection start. Reads are the right signal: a healthy session
    /// reads (requests/pings) continuously, whereas a hung tool call leaves
    /// the client blocked waiting for a response it never sent more bytes for,
    /// so the clock advances and the connection is reaped.
    struct ActivityStream {
        inner: UnixStream,
        last_activity: Arc<Mutex<Instant>>,
        /// Shared across every connection so the accept loop can answer "has
        /// *any* client said anything recently?". The per-connection
        /// `last_activity` above drives that connection's own idle ceiling;
        /// this one exists purely so an operator can tell a busy daemon from
        /// one held open by a parked shim.
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

        // Bound the handshake. `serve` resolves once the client sends
        // `initialize`; a peer that connects and then says nothing would
        // otherwise park this task forever, and because the idle ceiling below
        // only starts after this await, that connection holds the daemon's
        // active-client count above zero permanently — blocking the whole-daemon
        // backstop for good, not merely delaying it. A real shim cannot do this
        // (it exits when its stdin closes), so this bounds a misbehaving or
        // half-open peer, not normal traffic. Reuses the idle ceiling: a client
        // that has not spoken within it is idle by definition, and `0` keeps
        // that knob's "disabled" meaning.
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

        // Poll often enough to notice idleness within a minute of the
        // ceiling, but never busier than once a minute.
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

        // Bounded retry loop so a lock holder that dies mid-startup
        // doesn't permanently strand all subsequent shims. Each pass:
        // try to claim the start lock; if we get it, spawn; if not,
        // wait for the socket; if the wait times out, check whether
        // the lock holder is still alive, and only then take over.
        for attempt in 0..3 {
            match StartLock::try_acquire(&paths.lock)? {
                Some(_lock) => {
                    if attempt > 0 {
                        // OP3: surfacing recovery activity so a regression
                        // that breaks daemon startup across the user base
                        // shows up as warn-level log volume rather than
                        // silent retries.
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
                            // Two distinct failure modes:
                            //   (a) lock holder is still working — wait
                            //       another round before giving up.
                            //   (b) lock holder died — adopt the lock
                            //       and become the spawner ourselves.
                            // The old code blindly removed the lock file
                            // and raced with peer shims also in recovery,
                            // which let two starters land at once and
                            // produced a misleading "another starter
                            // still holds" error.
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
                                    // else: loop, wait again
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

    /// Inspect the start-lock file's recorded PID. If the lock holder is
    /// still alive, leave the lock alone and report `HolderAlive`. If the
    /// holder is dead (or the lock file vanished, or the contents are
    /// garbled), remove the lock and report `HolderDead` so the caller
    /// can attempt to acquire it.
    fn clean_stale_lock(lock_path: &Path) -> Result<LockCleanup> {
        let contents = match fs::read_to_string(lock_path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Lock file already gone (holder's Drop ran between our
                // try_acquire and now). Caller should re-try acquisition.
                return Ok(LockCleanup::HolderDead);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", lock_path.display())),
        };
        let pid: i32 = match contents.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => {
                // Garbled file — treat as stale.
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

    /// `kill(pid, 0)` is the standard POSIX "does this process exist?"
    /// probe — it sends no signal and just runs the permission/existence
    /// checks. ESRCH means dead; EPERM means alive but we're not allowed
    /// to signal it (still counts as alive).
    fn pid_alive(pid: i32) -> bool {
        // SAFETY: kill is async-signal-safe and side-effect-free for sig=0.
        let r = unsafe { libc::kill(pid, 0) };
        if r == 0 {
            return true;
        }
        matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
    }

    /// Cap the daemon log at this size on shim-driven spawn. The shim is
    /// the only spawn path; if the existing log is larger, rotate the
    /// chain (.1 -> .2, .log -> .1) so we keep up to LOG_KEEP_GENERATIONS
    /// generations of context but the active log doesn't grow without
    /// bound across many daemon restarts. Multi-generation matters when
    /// a daemon crashes repeatedly within one rotation cycle — keeping
    /// only one .prev would lose earlier failures.
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
        // Cap glibc's per-thread arena count. ORT + tokio are multithreaded,
        // so the default (8×ncpu arenas) retains a lot of freed model-load
        // scratch the daemon never gives back; 2 arenas trims the steady-state
        // RSS (~60 MB measured) at negligible alloc-contention cost for this
        // workload. MALLOC_ARENA_MAX is read at the child's first malloc, so it
        // must be set here on the spawn, not from within the daemon. Respect an
        // explicit operator override.
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

    /// Wait for the daemon `child` (spawned by this shim) to bind its
    /// socket. A single [`wait_for_socket`] round is too short for a cold
    /// model load, so loop it while the child process is still alive,
    /// bounded by `cap` in total. A round can also end early — before
    /// `round` elapses — when a stale same-key pid file trips
    /// `wait_for_socket`'s liveness bail; the child overwrites that file
    /// once it binds, so looping on child liveness rides out that window
    /// too. `try_wait` also reaps the child on the exited path, so no
    /// zombie lingers for the shim's lifetime.
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
        // Best-effort rotation; if any rename fails the daemon will just
        // continue appending to the existing log. The runtime is the
        // user's home, so a transient EACCES isn't a reason to fail
        // daemon startup.
        //
        // Walk highest -> lowest so each rename has a clean target:
        //   .log.N-1 -> .log.N (dropping the oldest if it exists)
        //   ...
        //   .log.1   -> .log.2
        //   .log     -> .log.1
        // Result: at most LOG_KEEP_GENERATIONS files retained.
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

    /// Poll for the socket to appear. Returns early with a `daemon exited`
    /// error if the PID recorded in `paths.pid` is no longer alive — saves
    /// the caller the full timeout wait when the daemon crashed during
    /// init (model load failure, port conflict, etc.). The log path is
    /// always included in the error so the user knows where to look.
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
            // Liveness check at most every 100ms — cheap (one kill(pid, 0)
            // syscall) but no point doing it every 25ms retry.
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

    /// SIGTERM codesage daemons in this runtime dir whose key differs from
    /// ours — i.e. left over from a previous build or version. The daemon key
    /// folds in the binary's version + on-disk identity, so a rebuilt or
    /// upgraded binary boots a fresh daemon under a new socket name instead of
    /// attaching to the incompatible old one. The old daemon then keeps its
    /// embedder + reranker resident until the idle backstop fires (default 30
    /// min) — a full second copy of the model memory for the whole overlap.
    ///
    /// Safety: we only signal processes that are (a) a live, start-time- or
    /// cmdline- or socket-validated codesage daemon in *this* runtime dir,
    /// validated against the SIBLING's own runtime files (our own socket is
    /// already bound by the time this runs, so probing it would vacuously
    /// succeed), and (b) started strictly before us. The strictly-before
    /// guard means two daemons racing to start can never kill each other;
    /// when either start time is unavailable (non-Linux) we skip rather than
    /// kill — reaping is an optimization, SIGTERM'ing a recycled pid is not
    /// recoverable. SIGTERM is graceful — the target drains in-flight clients
    /// and removes its own socket/pid files on exit.
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
                    // Dead or recycled pid: clear its leftover runtime files so
                    // `status` / `stop` don't trip over them, then move on.
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

    /// Pure reap decision for one sibling pid record. `validated` means the
    /// record survived [`daemon_pid_file_matches`] against the sibling's own
    /// paths (start-time match, cmdline match, or its socket accepting
    /// connections). Reaping requires proof the sibling started strictly
    /// before us; without comparable start times on both sides (non-Linux,
    /// where `process_start_time_ticks` is `None`) we leave it alone — a
    /// wrongly-killed innocent process is not recoverable, an unreaped
    /// stale daemon merely holds memory until its idle backstop fires.
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
            // With a default project, rewrite tools/call messages that omit
            // `project` before forwarding; otherwise raw-copy for zero
            // overhead (the Claude-plugin path always passes project).
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

        // select!, not try_join!: try_join! waits for BOTH futures, so
        // if the daemon closed its write half but the MCP client kept
        // stdin open, the stdin pump would block on read forever and
        // the shim would hang with no server behind it.
        //
        // Two reasons we exit the process instead of returning Ok:
        //   1. tokio::io::stdin() is backed by a blocking-pool thread
        //      stuck in read(2). Dropping the future does not cancel
        //      the syscall — the Runtime::drop in cmd_mcp waits for
        //      it forever, and the natural process::exit in main is
        //      never reached.
        //   2. Stdio MCP semantics: client closes → server exits, and
        //      vice versa. There's no reconnect protocol; once the
        //      daemon goes, the shim has no useful work left.
        // Both directions therefore terminate the process directly.
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

        // The /tmp fallback lives under a world-writable, sticky-bit dir, so a
        // co-located local user can pre-stage `codesage-<uid>` as a symlink (or
        // a dir they own) before our daemon does. Refuse anything that isn't a
        // real directory we own before creating sockets or trusting pid files.
        // lstat (symlink_metadata) does not follow the link, so a symlinked
        // runtime dir is rejected here.
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

    /// Every runtime dir a daemon could plausibly live in, in resolution
    /// order. The first entry is the canonical choice ([`default_runtime_dir`]);
    /// `status`/`stop` scan the whole list so they find a daemon that a
    /// differently-environment'd process started. Concretely: a Claude Code
    /// shim spawned without `XDG_RUNTIME_DIR` falls through to `/tmp`, while an
    /// interactive `codesage daemon status` has `XDG_RUNTIME_DIR` set and would
    /// otherwise look only under `/run/user/$UID/codesage`, missing the daemon
    /// and falsely reporting "not running" (and `stop` would fail to stop it).
    fn candidate_runtime_dirs() -> Vec<PathBuf> {
        candidate_runtime_dirs_from(
            std::env::var_os("CODESAGE_DAEMON_RUNTIME_DIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            &std::env::temp_dir(),
        )
    }

    /// Env-free core of [`candidate_runtime_dirs`], parameterized so tests
    /// can exercise the resolution logic without mutating process-global
    /// environment (a `set_var` here races every concurrent test that calls
    /// `tempfile::tempdir()` or reads the same vars).
    fn candidate_runtime_dirs_from(
        override_dir: Option<OsString>,
        xdg_runtime_dir: Option<OsString>,
        system_tmp: &Path,
    ) -> Vec<PathBuf> {
        // Treat set-but-empty env vars as unset. Documented workaround for the
        // WSL2 `/run/user/$UID` trap is to inject `XDG_RUNTIME_DIR=""` in the
        // client MCP env so the shim falls through to `/tmp`; without this
        // guard `PathBuf::from("").join("codesage")` collapses to a relative
        // `codesage/` that gets created next to whatever cwd the shim was
        // spawned in.
        let nonempty = |var: Option<OsString>| var.filter(|v| !v.is_empty());
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = nonempty(override_dir) {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(dir) = nonempty(xdg_runtime_dir) {
            dirs.push(PathBuf::from(dir).join("codesage"));
        }
        // Suffix the /tmp fallback with the real numeric UID, not the UID/USER
        // env vars. bash doesn't export UID, and a Claude Code shim's env may
        // carry UID=1000 while an interactive shell falls back to USER=ilia —
        // so env-derived suffixes disagree across processes and put the daemon
        // and a later `status`/`stop` in different /tmp dirs. getuid() is
        // stable regardless of environment (and matches the SO_PEERCRED check).
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

    /// Resolve the [`DaemonPaths`] of an existing daemon for `status`/`stop`.
    /// With an explicit `runtime_dir` (e.g. `--runtime-dir` in tests), use only
    /// that. Otherwise scan the candidate dirs and return the first whose pid
    /// file exists, falling back to the canonical dir so the caller still
    /// prints a coherent "not running" answer when no daemon is found anywhere.
    fn existing_daemon_paths(runtime_dir: Option<PathBuf>) -> Result<DaemonPaths> {
        let exe = std::env::current_exe().context("resolving current executable")?;
        let explicit_runtime_dir = runtime_dir.is_some();
        let dirs = match runtime_dir {
            Some(dir) => vec![dir],
            None => candidate_runtime_dirs(),
        };

        // First pass: the current binary's own key. This is the common case
        // (same binary that spawned the daemon) and is preferred so we never
        // pick a different daemon when ours is the one running.
        let mut fallback: Option<DaemonPaths> = None;
        let mut valid_dirs: Vec<&PathBuf> = Vec::new();
        for dir in &dirs {
            match runtime_dir_exists_and_is_valid(dir) {
                Ok(_) => valid_dirs.push(dir),
                Err(err) if explicit_runtime_dir => return Err(err),
                Err(_) => continue,
            }

            let paths = DaemonPaths::for_exe(dir.clone(), &exe)?;
            if paths.pid.exists() {
                return Ok(paths);
            }
            if fallback.is_none() {
                fallback = Some(paths);
            }
        }

        // Second pass: any *live* `mcp-<key>.pid` regardless of key. The key is
        // derived from the exe's dev/ino/len/mtime, so a rebuilt binary has a
        // new key and the first pass misses a daemon a prior build left running
        // (it would keep serving stale code while `stop` reported "not
        // running"). Scan for a foreign-key daemon whose pid is still alive.
        for dir in valid_dirs {
            if let Some(paths) = scan_live_daemon(dir) {
                return Ok(paths);
            }
        }

        fallback.ok_or_else(|| anyhow::anyhow!("no candidate runtime dir resolved"))
    }

    /// Reconstruct [`DaemonPaths`] from a `<dir>/mcp-<key>.pid` path so
    /// `status`/`stop` can address a daemon whose key differs from the current
    /// binary's (e.g. after a rebuild). Returns `None` for names that don't fit
    /// the `mcp-<key>.pid` shape.
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

    /// Return the most-recently-started live daemon in `dir`, by scanning every
    /// `mcp-*.pid` and keeping the newest whose pid is still alive. Newest wins
    /// so that after a rebuild we address the freshest daemon when several keys
    /// linger.
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

    /// FNV-1a 64-bit hash. Deterministic across Rust toolchain versions
    /// and architectures — unlike `std::collections::hash_map::DefaultHasher`
    /// whose output is documented to change. The daemon key only needs
    /// to be stable within one binary's lifetime today, but keying the
    /// runtime layout to an unstable hash is needless fragility.
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
            // Regression: the WSL2 workaround `XDG_RUNTIME_DIR=""` (and the
            // analogous override on CODESAGE_DAEMON_RUNTIME_DIR) used to
            // produce a relative `codesage/` runtime dir because var_os
            // returns Some("") for set-but-empty vars. The shim then
            // mkdir'd `codesage/` next to whatever cwd it spawned in.
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
            // SS-001: on a shared /tmp a co-located user can pre-stage the
            // runtime dir as a symlink; prepare_runtime_dir must refuse it
            // rather than follow the link and chmod the victim's target.
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
            // `status`/`stop` reconstruct a daemon's paths from a
            // foreign-key pid file (a daemon left by a different build).
            let exe = std::env::current_exe().unwrap();
            let runtime = std::path::Path::new("/tmp/codesage-test-runtime");
            let original = DaemonPaths::for_exe(runtime.to_path_buf(), &exe).unwrap();

            let reconstructed = paths_from_pid_file(&original.pid).expect("parse pid path");
            assert_eq!(reconstructed.socket, original.socket);
            assert_eq!(reconstructed.pid, original.pid);
            assert_eq!(reconstructed.lock, original.lock);
            assert_eq!(reconstructed.log, original.log);
            assert_eq!(reconstructed.runtime_dir, original.runtime_dir);

            // Non-matching names are rejected.
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
            // Our own PID is by definition alive: must report HolderAlive
            // and leave the lock file untouched. Pre-M2 the recovery
            // branch blindly removed the lock in this state, which let
            // peer shims race to spawn duplicate daemons.
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
            // PID 1 owned by init in PID 1 namespace; on Linux it's
            // permission-denied to kill(1,0) for non-root which returns
            // EPERM = "alive". Use a PID we own that's gone. The safest
            // portable choice is a high PID that's almost certainly
            // never been allocated.
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
            // Lock file vanished between try_acquire and clean_stale_lock
            // (holder's Drop ran). Caller should be told to retry.
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
            // Write LOG_ROTATE_AT_BYTES + 1 bytes so the threshold trips.
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

            // First rotation: .log -> .log.1
            fs::write(&log, &oversize).unwrap();
            fs::write(&log, b"GEN1").unwrap();
            // Force rotation by re-writing oversize before triggering.
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            // Second rotation: .log.1 -> .log.2, .log -> .log.1
            fs::write(&log, b"GEN2-current").unwrap();
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            // Third rotation: .log.2 -> .log.3, .log.1 -> .log.2, .log -> .log.1
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            // Fourth rotation: .log.3 dropped (would become .log.4 which we don't keep);
            // .log.2 -> .log.3, .log.1 -> .log.2, .log -> .log.1
            fs::write(&log, &oversize).unwrap();
            rotate_log_if_large(&log);

            // After 4 rotations of oversize files we keep exactly KEEP_GENERATIONS - 1
            // historical files (.1 through .KEEP_GENERATIONS-1).
            for n in 1..LOG_KEEP_GENERATIONS {
                assert!(
                    generation_path(&log, n).exists(),
                    "generation .{n} should exist"
                );
            }
            // The oldest generation we'd write is LOG_KEEP_GENERATIONS - 1.
            // Higher numbers should not appear because the rotate loop only
            // walks up to LOG_KEEP_GENERATIONS - 1.
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
            // log doesn't exist; should not panic / create anything.
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
            // The daemon key embeds the semver version, so the pid file name
            // carries dots ("mcp-0.11.0-<hash>.pid"). with_extension must
            // replace only the trailing ".pid", leaving the version intact.
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

        // ---------- shutdown connection drain ----------

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
            // A tool body wedged inside spawn_blocking survives abort_all —
            // aborting the connection task cannot interrupt a blocking
            // thread that already started. The drain must return promptly
            // and report the connection as still in flight; joining the
            // blocking work is only avoidable at shutdown by exiting the
            // process instead of dropping the runtime.
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
            // Wait for the blocking body to actually start so the abort
            // provably races a running (not queued) blocking task.
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
            // Release the blocking thread so this test runtime's drop
            // (which does join the blocking pool) can complete.
            release_tx.send(()).unwrap();
        }

        // ---------- spawner wait-for-daemon ----------

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
            // Boundary: reaching the cap exactly is a give-up.
            assert_eq!(spawner_wait_decision(true, cap, cap), CapExceeded);
            assert_eq!(
                spawner_wait_decision(true, Duration::from_secs(20), cap),
                CapExceeded
            );
            // A dead child fails immediately regardless of elapsed time —
            // even past the cap, its exit is the more informative failure.
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
            // Slow-bind simulation: the child stand-in stays alive while a
            // helper thread binds the socket only after the first wait round
            // has already timed out. The old single-shot wait failed here.
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
            // Guarantee the child is observably dead before the wait; the
            // status is cached, so the later try_wait still sees it.
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

        // ---------- sibling reap decision table ----------

        #[test]
        fn sibling_reap_dead_or_recycled_pid_cleans_files() {
            // A record that fails validation is a dead daemon or a recycled
            // pid — never signal it, just clear its runtime files.
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
            // Without comparable start times on BOTH sides (the non-Linux
            // case, where process_start_time_ticks returns None) the
            // strictly-before race guard can't run. Reaping is an
            // optimization; killing an innocent same-UID process whose pid
            // was recycled is not recoverable — so a validated sibling with
            // missing start times is always left alone.
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
            // Equal or newer start time: could be a daemon racing us up —
            // never reap.
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
    // L3: surface the unsupported flag instead of silently ignoring it.
    // Non-Unix has no daemon path; --runtime-dir would have configured
    // a daemon that can't exist, so failing loudly is right.
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
