use std::io;

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Notify,
};

use super::unix::{DaemonPaths, SPAWNER_WAIT_CAP, ensure_daemon, proxy_exit_code};
use crate::mcp::{framing, inject_default_project_line};

const MAX_PENDING_REQUESTS: usize = 1024;

#[derive(Default)]
struct Session {
    initialize: Option<Frame>,
    initialized: Option<Frame>,
    initialize_answered: bool,
    pending: Vec<PendingRequest>,
    pending_bytes: usize,
    recovered_without_response: bool,
}

struct PendingRequest {
    id: Value,
    initialize: bool,
}

struct Connection {
    replies: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self {
            replies: BufReader::new(read).lines(),
            write,
        }
    }
}

impl Session {
    fn recoverable(&self) -> bool {
        self.initialize_answered && self.initialized.is_some()
    }

    fn written(&mut self, frame: &Frame) {
        if let Some(id) = frame.request_id() {
            self.pending_bytes += id.to_string().len();
            self.pending.push(PendingRequest {
                id: id.clone(),
                initialize: self.initialize.is_none()
                    && frame.message.get("method").and_then(Value::as_str) == Some("initialize"),
            });
        }
    }

    fn sent(&mut self, frame: &Frame) {
        match frame.message.get("method").and_then(Value::as_str) {
            Some("initialize") if self.initialize.is_none() => {
                self.initialize = Some(frame.clone());
            }
            Some("notifications/initialized") if self.initialize.is_some() => {
                self.initialized = Some(frame.clone());
            }
            _ => {}
        }
    }

    fn answered(&mut self, frame: &Frame) {
        if frame.message.get("method").is_some()
            || (frame.message.get("result").is_none() && frame.message.get("error").is_none())
        {
            return;
        }
        let Some(id) = frame.message.get("id") else {
            return;
        };
        if let Some(position) = self.pending.iter().position(|pending| &pending.id == id) {
            let pending = self.pending.remove(position);
            self.pending_bytes -= pending.id.to_string().len();
            if pending.initialize {
                self.initialize_answered = frame.message.get("error").is_none();
            } else {
                self.recovered_without_response = false;
            }
        }
    }
}

#[derive(Clone)]
struct Frame {
    bytes: Vec<u8>,
    offset: usize,
    message: Value,
}

impl Frame {
    fn new(mut line: String, project: Option<&str>) -> io::Result<Self> {
        if let Some(project) = project {
            line = inject_default_project_line(&line, project);
            if line.len() > framing::MAX_FRAME_BYTES {
                return Err(framing::oversized_frame());
            }
        }
        let message = serde_json::from_str(&line).unwrap_or(Value::Null);
        line.push('\n');
        Ok(Self {
            bytes: line.into_bytes(),
            offset: 0,
            message,
        })
    }

    fn request_id(&self) -> Option<&Value> {
        self.message.get("method")?.as_str()?;
        self.message
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
    }

    async fn write<W: AsyncWrite + Unpin>(&mut self, writer: &mut W) -> io::Result<()> {
        while self.offset < self.bytes.len() {
            let count = writer.write(&self.bytes[self.offset..]).await?;
            if count == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.offset += count;
        }
        writer.flush().await
    }
}

enum Stop {
    ClientEof,
    ClientError(io::Error),
    Daemon(io::Result<()>),
}

