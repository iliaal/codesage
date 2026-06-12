#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(not(unix))]
use anyhow::{Result, bail};

#[cfg(unix)]
mod unix {
    use std::{
        fs::{self, OpenOptions},
        io::{self, Write},
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        pin::Pin,
        process::{Command, Stdio},
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
        time::sleep,
    };

    use crate::mcp::{CodeSageServer, CodeSageServerState};

    const START_TIMEOUT: Duration = Duration::from_secs(5);
    const RETRY_DELAY: Duration = Duration::from_millis(25);

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

        tracing::info!(socket = %paths.socket.display(), "codesage MCP daemon listening");
        let state = Arc::new(CodeSageServerState::new());
        let our_uid = unsafe { libc::getuid() };

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
                    *last_activity.lock().unwrap() = Instant::now();
                    let active_for_conn = active.clone();
                    let last_activity_for_conn = last_activity.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_client(server, stream).await {
                            tracing::debug!(error = %e, "MCP daemon client connection ended");
                        }
                        active_for_conn.fetch_sub(1, Ordering::SeqCst);
                        // Reset the idle clock on disconnect so the timeout
                        // measures continuous idleness, not uptime.
                        *last_activity_for_conn.lock().unwrap() = Instant::now();
                    });
                }
                _ = idle_tick.tick() => {
                    if !idle_timeout.is_zero()
                        && active.load(Ordering::SeqCst) == 0
                        && last_activity.lock().unwrap().elapsed() >= idle_timeout
                    {
                        break "idle";
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
        // tear down. They share this process, so leaving them running past
        // daemon exit would orphan inotify threads.
        state.shutdown_all_watchers();

        // Best-effort cleanup. The runtime dir is left in place because
        // other daemon keys may share it.
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.pid);
        Ok(())
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
                *this.last_activity.lock().unwrap() = Instant::now();
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

    async fn serve_client(server: CodeSageServer, stream: UnixStream) -> Result<()> {
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let tracked = ActivityStream {
            inner: stream,
            last_activity: last_activity.clone(),
        };
        let service = server
            .serve(tracked)
            .await
            .map_err(|e| anyhow::anyhow!("MCP daemon server error: {e}"))?;

        let idle_max = client_idle_max();
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
                    let idle = last_activity.lock().unwrap().elapsed();
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
                    spawn_daemon(paths)?;
                    return wait_for_socket(&paths.socket, START_TIMEOUT, paths).await;
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

    fn spawn_daemon(paths: &DaemonPaths) -> Result<()> {
        let exe = std::env::current_exe().context("resolving current executable")?;
        rotate_log_if_large(&paths.log);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log)
            .with_context(|| format!("opening daemon log {}", paths.log.display()))?;
        Command::new(exe)
            .arg("daemon")
            .arg("--runtime-dir")
            .arg(&paths.runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                log.try_clone().context("cloning daemon log for stdout")?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting codesage MCP daemon")?;
        Ok(())
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

        // CR-002: pre-fix used try_join! which waits for BOTH futures.
        // If the daemon closed its write half but the MCP client kept
        // stdin open, the stdin pump blocked on read forever and the
        // shim hung with no server behind it.
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
                if let Err(e) = res {
                    tracing::warn!(error = %e, "MCP daemon connection closed with error");
                }
                std::process::exit(0);
            }
            res = &mut stdin_to_socket => {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "MCP client stdin closed with error");
                }
                // Stdin EOF: drain in-flight server response, then exit.
                let _ = socket_to_stdout.await;
                std::process::exit(0);
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
        // Treat set-but-empty env vars as unset. Documented workaround for the
        // WSL2 `/run/user/$UID` trap is to inject `XDG_RUNTIME_DIR=""` in the
        // client MCP env so the shim falls through to `/tmp`; without this
        // guard `PathBuf::from("").join("codesage")` collapses to a relative
        // `codesage/` that gets created next to whatever cwd the shim was
        // spawned in.
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = nonempty_env("CODESAGE_DAEMON_RUNTIME_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(dir) = nonempty_env("XDG_RUNTIME_DIR") {
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
        let legacy_tmp = std::env::temp_dir().join(format!("codesage-{uid}"));
        if !dirs.contains(&tmp) {
            dirs.push(tmp.clone());
        }
        if legacy_tmp != tmp && !dirs.contains(&legacy_tmp) {
            dirs.push(legacy_tmp);
        }
        dirs
    }

    fn default_runtime_dir() -> PathBuf {
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

    fn nonempty_env(key: &str) -> Option<std::ffi::OsString> {
        std::env::var_os(key).filter(|v| !v.is_empty())
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
        use std::ffi::OsString;
        use std::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::new(());

        fn restore_env(key: &str, value: Option<OsString>) {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
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
            // Regression: the WSL2 workaround `XDG_RUNTIME_DIR=""` (and the
            // analogous override on CODESAGE_DAEMON_RUNTIME_DIR) used to
            // produce a relative `codesage/` runtime dir because var_os
            // returns Some("") for set-but-empty vars. The shim then
            // mkdir'd `codesage/` next to whatever cwd it spawned in.
            //
            let _guard = ENV_LOCK.lock().unwrap();
            let old_runtime_dir = std::env::var_os("CODESAGE_DAEMON_RUNTIME_DIR");
            let old_xdg = std::env::var_os("XDG_RUNTIME_DIR");

            // SAFETY: tests in this file may not run in parallel with other
            // code that reads these vars; ENV_LOCK serializes against the
            // sibling test that touches the same vars (none today, but the
            // guard documents the invariant).
            unsafe {
                std::env::set_var("CODESAGE_DAEMON_RUNTIME_DIR", "");
                std::env::set_var("XDG_RUNTIME_DIR", "");
            }

            let resolved = default_runtime_dir();
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

            restore_env("CODESAGE_DAEMON_RUNTIME_DIR", old_runtime_dir);
            restore_env("XDG_RUNTIME_DIR", old_xdg);
        }

        #[test]
        fn fallback_runtime_dir_does_not_depend_on_tmpdir() {
            let _guard = ENV_LOCK.lock().unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let old_runtime_dir = std::env::var_os("CODESAGE_DAEMON_RUNTIME_DIR");
            let old_xdg = std::env::var_os("XDG_RUNTIME_DIR");
            let old_tmpdir = std::env::var_os("TMPDIR");

            unsafe {
                std::env::remove_var("CODESAGE_DAEMON_RUNTIME_DIR");
                std::env::remove_var("XDG_RUNTIME_DIR");
                std::env::set_var("TMPDIR", scratch.path());
            }

            let candidates = candidate_runtime_dirs();
            let uid = unsafe { libc::getuid() };
            let expected = PathBuf::from("/tmp").join(format!("codesage-{uid}"));
            assert_eq!(
                candidates.first(),
                Some(&expected),
                "canonical fallback must be stable across different TMPDIR values: {candidates:?}"
            );

            restore_env("CODESAGE_DAEMON_RUNTIME_DIR", old_runtime_dir);
            restore_env("XDG_RUNTIME_DIR", old_xdg);
            restore_env("TMPDIR", old_tmpdir);
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
            // CR-007: status/stop reconstruct a daemon's paths from a
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
    }
}

#[cfg(unix)]
pub(crate) use unix::{run_daemon, run_daemon_status, run_daemon_stop, run_mcp_shim};

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
