use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};

// A built-in embedding call has at most 2 MiB of text, even before JSON's 6x escaping.
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn oversized_frame() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("MCP frame exceeds {MAX_FRAME_BYTES} bytes; closing connection"),
    )
}

/// Withhold a JSONL frame until its byte limit is checked, before parsing or forwarding it.
/// LF is the delimiter; every other byte, including CR and invalid UTF-8, counts.
pub(crate) struct FrameReader<R> {
    inner: BufReader<R>,
    frame: Vec<u8>,
    offset: usize,
    ready: bool,
    ended: bool,
    limit: usize,
}

impl<R: AsyncRead> FrameReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_FRAME_BYTES)
    }

    fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner: BufReader::new(inner),
            frame: Vec::new(),
            offset: 0,
            ready: false,
            ended: false,
            limit,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FrameReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        while !this.ready && !this.ended {
            let available = match ready!(Pin::new(&mut this.inner).poll_fill_buf(cx)) {
                Ok(available) => available,
                Err(error) => {
                    this.ended = true;
                    this.frame.clear();
                    return Poll::Ready(Err(error));
                }
            };
            if available.is_empty() {
                this.ended = true;
                this.ready = !this.frame.is_empty();
                break;
            }
            let newline = available.iter().position(|&b| b == b'\n');
            let payload = newline.unwrap_or(available.len());
            if payload > this.limit - this.frame.len() {
                this.ended = true;
                this.frame.clear();
                return Poll::Ready(Err(oversized_frame()));
            }
            let consumed = payload + usize::from(newline.is_some());
            let needed = this.frame.len() + consumed;
            if needed > this.frame.capacity() {
                let capacity = needed
                    .max(this.frame.capacity().saturating_mul(2))
                    .min(this.limit + 1);
                this.frame.reserve_exact(capacity - this.frame.len());
            }
            this.frame.extend_from_slice(&available[..consumed]);
            Pin::new(&mut this.inner).consume(consumed);
            this.ready = newline.is_some();
        }
        if this.ready {
            let count = out.remaining().min(this.frame.len() - this.offset);
            out.put_slice(&this.frame[this.offset..this.offset + count]);
            this.offset += count;
            if this.offset == this.frame.len() {
                this.frame.clear();
                this.offset = 0;
                this.ready = false;
            }
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn frame_boundaries_count_bytes_and_preserve_eof_and_crlf() {
        for input in [b"abcd\nabc\r\n\nend".as_slice(), b"abcd", b""] {
            let mut reader = FrameReader::with_limit(input, 4);
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, input);
            assert!(reader.frame.capacity() <= 5);
        }
        for input in [
            b"abcde\n".as_slice(),
            b"abcde",
            b"abcd\r\n",
            b"\xff\xff\xff\xff\xff",
        ] {
            let mut reader = FrameReader::with_limit(input, 4);
            let mut output = Vec::new();
            let error = reader.read_to_end(&mut output).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(output.is_empty(), "oversized bytes were forwarded");
            assert!(reader.frame.capacity() <= 5);
            assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn split_chunks_and_cancelled_reads_cannot_forward_an_oversized_prefix() {
        let (mut writer, input) = tokio::io::duplex(32);
        let mut reader = FrameReader::with_limit(input, 4);
        let mut out = [0; 16];
        for chunk in [b"ab".as_slice(), b"cd"] {
            writer.write_all(chunk).await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), reader.read(&mut out))
                    .await
                    .is_err()
            );
        }
        writer.write_all(b"e").await.unwrap();
        assert_eq!(
            reader.read(&mut out).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn completed_frames_are_delivered_before_a_later_oversized_frame() {
        let mut reader = FrameReader::with_limit(b"ok\n12345\nignored\n".as_slice(), 4);
        let mut out = Vec::new();
        assert!(reader.read_to_end(&mut out).await.is_err());
        assert_eq!(out, b"ok\n");
    }

    #[tokio::test]
    async fn project_injection_cannot_forward_an_expanded_oversized_frame() {
        let prefix = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"help","arguments":{"padding":""#;
        let suffix = r#""}}}"#;
        let mut wire = prefix.to_owned();
        wire.push_str(&"x".repeat(MAX_FRAME_BYTES - prefix.len() - suffix.len()));
        wire.push_str(suffix);
        assert_eq!(wire.len(), MAX_FRAME_BYTES);
        wire.push('\n');
        let mut output = Vec::new();
        let error = crate::mcp::pump_lines(wire.as_bytes(), &mut output, Some("/project".into()))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn split_utf8_and_reads_smaller_than_a_frame_preserve_all_bytes() {
        let (mut writer, input) = tokio::io::duplex(32);
        let mut reader = FrameReader::with_limit(input, 4);
        writer.write_all(&[0xc3]).await.unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), reader.read(&mut byte))
                .await
                .is_err()
        );
        writer.write_all(&[0xa9, b'a', b'b', b'\n']).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut output = Vec::new();
        while reader.read(&mut byte).await.unwrap() != 0 {
            output.push(byte[0]);
        }
        assert_eq!(output, "éab\n".as_bytes());
    }
}