async fn forward_client<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    lines: &mut Lines<R>,
    writer: &mut W,
    outgoing: &mut Option<Frame>,
    session: &Mutex<Session>,
    answered: &Notify,
    project: Option<&str>,
) -> Stop {
    loop {
        if outgoing.is_none() {
            loop {
                let full = {
                    let session = session.lock();
                    session.pending.len() >= MAX_PENDING_REQUESTS
                        || session.pending_bytes >= framing::MAX_FRAME_BYTES
                };
                if !full {
                    break;
                }
                answered.notified().await;
            }
            match lines.next_line().await {
                Ok(Some(line)) => match Frame::new(line, project) {
                    Ok(frame) => *outgoing = Some(frame),
                    Err(error) => return Stop::ClientError(error),
                },
                Ok(None) => {
                    let _ = writer.shutdown().await;
                    return Stop::ClientEof;
                }
                Err(error) => return Stop::ClientError(error),
            }
        }
        let Some(frame) = outgoing.as_mut() else {
            continue;
        };
        while frame.offset < frame.bytes.len() {
            match writer.write(&frame.bytes[frame.offset..]).await {
                Ok(0) => return Stop::Daemon(Err(io::ErrorKind::WriteZero.into())),
                Ok(count) => {
                    if frame.offset == 0 {
                        session.lock().written(frame);
                    }
                    frame.offset += count;
                }
                Err(error) => return Stop::Daemon(Err(error)),
            }
        }
        session.lock().sent(frame);
        *outgoing = None;
        if let Err(error) = writer.flush().await {
            return Stop::Daemon(Err(error));
        }
    }
}

async fn forward_daemon<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    lines: &mut Lines<R>,
    stdout: &mut W,
    output: &mut Option<Frame>,
    session: &Mutex<Session>,
    answered: &Notify,
) -> Stop {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let frame = match Frame::new(line, None) {
                    Ok(frame) if frame.message.is_object() => frame,
                    Ok(_) => {
                        return Stop::Daemon(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid JSON-RPC frame from MCP daemon",
                        )));
                    }
                    Err(error) => return Stop::Daemon(Err(error)),
                };
                session.lock().answered(&frame);
                answered.notify_one();
                *output = Some(frame);
                if let Some(frame) = output.as_mut()
                    && let Err(error) = frame.write(stdout).await
                {
                    return Stop::ClientError(error);
                }
                *output = None;
            }
            Ok(None) => return Stop::Daemon(Ok(())),
            Err(error) => return Stop::Daemon(Err(error)),
        }
    }
}

async fn lost_request<W: AsyncWrite + Unpin>(
    stdout: &mut W,
    id: Value,
    written: bool,
) -> io::Result<()> {
    let response = json!({
        "jsonrpc": "2.0", "id": id,
        "error": {
            "code": -32000,
            "message": "MCP daemon connection lost; request was not replayed",
            "data": {"code": "E_CONNECTION_LOST", "may_have_run": written, "replayed": false}
        }
    });
    Frame::new(response.to_string(), None)?.write(stdout).await
}

async fn reconnect(paths: &DaemonPaths, session: &Mutex<Session>) -> Result<Connection> {
    let (mut initialize, mut initialized) = {
        let session = session.lock();
        (
            session.initialize.clone().context("missing initialize")?,
            session.initialized.clone().context("missing initialized")?,
        )
    };
    initialize.offset = 0;
    initialized.offset = 0;
    let recovery = async {
        let mut connection = Connection::new(ensure_daemon(paths).await?);
        initialize.write(&mut connection.write).await?;
        let response = connection
            .replies
            .next_line()
            .await?
            .context("daemon closed during initialize replay")?;
        let response: Value = serde_json::from_str(&response)?;
        if response.get("id") != initialize.request_id()
            || response.get("result").is_none()
            || response.get("error").is_some()
        {
            bail!("daemon refused initialize replay");
        }
        initialized.write(&mut connection.write).await?;
        Ok(connection)
    };
    tokio::time::timeout(SPAWNER_WAIT_CAP, recovery)
        .await
        .context("MCP daemon reconnect deadline expired")?
}

pub(super) async fn run(
    paths: &DaemonPaths,
    stream: UnixStream,
    project: Option<String>,
) -> Result<()> {
    let result = proxy(paths, stream, project).await;
    if let Err(error) = &result {
        tracing::warn!(%error, "MCP stdio shim stopped");
    }
    // Dropping Tokio's runtime would join its uncancellable blocking stdin read.
    std::process::exit(proxy_exit_code(&result));
}

