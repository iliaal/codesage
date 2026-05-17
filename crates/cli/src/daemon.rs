#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(not(unix))]
use anyhow::{Result, bail};

#[cfg(unix)]
mod unix {
    use std::{
        collections::hash_map::DefaultHasher,
        fs::{self, OpenOptions},
        hash::{Hash, Hasher},
        io::{self, Write},
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, bail};
    use rmcp::ServiceExt;
    use tokio::{
        io::{AsyncWriteExt, copy},
        net::{UnixListener, UnixStream},
        time::sleep,
    };

    use crate::mcp::{CodeSageServer, CodeSageServerState};

    const START_TIMEOUT: Duration = Duration::from_secs(5);
    const RETRY_DELAY: Duration = Duration::from_millis(25);

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

    pub(crate) async fn run_mcp_shim(runtime_dir: Option<PathBuf>) -> Result<()> {
        let paths = DaemonPaths::for_current_exe(runtime_dir)?;
        prepare_runtime_dir(&paths.runtime_dir)?;
        let stream = ensure_daemon(&paths).await?;
        proxy_stdio(stream).await
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

        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("binding {}", paths.socket.display()))?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", paths.socket.display()))?;
        fs::write(&paths.pid, std::process::id().to_string())
            .with_context(|| format!("writing {}", paths.pid.display()))?;

        tracing::info!(socket = %paths.socket.display(), "codesage MCP daemon listening");
        let state = Arc::new(CodeSageServerState::new());
        loop {
            let (stream, _) = listener.accept().await.with_context(|| {
                format!(
                    "accepting MCP daemon connection on {}",
                    paths.socket.display()
                )
            })?;
            let server = CodeSageServer::with_state(state.clone());
            tokio::spawn(async move {
                if let Err(e) = serve_client(server, stream).await {
                    tracing::debug!(error = %e, "MCP daemon client connection ended");
                }
            });
        }
    }

    async fn serve_client(server: CodeSageServer, stream: UnixStream) -> Result<()> {
        let service = server
            .serve(stream)
            .await
            .map_err(|e| anyhow::anyhow!("MCP daemon server error: {e}"))?;
        service
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("MCP daemon server stopped: {e}"))?;
        Ok(())
    }

    async fn ensure_daemon(paths: &DaemonPaths) -> Result<UnixStream> {
        if let Ok(stream) = UnixStream::connect(&paths.socket).await {
            return Ok(stream);
        }

        match StartLock::try_acquire(&paths.lock)? {
            Some(_lock) => {
                remove_stale_socket(paths).await?;
                spawn_daemon(paths)?;
                wait_for_socket(&paths.socket, START_TIMEOUT).await
            }
            None => match wait_for_socket(&paths.socket, START_TIMEOUT).await {
                Ok(stream) => Ok(stream),
                Err(_) => {
                    let _ = fs::remove_file(&paths.lock);
                    let _lock = StartLock::try_acquire(&paths.lock)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "another codesage MCP daemon starter still holds {}",
                            paths.lock.display()
                        )
                    })?;
                    remove_stale_socket(paths).await?;
                    spawn_daemon(paths)?;
                    wait_for_socket(&paths.socket, START_TIMEOUT).await
                }
            },
        }
    }

    fn spawn_daemon(paths: &DaemonPaths) -> Result<()> {
        let exe = std::env::current_exe().context("resolving current executable")?;
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

    async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<UnixStream> {
        let deadline = Instant::now() + timeout;
        loop {
            let error = match UnixStream::connect(path).await {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for codesage MCP daemon at {}: {}",
                    path.display(),
                    error
                );
            }
            sleep(RETRY_DELAY).await;
        }
    }

    async fn proxy_stdio(stream: UnixStream) -> Result<()> {
        let (mut socket_read, mut socket_write) = tokio::io::split(stream);
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

        let stdin_to_socket = async {
            copy(&mut stdin, &mut socket_write).await?;
            socket_write.shutdown().await
        };
        let socket_to_stdout = async {
            copy(&mut socket_read, &mut stdout).await?;
            stdout.flush().await
        };
        tokio::try_join!(stdin_to_socket, socket_to_stdout)?;
        Ok(())
    }

    fn prepare_runtime_dir(path: &Path) -> Result<()> {
        fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
        Ok(())
    }

    fn default_runtime_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("CODESAGE_DAEMON_RUNTIME_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir).join("codesage");
        }
        let suffix = std::env::var("UID")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());
        std::env::temp_dir().join(format!("codesage-{suffix}"))
    }

    fn daemon_key_for_exe(exe: &Path) -> Result<String> {
        let meta = fs::metadata(exe).with_context(|| format!("reading {}", exe.display()))?;
        let modified = meta
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut hasher = DefaultHasher::new();
        env!("CARGO_PKG_VERSION").hash(&mut hasher);
        exe.to_string_lossy().hash(&mut hasher);
        meta.dev().hash(&mut hasher);
        meta.ino().hash(&mut hasher);
        meta.len().hash(&mut hasher);
        modified.as_secs().hash(&mut hasher);
        modified.subsec_nanos().hash(&mut hasher);
        Ok(format!(
            "{}-{:016x}",
            env!("CARGO_PKG_VERSION"),
            hasher.finish()
        ))
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
    }
}

#[cfg(unix)]
pub(crate) use unix::{run_daemon, run_mcp_shim};

#[cfg(not(unix))]
pub(crate) async fn run_mcp_shim(_runtime_dir: Option<PathBuf>) -> Result<()> {
    crate::mcp::run_mcp_server().await
}

#[cfg(not(unix))]
pub(crate) async fn run_daemon(_runtime_dir: Option<PathBuf>) -> Result<()> {
    bail!("codesage MCP daemon requires Unix domain sockets")
}
