//! The read-only listener's streams (`yas server --read-only-sock PATH`).
//!
//! Every session that arrives there is made read-only before this server reads
//! a byte of it: the client's HELLO goes through
//! [`yas_wire::read_only::ReadOnlyIngress`], exactly as a read-only share's does
//! in the WebRTC forwarder, so negotiation grants the passive catalogue whatever
//! the client asked for. Writes pass through untouched.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use yas_wire::read_only::ReadOnlyIngress;

/// Bytes read from the client per step until the HELLO went through.
const HELLO_READ_CHUNK: usize = 8 * 1024;

pub(crate) struct ReadOnlyStream<S> {
    inner: S,
    ingress: ReadOnlyIngress,
    /// Rewritten bytes not yet handed to the reader.
    ready: Vec<u8>,
    offset: usize,
}

impl<S> ReadOnlyStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            ingress: ReadOnlyIngress::new(),
            ready: Vec::new(),
            offset: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReadOnlyStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.offset < this.ready.len() {
                let count = buf.remaining().min(this.ready.len() - this.offset);
                buf.put_slice(&this.ready[this.offset..this.offset + count]);
                this.offset += count;
                if this.offset == this.ready.len() {
                    this.ready = Vec::new();
                    this.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.ingress.is_negotiated() {
                return Pin::new(&mut this.inner).poll_read(cx, buf);
            }
            let mut chunk = [0; HELLO_READ_CHUNK];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                // The client left before its HELLO was whole.
                return Poll::Ready(Ok(()));
            }
            match this.ingress.push(read.filled()) {
                Ok(Some(bytes)) => {
                    this.ready = bytes;
                    this.offset = 0;
                }
                Ok(None) => {}
                Err(error) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("read-only listener: {error}"),
                    )));
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReadOnlyStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_client_that_does_not_speak_yas_reads_as_an_error() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut stream = ReadOnlyStream::new(server);
        let mut byte = [0; 1];
        let error = stream.read(&mut byte).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn writes_pass_through() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut stream = ReadOnlyStream::new(server);
        stream.write_all(b"to the client").await.unwrap();
        let mut bytes = [0; 13];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"to the client");
    }
}
