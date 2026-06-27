//! `AsterStreamIo` — adapt an Aster bidi stream pair to `tokio`'s
//! [`AsyncRead`] + [`AsyncWrite`], so hyper (or any tokio IO consumer) can
//! drive it directly. This is the "in-memory duplex" from the L7
//! terminate-and-relay path in `docs/_internal/working_ideas/aster-expose-http.md`
//! §5.1 — it is *not* a loopback socket; it has no kernel buffers and no
//! second TCP connection. Core stays hyper-free: this module only speaks
//! `tokio::io` traits over the existing [`CoreSendStream`]/[`CoreRecvStream`].
//!
//! The Aster stream methods are `async fn` (they lock an inner `Mutex` and
//! await QUIC), but `AsyncRead`/`AsyncWrite` are poll-based. We bridge by
//! holding the in-flight future for each direction and polling it. A small
//! trait seam ([`ChunkRead`]/[`ChunkWrite`]) lets the adapter be unit-tested
//! with in-memory mocks — the real streams (whose constructors require a live
//! QUIC connection) implement it for production.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{CoreRecvStream, CoreSendStream};

/// Boxed `Send` future returned by the [`ChunkRead`]/[`ChunkWrite`] seam.
/// Exported so out-of-crate types (companion crates, test mocks) can
/// implement the traits.
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Chunk-oriented read seam. Mirrors [`CoreRecvStream::read`].
pub trait ChunkRead: Send + 'static {
    /// Read up to `max_len` bytes. `Ok(None)` is clean EOF.
    fn read_chunk(&self, max_len: usize) -> BoxFut<Result<Option<Vec<u8>>>>;
}

/// Chunk-oriented write seam. Mirrors [`CoreSendStream::write_all`]/`finish`.
pub trait ChunkWrite: Send + 'static {
    fn write_chunk(&self, data: Vec<u8>) -> BoxFut<Result<()>>;
    fn finish(&self) -> BoxFut<Result<()>>;
}

impl ChunkRead for CoreRecvStream {
    fn read_chunk(&self, max_len: usize) -> BoxFut<Result<Option<Vec<u8>>>> {
        let s = self.clone();
        Box::pin(async move { s.read(max_len).await })
    }
}

impl ChunkWrite for CoreSendStream {
    fn write_chunk(&self, data: Vec<u8>) -> BoxFut<Result<()>> {
        let s = self.clone();
        Box::pin(async move { s.write_all(data).await })
    }

    fn finish(&self) -> BoxFut<Result<()>> {
        let s = self.clone();
        Box::pin(async move { s.finish().await })
    }
}

/// Default chunk size requested from the recv stream when the caller's
/// `ReadBuf` is smaller — any surplus is buffered in `leftover`.
const READ_CHUNK: usize = 16 * 1024;

fn other(e: anyhow::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// A `Send`-but-not-`Sync` in-flight future kept across polls. Wrapped in a
/// `std::sync::Mutex` purely to keep the adapter `Sync` (a `Mutex<T>` is `Sync`
/// for any `T: Send`); we only ever touch it through `&mut self` via
/// [`slot_mut`], so there is **no runtime locking** on the hot path.
type FutSlot<T> = std::sync::Mutex<Option<T>>;

fn slot_mut<T>(slot: &mut FutSlot<T>) -> &mut Option<T> {
    // We hold `&mut`, so the lock is uncontended and cannot be poisoned by us;
    // `into_inner` on a poisoned guard still yields the value.
    slot.get_mut().unwrap_or_else(|e| e.into_inner())
}

/// `AsyncRead` over a single Aster recv half. The Aster `read` is `async fn`
/// (locks a mutex and awaits QUIC) while `poll_read` is poll-based, so we hold
/// the in-flight future across polls and buffer any surplus from an oversized
/// chunk in `leftover`. `Unpin`, `Send + Sync` — works with the `AsyncReadExt`
/// combinators and across spawned tasks.
///
/// Use this when you have only the recv half — e.g. a uni-stream lane. Pair it
/// with [`AsterWrite`] (or use [`AsterStreamIo`]) for a bidi stream.
pub struct AsterRead<R = CoreRecvStream> {
    recv: R,
    read_fut: FutSlot<BoxFut<Result<Option<Vec<u8>>>>>,
    leftover: Vec<u8>,
}

impl<R: ChunkRead> AsterRead<R> {
    pub fn new(recv: R) -> Self {
        Self {
            recv,
            read_fut: FutSlot::new(None),
            leftover: Vec::new(),
        }
    }

    /// Borrow the underlying recv half (for its chunk-oriented async methods).
    /// Safe to mix with poll-based reads only while no poll-read is mid-chunk
    /// (the per-call `leftover` buffer lives here, not in the underlying half).
    pub fn get_ref(&self) -> &R {
        &self.recv
    }

    pub fn into_inner(self) -> R {
        self.recv
    }
}

impl<R: ChunkRead + Unpin> AsyncRead for AsterRead<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        // Serve buffered surplus from a prior chunk before touching the wire.
        if !this.leftover.is_empty() {
            let n = this.leftover.len().min(buf.remaining());
            buf.put_slice(&this.leftover[..n]);
            this.leftover.drain(..n);
            return Poll::Ready(Ok(()));
        }

        // Take the in-flight future out (or start one), poll it, put it back on
        // Pending — keeps the `read_fut` borrow disjoint from `recv`/`leftover`.
        let mut fut = slot_mut(&mut this.read_fut)
            .take()
            .unwrap_or_else(|| this.recv.read_chunk(READ_CHUNK.max(buf.remaining())));
        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                *slot_mut(&mut this.read_fut) = Some(fut);
                Poll::Pending
            }
            Poll::Ready(res) => match res {
                // Clean EOF: leave `buf` unfilled — tokio reads this as EOF.
                Ok(None) => Poll::Ready(Ok(())),
                Ok(Some(chunk)) if chunk.is_empty() => Poll::Ready(Ok(())),
                Ok(Some(chunk)) => {
                    let n = chunk.len().min(buf.remaining());
                    buf.put_slice(&chunk[..n]);
                    if n < chunk.len() {
                        this.leftover.extend_from_slice(&chunk[n..]);
                    }
                    Poll::Ready(Ok(()))
                }
                Err(e) => Poll::Ready(Err(other(e))),
            },
        }
    }
}

