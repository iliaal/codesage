use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream as BlockingStream,
    path::Path,
    time::{Duration, Instant},
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
    if response != *b"+" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon identity mismatch",
        ));
    }
    Ok(stream)
}

pub(super) fn stop(path: &Path, pid: i32, nonce: [u8; 32]) -> io::Result<()> {
    let stream = request(path, pid, nonce, true)?;
    wait_for_shutdown(stream, Duration::from_secs(10))
}

fn wait_for_shutdown(mut stream: BlockingStream, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "daemon shutdown timed out"))?;
        let timeout = rustix::event::Timespec::try_from(remaining).map_err(io::Error::other)?;
        let mut poll = [rustix::event::PollFd::new(
            &stream,
            rustix::event::PollFlags::IN,
        )];
        // macOS rejects socket-option changes after the acknowledged peer exits.
        match rustix::event::poll(&mut poll, Some(&timeout)) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon shutdown timed out",
                ));
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected shutdown response",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_wait_accepts_an_already_closed_peer() {
        let (stream, peer) = BlockingStream::pair().unwrap();
        stream.set_read_timeout(Some(CONTROL_TIMEOUT)).unwrap();
        drop(peer);
        #[cfg(target_os = "macos")]
        assert_eq!(
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
        wait_for_shutdown(stream, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn shutdown_wait_rejects_unexpected_data() {
        let (stream, mut peer) = BlockingStream::pair().unwrap();
        peer.write_all(b"x").unwrap();
        drop(peer);
        assert_eq!(
            wait_for_shutdown(stream, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn shutdown_wait_times_out_while_the_peer_remains_open() {
        let (stream, _peer) = BlockingStream::pair().unwrap();
        let timeout = Duration::from_millis(20);
        let started = Instant::now();
        assert_eq!(
            wait_for_shutdown(stream, timeout).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() >= timeout);
    }
}
