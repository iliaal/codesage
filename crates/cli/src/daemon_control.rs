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
