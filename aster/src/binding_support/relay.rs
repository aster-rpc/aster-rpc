//! Consumer-side streaming HTTP relay for bindings (`expose`).
//!
//! [`crate::expose::relay_request_streaming`] is generic over the request
//! body and hands back an `http_body::Body`; neither is something foreign
//! code can hold. [`relay_streaming`] wraps it in concrete handles:
//!
//! - [`RelayUpload`]: the request body. The binding pushes chunks with
//!   backpressure, then [`finish`](RelayUpload::finish)es or
//!   [`abort`](RelayUpload::abort)s. Dropping every clone without finishing
//!   **aborts** the upload, so an abandoned writer resets the request stream
//!   and the handler sees a failed body, never a truncated one.
//! - [`RelayReader`]: the response body. Pull-based reads of at most
//!   `max_chunk` bytes; [`close`](RelayReader::close) (or dropping it) stops
//!   the response stream, which the handler sees as a cancelled write.
//! - [`RelayResponse`]: the exchange itself, already running on the
//!   runtime: [`wait`](RelayResponse::wait) for the head and the reader, or
//!   [`cancel`](RelayResponse::cancel) it. Dropping every clone of it (or a
//!   `wait` future holding the last one) before the head is in cancels the
//!   exchange and resets the request.
//!
//! The exchange is **full duplex**: the response head can arrive while the
//! upload is still going (an early `413`, or a handler streaming back as it
//! reads), and the upload carries on after it. The upload ends when the
//! binding finishes or aborts it, when the handler stops reading (writes
//! then fail [`BodyError::Cancelled`]), or when the response body is
//! dropped or [`close`](RelayReader::close)d *before its end* -- which
//! abandons the exchange and resets an unfinished upload. A response that
//! ends cleanly does not cancel the upload. An upload that fails before the
//! head fails [`wait`](RelayResponse::wait) ([`ResponseError::UploadFailed`]);
//! after the head it only resets the request.
//!
//! Body errors are [`BodyError`]s, sharing the server side's stable codes: a
//! failed (reset) response reads as [`BodyError::Read`] on every later read
//! too, never as a clean end; a call after [`RelayReader::close`] or on an
//! upload whose request side is gone is [`BodyError::Cancelled`]; a call
//! after `finish` / `abort` is [`BodyError::Finished`]. The exchange itself
//! fails with a [`ResponseError`], whose [`code`](ResponseError::code)s
//! continue the same numbering (the C ABI returns both from one enum).
//!
//! # Blocking and polling (C ABI)
//!
//! Besides the async calls, every handle has a blocking variant with a
//! timeout ([`RelayUpload::write_blocking`], [`RelayReader::read_blocking`],
//! [`RelayResponse::wait_blocking`]) for a binding's own threads -- never a
//! tokio runtime thread ([`BodyError::BlockingOnRuntime`]) -- and a
//! non-blocking variant plus an async readiness future for poll loops
//! ([`RelayUpload::try_write`] / [`RelayUpload::ready`],
//! [`RelayReader::try_read`] / [`RelayReader::ready`],
//! [`RelayResponse::try_wait`] / [`RelayResponse::ready`]). A timed-out
//! blocking call consumed or queued nothing.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::Body;
use tokio::sync::Notify;

use super::upcall::codec::HttpHeader;
pub use super::upcall::expose::BodyError;
use crate::expose::{body_channel, http, BodySender, RelayBody, RelayFailure, RelayFailureKind};
use crate::{Connection, Error, Result};

/// Chunking and buffering for one [`relay_streaming`] exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayOptions {
    /// Largest chunk a read returns or a write queues as one piece (bigger
    /// writes are split). Default 1 MiB.
    pub max_chunk: usize,
    /// Upload chunks that may wait unsent before a write blocks. Default 8.
    pub upload_capacity: usize,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            max_chunk: 1 << 20,
            upload_capacity: 8,
        }
    }
}

/// The response head: status and headers (names lowercase).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayHead {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
}

