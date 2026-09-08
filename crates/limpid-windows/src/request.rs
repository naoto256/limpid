//! Windows control request envelope. Responses retain their existing raw
//! bytes. A big-endian u32 length precedes each data block; zero ends requests.
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
pub const MAX_FRAME: usize = 65536;

pub struct Reader<R> {
    inner: R,
    header: [u8; 4],
    header_len: usize,
    remaining: usize,
    ended: bool,
}
impl<R> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            header: [0; 4],
            header_len: 0,
            remaining: 0,
            ended: false,
        }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for Reader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.ended || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        while this.remaining == 0 {
            let mut header = ReadBuf::new(&mut this.header[this.header_len..]);
            std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, &mut header))?;
            let count = header.filled().len();
            if count == 0 {
                return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
            }
            this.header_len += count;
            if this.header_len < 4 {
                continue;
            }
            this.remaining = u32::from_be_bytes(this.header) as usize;
            this.header_len = 0;
            if this.remaining > MAX_FRAME {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control request frame exceeds 65536 bytes",
                )));
            }
            if this.remaining == 0 {
                this.ended = true;
                return Poll::Ready(Ok(()));
            }
        }
        let mut limited = buf.take(this.remaining);
        std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, &mut limited))?;
        let count = limited.filled().len();
        if count == 0 {
            return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
        }
        // The child ReadBuf initialized these exact bytes in the parent.
        unsafe {
            buf.assume_init(count);
        }
        buf.advance(count);
        this.remaining -= count;
        Poll::Ready(Ok(()))
    }
}

/// Bounded buffered encoder. A successful write accepts bytes into one frame;
/// flush/shutdown transmit it. Shutdown sends END without closing the reader.
pub struct Writer<W> {
    inner: W,
    pending: Vec<u8>,
    offset: usize,
    ended: bool,
}
impl<W> Writer<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            offset: 0,
            ended: false,
        }
    }
}
impl<W: AsyncWrite + Unpin> Writer<W> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.offset < self.pending.len() {
            let count = std::task::ready!(
                Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.offset..])
            )?;
            if count == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.offset += count;
        }
        self.pending.clear();
        self.offset = 0;
        Poll::Ready(Ok(()))
    }
}
impl<W: AsyncWrite + Unpin> AsyncWrite for Writer<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        std::task::ready!(this.drain(cx))?;
        let count = bytes.len().min(MAX_FRAME);
        if count != 0 {
            this.pending
                .extend_from_slice(&(count as u32).to_be_bytes());
            this.pending.extend_from_slice(&bytes[..count]);
        }
        Poll::Ready(Ok(count))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.drain(cx))?;
        if !this.ended {
            this.pending.extend_from_slice(&0u32.to_be_bytes());
            this.ended = true;
        }
        std::task::ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }
}
impl<W: AsyncRead + Unpin> AsyncRead for Writer<W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    #[tokio::test]
    async fn data_blocks_and_end_are_decoded_without_altering_bytes() {
        let wire: &[u8] = b"\0\0\0\x03\xff\0\n\0\0\0\x02ok\0\0\0\0";
        let mut bytes = Vec::new();
        Reader::new(wire).read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"\xff\0\nok");
    }
    #[tokio::test]
    async fn physical_eof_without_end_marker_is_an_error() {
        let mut bytes = Vec::new();
        let error = Reader::new(&b"\0\0\0\x02ok"[..])
            .read_to_end(&mut bytes)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
    #[tokio::test]
    async fn oversized_length_is_rejected_before_allocating_payload() {
        let header = (MAX_FRAME as u32 + 1).to_be_bytes();
        let mut bytes = Vec::new();
        let error = Reader::new(&header[..])
            .read_to_end(&mut bytes)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn fragmented_large_request_and_end_preserve_reply_channel() {
        use tokio::io::AsyncWriteExt;
        let (client, server) = tokio::io::duplex(7);
        let task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut bytes = Vec::new();
            Reader::new(read).read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, vec![0xff; MAX_FRAME * 2 + 3]);
            write.write_all(b"response\n").await.unwrap();
        });
        let mut client = Writer::new(client);
        client
            .write_all(&vec![0xff; MAX_FRAME * 2 + 3])
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        client.shutdown().await.unwrap();
        assert!(client.write_all(b"late").await.is_err());
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert_eq!(response, "response\n");
        task.await.unwrap();
    }
}