async fn proxy(paths: &DaemonPaths, stream: UnixStream, project: Option<String>) -> Result<()> {
    let mut input = BufReader::new(framing::FrameReader::new(tokio::io::stdin())).lines();
    let mut stdout = tokio::io::stdout();
    let session = Mutex::new(Session::default());
    let answered = Notify::new();
    let mut outgoing = None;
    let mut output = None;
    let mut connection = Connection::new(stream);
    loop {
        let Connection {
            mut replies,
            mut write,
        } = connection;
        let (stop, client_eof) = {
            let client = forward_client(
                &mut input,
                &mut write,
                &mut outgoing,
                &session,
                &answered,
                project.as_deref(),
            );
            let daemon =
                forward_daemon(&mut replies, &mut stdout, &mut output, &session, &answered);
            tokio::pin!(client, daemon);
            tokio::select! {
                stop = &mut client => {
                    if matches!(stop, Stop::ClientEof) {
                        (daemon.await, true)
                    } else {
                        (stop, false)
                    }
                },
                stop = &mut daemon => (stop, false),
            }
        };
        if let Stop::ClientError(error) = stop {
            return Err(error.into());
        }
        drop(replies);
        drop(write);
        if let Some(frame) = output.as_mut() {
            frame.write(&mut stdout).await?;
        }
        output = None;
        let (pending, recoverable) = {
            let mut session = session.lock();
            let recoverable =
                !client_eof && session.recoverable() && !session.recovered_without_response;
            session.pending_bytes = 0;
            (std::mem::take(&mut session.pending), recoverable)
        };
        for pending in pending {
            lost_request(&mut stdout, pending.id, true).await?;
        }
        if outgoing.as_ref().is_some_and(|frame| frame.offset > 0) {
            outgoing = None;
        }
        if !recoverable {
            if let Some(id) = outgoing.as_ref().and_then(Frame::request_id) {
                lost_request(&mut stdout, id.clone(), false).await?;
            }
            return match stop {
                Stop::Daemon(result) => result.map_err(Into::into),
                _ => Ok(()),
            };
        }
        if outgoing.is_none() {
            let Some(line) = input.next_line().await? else {
                return Ok(());
            };
            outgoing = Some(Frame::new(line, project.as_deref())?);
        }
        match reconnect(paths, &session).await {
            Ok(next) => {
                connection = next;
                session.lock().recovered_without_response = true;
            }
            Err(error) => {
                if let Some(id) = outgoing.as_ref().and_then(Frame::request_id) {
                    lost_request(&mut stdout, id.clone(), false).await?;
                }
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    struct FailingWriter {
        remaining: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }
            let count = self.remaining.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn partial_socket_writes_are_pending_but_zero_byte_failures_remain_unsent() {
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n";
        for count in [0, 7] {
            let mut input = BufReader::new(request.as_slice()).lines();
            let mut writer = FailingWriter {
                remaining: count,
                bytes: Vec::new(),
            };
            let mut outgoing = None;
            let state = Mutex::new(Session::default());
            let stop = forward_client(
                &mut input,
                &mut writer,
                &mut outgoing,
                &state,
                &Notify::new(),
                None,
            )
            .await;
            assert!(matches!(stop, Stop::Daemon(Err(_))));
            assert_eq!(writer.bytes, request[..count]);
            assert_eq!(outgoing.unwrap().offset, count);
            assert_eq!(state.lock().pending.len(), usize::from(count > 0));
        }
    }

    #[test]
    fn reusing_initialize_id_for_a_later_request_counts_as_recovery_progress() {
        let mut session = Session::default();
        let init = Frame::new(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.into(),
            None,
        )
        .unwrap();
        session.written(&init);
        session.sent(&init);
        let response = Frame::new(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.into(), None).unwrap();
        session.answered(&response);
        assert!(session.initialize_answered);
        session.recovered_without_response = true;
        let ping = Frame::new(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.into(), None).unwrap();
        session.written(&ping);
        session.answered(&response);
        assert!(!session.recovered_without_response);
        assert!(session.pending.is_empty());
        assert_eq!(session.pending_bytes, 0);
    }
}