/// Why a [`RelayResponse`] has no head. [`code`](Self::code) is stable and
/// shares [`BodyError`]'s numbering: `2`, `6` and `9` mean what they mean
/// there, `10..=16` are relay-only. The message is the full error chain.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResponseError {
    /// [`cancel`](RelayResponse::cancel)led or abandoned before the head.
    #[error("relay exchange cancelled")]
    Cancelled,
    /// A [`wait_blocking`](RelayResponse::wait_blocking) timed out; the
    /// exchange is still running (not sticky).
    #[error("timed out waiting for the response head")]
    Timeout,
    /// A blocking call was made on a tokio runtime thread.
    #[error("blocking relay call on a runtime thread")]
    BlockingOnRuntime,
    /// The peer has no exposed service of this id.
    #[error("{0}")]
    UnknownService(String),
    /// The service's admission policy refused this peer (the reserved
    /// `PERMISSION_DENIED` of the C ABI).
    #[error("{0}")]
    Denied(String),
    /// The service is at its concurrent-stream cap; retryable.
    #[error("{0}")]
    Overloaded(String),
    /// The peer reset or ended the stream without a response head (the
    /// handler or relay failed).
    #[error("{0}")]
    Reset(String),
    /// The response head was malformed or too large.
    #[error("{0}")]
    BadHead(String),
    /// The connection is closed or was lost.
    #[error("{0}")]
    ConnectionClosed(String),
    /// The caller's own upload failed (aborted, or dropped unfinished)
    /// before the head.
    #[error("{0}")]
    UploadFailed(String),
}

impl ResponseError {
    /// The stable ABI code (C ABI status mapping).
    pub fn code(&self) -> u32 {
        match self {
            ResponseError::Cancelled => BodyError::Cancelled.code(),
            ResponseError::Timeout => BodyError::Timeout.code(),
            ResponseError::BlockingOnRuntime => BodyError::BlockingOnRuntime.code(),
            ResponseError::UnknownService(_) => 10,
            ResponseError::Denied(_) => 11,
            ResponseError::Overloaded(_) => 12,
            ResponseError::Reset(_) => 13,
            ResponseError::BadHead(_) => 14,
            ResponseError::ConnectionClosed(_) => 15,
            ResponseError::UploadFailed(_) => 16,
        }
    }

    /// Classify a failed consumer relay (also the error of the buffered
    /// `expose::relay_request`).
    pub fn classify(e: &Error) -> Self {
        let msg = format!("{e:#}");
        let kind = match e {
            Error::Transport(a) => a.downcast_ref::<RelayFailure>().map(RelayFailure::kind),
            Error::Connection(_) => Some(RelayFailureKind::ConnectionClosed),
            _ => None,
        };
        match kind {
            Some(RelayFailureKind::UnknownService) => ResponseError::UnknownService(msg),
            Some(RelayFailureKind::Denied) => ResponseError::Denied(msg),
            Some(RelayFailureKind::Overloaded) => ResponseError::Overloaded(msg),
            Some(RelayFailureKind::BadHead) => ResponseError::BadHead(msg),
            Some(RelayFailureKind::ConnectionClosed) => ResponseError::ConnectionClosed(msg),
            Some(RelayFailureKind::UploadFailed) => ResponseError::UploadFailed(msg),
            _ => ResponseError::Reset(msg),
        }
    }
}

impl From<ResponseError> for Error {
    fn from(e: ResponseError) -> Self {
        match e {
            ResponseError::Cancelled | ResponseError::ConnectionClosed(_) => {
                Error::Connection(e.to_string())
            }
            other => Error::Transport(anyhow::Error::new(other)),
        }
    }
}