/// `AsyncWrite` over a single Aster send half. Holds the in-flight `write`/
/// `finish` future across polls (the underlying methods are `async fn`).
/// `poll_shutdown` finishes the send side. `Unpin`, `Send + Sync`.
///
/// Use this when you have only the send half — e.g. a uni-stream lane.
pub struct AsterWrite<W = CoreSendStream> {
    send: W,
    /// In-flight write: `(bytes_in_flight, future)`. The byte count is what
    /// `poll_write` reports consumed once the future resolves.
    write_fut: FutSlot<(usize, BoxFut<Result<()>>)>,
    /// In-flight `finish` future during shutdown.
    shutdown_fut: FutSlot<BoxFut<Result<()>>>,
}

impl<W: ChunkWrite> AsterWrite<W> {
    pub fn new(send: W) -> Self {
        Self {
            send,
            write_fut: FutSlot::new(None),
            shutdown_fut: FutSlot::new(None),
        }
    }

    /// Borrow the underlying send half (for its chunk-oriented async methods).
    pub fn get_ref(&self) -> &W {
        &self.send
    }

    pub fn into_inner(self) -> W {
        self.send
    }
}

impl<W: ChunkWrite + Unpin> AsyncWrite for AsterWrite<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Once a write is in flight we keep polling *that* future and ignore
        // the (by-contract identical) `data` until it resolves.
        let (len, mut fut) = slot_mut(&mut this.write_fut).take().unwrap_or_else(|| {
            let owned = data.to_vec();
            let len = owned.len();
            (len, this.send.write_chunk(owned))
        });
        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                *slot_mut(&mut this.write_fut) = Some((len, fut));
                Poll::Pending
            }
            Poll::Ready(Ok(())) => Poll::Ready(Ok(len)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(other(e))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // QUIC needs no explicit flush beyond completing the in-flight write;
        // `write_all` already hands bytes to the stream.
        drive_write(&mut self.get_mut().write_fut, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Flush any pending write before finishing the send side.
        match drive_write(&mut this.write_fut, cx) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        let mut fut = slot_mut(&mut this.shutdown_fut)
            .take()
            .unwrap_or_else(|| this.send.finish());
        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                *slot_mut(&mut this.shutdown_fut) = Some(fut);
                Poll::Pending
            }
            Poll::Ready(res) => Poll::Ready(res.map_err(other)),
        }
    }
}

/// Poll an in-flight write future to completion, clearing it on resolve.
fn drive_write(
    slot: &mut FutSlot<(usize, BoxFut<Result<()>>)>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let Some((len, mut fut)) = slot_mut(slot).take() else {
        return Poll::Ready(Ok(()));
    };
    match fut.as_mut().poll(cx) {
        Poll::Pending => {
            *slot_mut(slot) = Some((len, fut));
            Poll::Pending
        }
        Poll::Ready(res) => Poll::Ready(res.map_err(other)),
    }
}

