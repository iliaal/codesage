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
        process::{Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, bail};
    use rmcp::ServiceExt;
    use tokio::{
        io::{AsyncWriteExt, copy},
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
        let paths = DaemonPaths::for_current_exe(runtime_dir)?;
        let Some(pid) = read_daemon_pid(&paths.pid) else {
            println!("not running (no pid file at {})", paths.pid.display());
            std::process::exit(1);
        };
        if !pid_alive(pid) {
            println!(
                "not running (pid file references dead pid {}; left over from a previous run)",
                pid
            );
            std::process::exit(1);
        }
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
        let paths = DaemonPaths::for_current_exe(runtime_dir)?;
        let Some(pid) = read_daemon_pid(&paths.pid) else {
            println!("not running (no pid file at {})", paths.pid.display());
            return Ok(());
        };
        if !pid_alive(pid) {
            println!(
                "not running (pid {} already dead); cleaning stale files",
                pid
            );
            let _ = fs::remove_file(&paths.pid);
            let _ = fs::remove_file(&paths.socket);
            return Ok(());
        }
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
        fs::write(&paths.pid, std::process::id().to_string())
            .with_context(|| format!("writing {}", paths.pid.display()))?;

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
                    let (stream, _) = accepted.with_context(|| {
                        format!(
                            "accepting MCP daemon connection on {}",
                            paths.socket.display()
                        )
                    })?;

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

        // Best-effort cleanup. The runtime dir is left in place because
        // other daemon keys may share it.
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.pid);
        Ok(())
    }

    /// M7: a per-request timeout would require introspecting the rmcp
    /// service's tool dispatch, which is private to the crate. As a
    /// coarse alternative, the entire client connection has a hard
    /// ceiling — if a hung tool call (e.g. deadlocked ORT session)
    /// pins the connection past this, the daemon forcibly drops it
    /// so the agent gets an error instead of an indefinite hang. A
    /// healthy MCP session is typically a few minutes; one hour is
    /// generous for slow, multi-call sweeps and still bounded.
    const CLIENT_SESSION_MAX: Duration = Duration::from_secs(3600);

    async fn serve_client(server: CodeSageServer, stream: UnixStream) -> Result<()> {
        let service = server
            .serve(stream)
            .await
            .map_err(|e| anyhow::anyhow!("MCP daemon server error: {e}"))?;

        let wait = service.waiting();
        match tokio::time::timeout(CLIENT_SESSION_MAX, wait).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("MCP daemon server stopped: {e}")),
            Err(_elapsed) => {
                tracing::warn!(
                    "MCP client connection exceeded {:?}; dropping",
                    CLIENT_SESSION_MAX
                );
                Ok(())
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
                if let Some(pid) = read_daemon_pid(&paths.pid)
                    && !pid_alive(pid)
                {
                    bail!(
                        "codesage MCP daemon (pid {}) exited before becoming ready; \
                         see {} for the failure",
                        pid,
                        paths.log.display()
                    );
                }
                last_alive_check = Instant::now();
            }
            sleep(RETRY_DELAY).await;
        }
    }

    fn read_daemon_pid(pid_path: &Path) -> Option<i32> {
        fs::read_to_string(pid_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&p: &i32| p > 0)
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

    fn prepare_runtime_dir(path: &Path) -> Result<()> {
        fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
        Ok(())
    }

    fn default_runtime_dir() -> PathBuf {
        // Treat set-but-empty env vars as unset. Documented workaround for the
        // WSL2 `/run/user/$UID` trap is to inject `XDG_RUNTIME_DIR=""` in the
        // client MCP env so the shim falls through to `/tmp`; without this
        // guard `PathBuf::from("").join("codesage")` collapses to a relative
        // `codesage/` that gets created next to whatever cwd the shim was
        // spawned in.
        if let Some(dir) = nonempty_env("CODESAGE_DAEMON_RUNTIME_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(dir) = nonempty_env("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir).join("codesage");
        }
        let suffix = std::env::var("UID")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());
        std::env::temp_dir().join(format!("codesage-{suffix}"))
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
            // Mutating process env in tests is racy with parallel test
            // threads, so this is wrapped in a Mutex.
            use std::sync::Mutex;
            static ENV_LOCK: Mutex<()> = Mutex::new(());
            let _guard = ENV_LOCK.lock().unwrap();

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
                resolved.starts_with(std::env::temp_dir()),
                "expected /tmp fallback, got {}",
                resolved.display()
            );

            unsafe {
                std::env::remove_var("CODESAGE_DAEMON_RUNTIME_DIR");
                std::env::remove_var("XDG_RUNTIME_DIR");
            }
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