/// Start one streaming HTTP exchange with `service_id` on `conn`, spawned
/// on `runtime` at once. Returns the upload handle and the
/// [`RelayResponse`]: drive the upload (write, then finish) while waiting
/// on the response. An invalid method, URI, header or service id is
/// [`Error::InvalidArgument`] and nothing is sent.
///
/// [`RelayResponse::wait`] can resolve before the upload is done; keep
/// driving the upload after it.
pub fn relay_streaming(
    runtime: &tokio::runtime::Handle,
    conn: &Connection,
    service_id: &str,
    method: &str,
    uri: &str,
    headers: &[HttpHeader],
    options: RelayOptions,
) -> Result<(RelayUpload, RelayResponse)> {
    if service_id.len() > usize::from(u16::MAX) {
        return Err(Error::InvalidArgument(format!(
            "service_id too long ({} bytes, max {})",
            service_id.len(),
            u16::MAX
        )));
    }
    let max_chunk = options.max_chunk.max(1);
    let (tx, body) = body_channel(options.upload_capacity.max(1));
    let mut builder = http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }
    let request = builder
        .body(body)
        .map_err(|e| Error::InvalidArgument(format!("invalid HTTP request: {e}")))?;
    let upload = RelayUpload {
        inner: Arc::new(UploadInner {
            tx: Mutex::new(Some(tx)),
            ended: Signal::default(),
            max_chunk,
            runtime: runtime.clone(),
        }),
    };
    let slot = Arc::new(Slot::default());
    let conn = conn.clone();
    let service_id = service_id.to_owned();
    let task_slot = slot.clone();
    let task = runtime.spawn(async move {
        // Settles the slot if this task is aborted or panics before it does.
        let guard = SettleOnDrop(task_slot);
        let res = crate::expose::relay_request_streaming(&conn, &service_id, request).await;
        let outcome = match res {
            Ok(resp) => {
                let (parts, body) = resp.into_parts();
                let head = RelayHead {
                    status: parts.status.as_u16(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
                        .collect(),
                };
                Ok((head, RelayReader::new(body, max_chunk)))
            }
            Err(e) => Err(ResponseError::classify(&e)),
        };
        guard.0.settle(outcome);
    });
    Ok((
        upload,
        RelayResponse {
            inner: Arc::new(ResponseInner {
                slot,
                task,
                runtime: runtime.clone(),
            }),
        },
    ))
}

/// Refuse a blocking call on a runtime thread (it would stall the runtime,
/// or panic in `block_on`).
fn not_on_runtime() -> bool {
    tokio::runtime::Handle::try_current().is_err()
}

// ── response ────────────────────────────────────────────────────────────────

/// A set-once flag with async waiters.
#[derive(Default)]
struct Signal {
    flag: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl Signal {
    fn set(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

type Outcome = std::result::Result<(RelayHead, RelayReader), ResponseError>;

/// Where the exchange task leaves its outcome: set once (first writer wins).
#[derive(Default)]
struct Slot {
    outcome: Mutex<Option<Outcome>>,
    done: Signal,
}

impl Slot {
    /// Store `outcome` unless one is already there; `true` if stored.
    fn settle(&self, outcome: Outcome) -> bool {
        let mut slot = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(outcome);
        drop(slot);
        self.done.set();
        true
    }

    fn get(&self) -> Option<Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

struct SettleOnDrop(Arc<Slot>);

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        let outcome = if std::thread::panicking() {
            ResponseError::Reset("relay task failed".into())
        } else {
            ResponseError::Cancelled
        };
        self.0.settle(Err(outcome));
    }
}

struct ResponseInner {
    slot: Arc<Slot>,
    task: tokio::task::JoinHandle<()>,
    runtime: tokio::runtime::Handle,
}

impl Drop for ResponseInner {
    fn drop(&mut self) {
        // Before the head this cancels the exchange; after it, a no-op.
        self.task.abort();
    }
}

/// A running [`relay_streaming`] exchange. Clone-cheap; every call works
/// from any thread. Dropping the last clone before the head is in cancels
/// the exchange (the request is reset). After the head, the response body
/// is the [`RelayReader`] in the outcome; this handle keeps one clone of it,
/// so drop it (or [`close`](Self::close)) when done.
#[derive(Clone)]
pub struct RelayResponse {
    inner: Arc<ResponseInner>,
}

impl std::fmt::Debug for RelayResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayResponse")
            .field("settled", &self.inner.slot.done.is_set())
            .finish()
    }
}

