use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream as BlockingStream,
    path::Path,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_BYTES: usize = 37;

fn connect(path: &Path) -> io::Result<BlockingStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(path)?, CONTROL_TIMEOUT)?;
    Ok(BlockingStream::from(socket))
}

#[cfg(target_os = "linux")]
pub(super) fn legacy_peer_matches(path: &Path, pid: i32, version: &str) -> io::Result<bool> {
    let mut stream = connect(path)?;
    let peer = rustix::net::sockopt::socket_peercred(&stream)?;
    if peer.pid.as_raw_nonzero().get() != pid || peer.uid != rustix::process::getuid() {
        return Ok(false);
    }
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"codesage-daemon-control\",\"version\":\"1\"}}}\n")?;
    let mut reply = Vec::new();
    let deadline = std::time::Instant::now() + CONTROL_TIMEOUT;
    let mut buffer = [0; 4096];
    while reply.len() < 64 * 1024 {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "legacy daemon handshake timed out")
            })?;
        stream.set_read_timeout(Some(remaining))?;
        let limit = (64 * 1024 - reply.len()).min(buffer.len());
        let count = stream.read(&mut buffer[..limit])?;
        if count == 0 {
            break;
        }
        if let Some(end) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            reply.extend_from_slice(&buffer[..=end]);
            break;
        }
        reply.extend_from_slice(&buffer[..count]);
    }
    let response: serde_json::Value = serde_json::from_slice(&reply)?;
    Ok(response["id"] == 1
        && response["result"]["serverInfo"]["name"] == "codesage"
        && response["result"]["serverInfo"]["version"] == version)
}

pub(super) fn socket_may_be_live(path: &Path) -> bool {
    match connect(path) {
        Ok(_) => true,
        Err(error) => !matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ),
    }
}

pub(super) fn request(
    path: &Path,
    pid: i32,
    nonce: [u8; 32],
    stop: bool,
) -> io::Result<BlockingStream> {
    let mut stream = connect(path)?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    let mut message = [0; REQUEST_BYTES];
    message[0] = if stop { b'!' } else { b'?' };
    message[1..5].copy_from_slice(&pid.to_be_bytes());
    message[5..].copy_from_slice(&nonce);
    stream.write_all(&message)?;
    let mut response = [0];
    stream.read_exact(&mut response)?;
    if response != [b'+'] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon identity mismatch",
        ));
    }
    Ok(stream)
}

pub(super) fn stop(path: &Path, pid: i32, nonce: [u8; 32]) -> io::Result<()> {
    let mut stream = request(path, pid, nonce, true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut byte = [0];
    match stream.read(&mut byte)? {
        0 => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected shutdown response",
        )),
    }
}

// A stop connection remains open through the drain. EOF then proves the daemon
// released the connection, without polling a PID that may have been recycled.
pub(super) async fn accept(
    mut stream: UnixStream,
    uid: u32,
    pid: i32,
    nonce: [u8; 32],
) -> io::Result<Option<UnixStream>> {
    if stream.peer_cred()?.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "foreign control peer",
        ));
    }
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let mut message = [0; REQUEST_BYTES];
        stream.read_exact(&mut message).await?;
        if !matches!(message[0], b'?' | b'!')
            || message[1..5] != pid.to_be_bytes()
            || message[5..] != nonce
        {
            stream.write_all(b"-").await?;
            return Ok(None);
        }
        stream.write_all(b"+").await?;
        Ok((message[0] == b'!').then_some(stream))
    })
    .await?
}