/// `AsyncRead + AsyncWrite` over an Aster `(recv, send)` stream pair — a thin
/// composition of [`AsterRead`] + [`AsterWrite`].
///
/// Type params default to the real Aster streams; tests substitute mocks.
/// The struct is `Unpin` (all fields are), so it works with the
/// `AsyncReadExt`/`AsyncWriteExt` combinators and `hyper::serve_connection`.
pub struct AsterStreamIo<R = CoreRecvStream, W = CoreSendStream> {
    read: AsterRead<R>,
    write: AsterWrite<W>,
}

impl<R: ChunkRead, W: ChunkWrite> AsterStreamIo<R, W> {
    pub fn new(recv: R, send: W) -> Self {
        Self {
            read: AsterRead::new(recv),
            write: AsterWrite::new(send),
        }
    }

    /// Split into the independent read and write halves.
    pub fn into_halves(self) -> (AsterRead<R>, AsterWrite<W>) {
        (self.read, self.write)
    }
}

impl<R: ChunkRead + Unpin, W: ChunkWrite + Unpin> AsyncRead for AsterStreamIo<R, W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl<R: ChunkRead + Unpin, W: ChunkWrite + Unpin> AsyncWrite for AsterStreamIo<R, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The adapters must stay `Send + Sync` (real `CoreSendStream`/`CoreRecvStream`
    /// are both): consumers hold `&stream` across `.await` in spawned tasks, which
    /// requires `Sync`. Compile-time guard — see the `FutSlot` mutex wrapper.
    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AsterRead<CoreRecvStream>>();
        assert_send_sync::<AsterWrite<CoreSendStream>>();
        assert_send_sync::<AsterStreamIo>();
    };

    #[derive(Clone)]
    struct MockRead {
        chunks: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    }

    impl MockRead {
        fn new(chunks: Vec<&'static [u8]>) -> Self {
            Self {
                chunks: Arc::new(StdMutex::new(
                    chunks.into_iter().map(|c| c.to_vec()).collect(),
                )),
            }
        }
    }

    impl ChunkRead for MockRead {
        fn read_chunk(&self, max_len: usize) -> BoxFut<Result<Option<Vec<u8>>>> {
            let chunks = self.chunks.clone();
            Box::pin(async move {
                let mut q = chunks.lock().unwrap();
                match q.front_mut() {
                    None => Ok(None),
                    Some(front) if front.len() <= max_len => Ok(Some(q.pop_front().unwrap())),
                    Some(front) => {
                        let head = front[..max_len].to_vec();
                        front.drain(..max_len);
                        Ok(Some(head))
                    }
                }
            })
        }
    }

    #[derive(Clone, Default)]
    struct MockWrite {
        sink: Arc<StdMutex<Vec<u8>>>,
        finished: Arc<StdMutex<bool>>,
    }

    impl ChunkWrite for MockWrite {
        fn write_chunk(&self, data: Vec<u8>) -> BoxFut<Result<()>> {
            let sink = self.sink.clone();
            Box::pin(async move {
                sink.lock().unwrap().extend_from_slice(&data);
                Ok(())
            })
        }
        fn finish(&self) -> BoxFut<Result<()>> {
            let finished = self.finished.clone();
            Box::pin(async move {
                *finished.lock().unwrap() = true;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn reads_across_chunk_boundaries() {
        let io = AsterStreamIo::new(
            MockRead::new(vec![b"hello", b" ", b"world"]),
            MockWrite::default(),
        );
        let mut io = io;
        let mut out = Vec::new();
        io.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn leftover_drains_before_next_wire_read() {
        // One 6-byte chunk read through a 4-byte buffer: 4 delivered, 2 buffered.
        let mut io = AsterStreamIo::new(MockRead::new(vec![b"abcdef"]), MockWrite::default());
        let mut b = [0u8; 4];
        let n = io.read(&mut b).await.unwrap();
        assert_eq!(n, 4);
        assert_eq!(&b[..n], b"abcd");
        let n = io.read(&mut b).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(&b[..n], b"ef");
        // Then EOF.
        let n = io.read(&mut b).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn writes_concatenate_and_shutdown_finishes() {
        let w = MockWrite::default();
        let mut io = AsterStreamIo::new(MockRead::new(vec![]), w.clone());
        io.write_all(b"abc").await.unwrap();
        io.write_all(b"def").await.unwrap();
        io.flush().await.unwrap();
        io.shutdown().await.unwrap();
        assert_eq!(&*w.sink.lock().unwrap(), b"abcdef");
        assert!(*w.finished.lock().unwrap());
    }
}