impl RelayResponse {
    /// The response head and its body reader. Resolves as soon as the head
    /// is in, whether or not the upload has finished; an exchange that
    /// failed (refused, unknown service, upload aborted, connection lost) or
    /// was [`cancel`](Self::cancel)led is an `Err` (a [`ResponseError`]
    /// inside: `Cancelled` / `ConnectionClosed` as [`Error::Connection`],
    /// the rest as [`Error::Transport`]). Dropping this future drops this
    /// handle, which cancels the exchange if it was the last clone.
    pub async fn wait(self) -> Result<(RelayHead, RelayReader)> {
        self.ready().await;
        self.outcome().map_err(Error::from)
    }

    fn outcome(&self) -> Outcome {
        self.inner
            .slot
            .get()
            .unwrap_or(Err(ResponseError::Cancelled))
    }

    /// Resolves once the outcome is in ([`try_wait`](Self::try_wait) is
    /// then `Some`).
    pub async fn ready(&self) {
        self.inner.slot.done.wait().await
    }

    /// The outcome if it is in, without waiting. Sticky: every call after
    /// the head returns the same head and (a clone of) the same reader.
    pub fn try_wait(&self) -> Option<Outcome> {
        self.inner.slot.get()
    }

    /// [`try_wait`](Self::try_wait), blocking the calling thread for up to
    /// `timeout` ([`ResponseError::Timeout`]: the exchange keeps running).
    /// Never call it on a runtime thread.
    pub fn wait_blocking(&self, timeout: Duration) -> Outcome {
        if let Some(outcome) = self.try_wait() {
            return outcome;
        }
        if !not_on_runtime() {
            return Err(ResponseError::BlockingOnRuntime);
        }
        let slot = &self.inner.slot;
        let done = self
            .inner
            .runtime
            .block_on(async { tokio::time::timeout(timeout, slot.done.wait()).await });
        match done {
            Ok(()) => self.outcome(),
            Err(_) => Err(ResponseError::Timeout),
        }
    }

    /// Cancel the exchange: the request is reset and the outcome becomes
    /// [`ResponseError::Cancelled`]. No effect once the head is in.
    pub fn cancel(&self) {
        self.inner.task.abort();
        self.inner.slot.settle(Err(ResponseError::Cancelled));
    }

    /// Abandon the exchange whatever its state: [`cancel`](Self::cancel)
    /// before the head, [`RelayReader::close`] after it (an unfinished
    /// upload is then reset unless the response already ended cleanly).
    pub fn close(&self) {
        self.cancel();
        if let Some(Ok((_, reader))) = self.try_wait() {
            reader.close();
        }
    }
}

// ── upload ──────────────────────────────────────────────────────────────────

struct UploadInner {
    tx: Mutex<Option<BodySender>>,
    /// Set by `finish` / `abort`: parked and split writes stop queuing.
    ended: Signal,
    max_chunk: usize,
    runtime: tokio::runtime::Handle,
}

impl Drop for UploadInner {
    fn drop(&mut self) {
        let tx = self.tx.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(tx) = tx {
            tx.abort("request body dropped without finish");
        }
    }
}

/// The request body of a [`relay_streaming`] exchange. Clone-cheap; every
/// call works from any thread.
#[derive(Clone)]
pub struct RelayUpload {
    inner: Arc<UploadInner>,
}

impl std::fmt::Debug for RelayUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayUpload")
            .field("open", &self.sender().is_ok())
            .finish()
    }
}

impl RelayUpload {
    fn sender(&self) -> std::result::Result<BodySender, BodyError> {
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(BodyError::Finished)
    }

    fn take(&self) -> std::result::Result<BodySender, BodyError> {
        let tx = self
            .inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // After the take: a writer that wakes on `ended` cannot re-read a
        // sender that is still there.
        self.inner.ended.set();
        tx.ok_or(BodyError::Finished)
    }

    /// The largest piece one write queues.
    pub fn max_chunk(&self) -> usize {
        self.inner.max_chunk
    }

    /// Queue `chunk`, split into `max_chunk` pieces, waiting while the
    /// buffer is full (backpressure). [`BodyError::Cancelled`]: the request
    /// side is gone (the relay failed or was dropped); stop producing.
    /// `finish` / `abort` from another thread wakes a parked write, which
    /// then returns [`BodyError::Finished`] without queuing the rest. A
    /// write cancelled partway may have queued a prefix; abort after it.
    pub async fn write(&self, chunk: Bytes) -> std::result::Result<(), BodyError> {
        let tx = self.sender()?;
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.inner.ended.is_set() {
                return Err(BodyError::Finished);
            }
            let piece = rest.split_to(self.inner.max_chunk.min(rest.len()));
            tokio::select! {
                biased;
                () = self.inner.ended.wait() => return Err(BodyError::Finished),
                r = tx.send(piece) => r.map_err(|_| BodyError::Cancelled)?,
            }
        }
        if tx.is_closed() {
            return Err(BodyError::Cancelled);
        }
        Ok(())
    }

    /// Queue `chunk` (at most `max_chunk` bytes, else
    /// [`BodyError::ChunkTooLarge`]), blocking the calling thread for up to
    /// `timeout` while the buffer is full. A timed-out write
    /// ([`BodyError::Timeout`]) queued nothing. Never call it on a runtime
    /// thread ([`BodyError::BlockingOnRuntime`]).
    pub fn write_blocking(
        &self,
        chunk: Bytes,
        timeout: Duration,
    ) -> std::result::Result<(), BodyError> {
        let max = self.inner.max_chunk;
        if chunk.len() > max {
            return Err(BodyError::ChunkTooLarge {
                got: chunk.len(),
                max,
            });
        }
        if !not_on_runtime() {
            return Err(BodyError::BlockingOnRuntime);
        }
        // One piece: either queued whole or (timed out / cancelled) not at
        // all -- the channel send is cancel-safe.
        self.inner.runtime.block_on(async {
            tokio::time::timeout(timeout, self.write(chunk))
                .await
                .unwrap_or(Err(BodyError::Timeout))
        })
    }

    /// Queue `chunk` (at most `max_chunk` bytes) if there is room now:
    /// `Ok(false)` when the buffer is full (nothing queued).
    pub fn try_write(&self, chunk: Bytes) -> std::result::Result<bool, BodyError> {
        let max = self.inner.max_chunk;
        if chunk.len() > max {
            return Err(BodyError::ChunkTooLarge {
                got: chunk.len(),
                max,
            });
        }
        let tx = self.sender()?;
        if tx.is_closed() {
            return Err(BodyError::Cancelled);
        }
        tx.try_send(chunk).map_err(|_| BodyError::Cancelled)
    }

    /// Resolves when a [`try_write`](Self::try_write) could queue a chunk
    /// (`Ok`), or with the error any write would now return
    /// ([`BodyError::Finished`] after `finish` / `abort`,
    /// [`BodyError::Cancelled`] when the request side is gone). Readiness
    /// only: a concurrent writer may take the room first.
    pub async fn ready(&self) -> std::result::Result<(), BodyError> {
        let tx = self.sender()?;
        if tx.is_closed() {
            return Err(BodyError::Cancelled);
        }
        tokio::select! {
            biased;
            () = self.inner.ended.wait() => Err(BodyError::Finished),
            r = tx.writable() => r.map_err(|_| BodyError::Cancelled),
        }
    }

    /// End the request body cleanly; queued chunks are still sent.
    pub fn finish(&self) -> std::result::Result<(), BodyError> {
        self.take()?.finish();
        Ok(())
    }

    /// End the request body with an error: queued chunks are discarded and
    /// the request stream is reset, so the handler sees a failed body.
    /// `message` stays local.
    pub fn abort(&self, message: &str) -> std::result::Result<(), BodyError> {
        self.take()?.abort(message);
        Ok(())
    }

    /// Whether the upload is over: finished, aborted, or its request side
    /// gone (writes would fail).
    pub fn is_closed(&self) -> bool {
        self.sender().map(|tx| tx.is_closed()).unwrap_or(true)
    }
}

// ── response reader ─────────────────────────────────────────────────────────

/// The result of a non-blocking [`RelayReader::try_read`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TryRead {
    /// A chunk (never empty).
    Data(Bytes),
    /// The clean end of the response body.
    End,
    /// Nothing ready yet (or another read holds the reader); wait on
    /// [`RelayReader::ready`] and retry.
    Pending,
}

struct ReaderState {
    body: Option<RelayBody>,
    pending: Bytes,
    /// Sticky failure: every read after a failed one fails the same way.
    failed: Option<BodyError>,
}

struct ReaderInner {
    state: tokio::sync::Mutex<ReaderState>,
    closed: Signal,
    max_chunk: usize,
    runtime: tokio::runtime::Handle,
}

impl ReaderInner {
    /// Make progress until there is data in `pending` (`Ready(Ok(true))`),
    /// the body ended (`Ready(Ok(false))`) or failed, or the wire has
    /// nothing yet (`Pending`, with `cx` registered).
    fn poll_fill(
        &self,
        st: &mut ReaderState,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<bool, BodyError>> {
        loop {
            if self.closed.is_set() {
                st.body = None;
                return Poll::Ready(Err(BodyError::Cancelled));
            }
            if let Some(e) = &st.failed {
                return Poll::Ready(Err(e.clone()));
            }
            if !st.pending.is_empty() {
                return Poll::Ready(Ok(true));
            }
            let Some(body) = st.body.as_mut() else {
                return Poll::Ready(Ok(false));
            };
            match std::pin::Pin::new(body).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => st.body = None,
                Poll::Ready(Some(Err(e))) => {
                    st.body = None;
                    st.failed = Some(BodyError::Read(e.to_string()));
                }
                // The relay hop carries no trailers yet; skip any.
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        st.pending = data;
                    }
                }
            }
        }
    }

    /// [`poll_fill`](Self::poll_fill) to completion; a `close` wakes it.
    async fn fill(&self, st: &mut ReaderState) -> std::result::Result<bool, BodyError> {
        let filled = tokio::select! {
            r = std::future::poll_fn(|cx| self.poll_fill(st, cx)) => Some(r),
            () = self.closed.wait() => None,
        };
        match filled {
            Some(r) => r,
            None => {
                st.body = None;
                Err(BodyError::Cancelled)
            }
        }
    }

    fn take(st: &mut ReaderState, max: usize) -> Bytes {
        let n = max.min(st.pending.len());
        st.pending.split_to(n)
    }
}

/// The response body of a [`relay_streaming`] exchange. Clone-cheap; every
/// call works from any thread. Dropping the last clone closes it.
#[derive(Clone)]
pub struct RelayReader {
    inner: Arc<ReaderInner>,
}

impl std::fmt::Debug for RelayReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayReader")
            .field("closed", &self.inner.closed.is_set())
            .finish()
    }
}

impl RelayReader {
    fn new(body: RelayBody, max_chunk: usize) -> Self {
        Self {
            inner: Arc::new(ReaderInner {
                state: tokio::sync::Mutex::new(ReaderState {
                    body: Some(body),
                    pending: Bytes::new(),
                    failed: None,
                }),
                closed: Signal::default(),
                max_chunk,
                runtime: tokio::runtime::Handle::current(),
            }),
        }
    }

    /// The largest chunk a read returns.
    pub fn max_chunk(&self) -> usize {
        self.inner.max_chunk
    }

    /// The next response-body chunk of at most `max` bytes (clamped to
    /// `1..=max_chunk`); `Ok(None)` is the clean end. Pull-based: nothing
    /// is read off the wire until asked. Concurrent reads are served one
    /// after the other. Cancel-safe: a dropped read consumed nothing.
    pub async fn read(&self, max: usize) -> std::result::Result<Option<Bytes>, BodyError> {
        let inner = &*self.inner;
        let max = max.clamp(1, inner.max_chunk);
        let mut guard = tokio::select! {
            g = inner.state.lock() => g,
            () = inner.closed.wait() => return Err(BodyError::Cancelled),
        };
        let st = &mut *guard;
        match inner.fill(st).await? {
            true => Ok(Some(ReaderInner::take(st, max))),
            false => Ok(None),
        }
    }

    /// [`read`](Self::read), blocking the calling thread for up to
    /// `timeout`. A timed-out read ([`BodyError::Timeout`]) consumed
    /// nothing. Never call it on a runtime thread
    /// ([`BodyError::BlockingOnRuntime`]).
    pub fn read_blocking(
        &self,
        max: usize,
        timeout: Duration,
    ) -> std::result::Result<Option<Bytes>, BodyError> {
        if !not_on_runtime() {
            return Err(BodyError::BlockingOnRuntime);
        }
        self.inner.runtime.block_on(async {
            tokio::time::timeout(timeout, self.read(max))
                .await
                .unwrap_or(Err(BodyError::Timeout))
        })
    }

    /// The next chunk if one is ready now, without waiting:
    /// [`TryRead::Pending`] when the wire has nothing yet or another read
    /// holds the reader. Errors as [`read`](Self::read). Any thread.
    pub fn try_read(&self, max: usize) -> std::result::Result<TryRead, BodyError> {
        let inner = &*self.inner;
        let max = max.clamp(1, inner.max_chunk);
        if inner.closed.is_set() {
            return Err(BodyError::Cancelled);
        }
        let Ok(mut guard) = inner.state.try_lock() else {
            return Ok(TryRead::Pending);
        };
        let st = &mut *guard;
        // Polled with a no-op waker: `ready` re-polls with a real one.
        let _enter = inner.runtime.enter();
        let mut cx = Context::from_waker(std::task::Waker::noop());
        match inner.poll_fill(st, &mut cx) {
            Poll::Pending => Ok(TryRead::Pending),
            Poll::Ready(Ok(true)) => Ok(TryRead::Data(ReaderInner::take(st, max))),
            Poll::Ready(Ok(false)) => Ok(TryRead::End),
            Poll::Ready(Err(e)) => Err(e),
        }
    }

    /// Resolves once a [`try_read`](Self::try_read) would not be
    /// [`TryRead::Pending`], pulling at most one frame off the wire into the
    /// reader: `Ok` for data or the clean end, else the error the read
    /// would fail with (`Cancelled` after `close`). Readiness only: a
    /// concurrent read may take the data first.
    pub async fn ready(&self) -> std::result::Result<(), BodyError> {
        let inner = &*self.inner;
        let mut guard = tokio::select! {
            g = inner.state.lock() => g,
            () = inner.closed.wait() => return Err(BodyError::Cancelled),
        };
        inner.fill(&mut guard).await.map(drop)
    }

    /// Stop reading: the response stream is dropped (the handler's further
    /// writes fail `Cancelled`), an unfinished upload is reset (unless the
    /// response already ended cleanly), and every later read fails
    /// `Cancelled`.
    /// Idempotent.
    pub fn close(&self) {
        self.inner.closed.set();
        match self.inner.state.try_lock() {
            Ok(mut st) => {
                st.body = None;
                st.pending = Bytes::new();
            }
            // A read holds the lock (it may be returning data it already
            // has): drop the body as soon as it lets go.
            Err(_) => {
                let inner = self.inner.clone();
                self.inner.runtime.spawn(async move {
                    let mut st = inner.state.lock().await;
                    st.body = None;
                    st.pending = Bytes::new();
                });
            }
        }
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_set()
    }
}
