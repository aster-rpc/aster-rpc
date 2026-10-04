//! Consumer-side streaming HTTP relay for bindings (`expose`).
//!
//! [`crate::expose::relay_request_streaming`] is generic over the request
//! body and hands back an `http_body::Body`; neither is something foreign
//! code can hold. [`relay_streaming`] wraps it in two concrete handles:
//!
//! - [`RelayUpload`]: the request body. The binding pushes chunks with
//!   backpressure, then [`finish`](RelayUpload::finish)es or
//!   [`abort`](RelayUpload::abort)s. Dropping every clone without finishing
//!   **aborts** the upload, so an abandoned writer resets the request stream
//!   and the handler sees a failed body, never a truncated one.
//! - [`RelayReader`]: the response body. Pull-based reads of at most
//!   `max_chunk` bytes; [`close`](RelayReader::close) (or dropping it) stops
//!   the response stream, which the handler sees as a cancelled write.
//!
//! - [`RelayResponse`]: the exchange itself, already running on the
//!   runtime: [`wait`](RelayResponse::wait) for the head and the reader, or
//!   [`cancel`](RelayResponse::cancel) it. Dropping it (or a `wait` future)
//!   before the head is in cancels the exchange and resets the request.
//!
//! The exchange is **full duplex**: the response head can arrive while the
//! upload is still going (an early `413`, or a handler streaming back as it
//! reads), and the upload carries on after it. The upload ends when the
//! binding finishes or aborts it, when the handler stops reading (writes
//! then fail [`BodyError::Cancelled`]), or when the response body is
//! dropped or [`close`](RelayReader::close)d *before its end* -- which
//! abandons the exchange and resets an unfinished upload. A response that
//! ends cleanly does not cancel the upload. An upload that fails before the head fails
//! [`wait`](RelayResponse::wait); after the head it only resets the request.
//!
//! Errors are [`BodyError`]s, sharing the server side's stable codes: a
//! failed (reset) response reads as [`BodyError::Read`] on every later read
//! too, never as a clean end; a call after [`RelayReader::close`] or on an
//! upload whose request side is gone is [`BodyError::Cancelled`]; a call
//! after `finish` / `abort` is [`BodyError::Finished`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::BodyExt;
use tokio::sync::Notify;

use super::upcall::codec::HttpHeader;
pub use super::upcall::expose::BodyError;
use crate::expose::{body_channel, http, BodySender, RelayBody};
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

/// Start one streaming HTTP exchange with `service_id` on `conn`, spawned
/// on `runtime` at once. Returns the upload handle and the
/// [`RelayResponse`]: drive the upload (write, then finish) while waiting
/// on the response. An invalid method, URI or header is
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
            ended: AtomicBool::new(false),
            max_chunk,
        }),
    };
    let conn = conn.clone();
    let service_id = service_id.to_owned();
    let task = runtime.spawn(async move {
        let resp = crate::expose::relay_request_streaming(&conn, &service_id, request).await?;
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
    });
    Ok((upload, RelayResponse { task: Some(task) }))
}

/// A running [`relay_streaming`] exchange, until its response head is in.
/// Dropping it before then cancels the exchange (the request is reset).
#[derive(Debug)]
pub struct RelayResponse {
    task: Option<tokio::task::JoinHandle<Result<(RelayHead, RelayReader)>>>,
}

impl RelayResponse {
    /// The response head and its body reader. Resolves as soon as the head
    /// is in, whether or not the upload has finished; an exchange that
    /// failed (refused,
    /// unknown service, upload aborted, connection lost) or was
    /// [`cancel`](Self::cancel)led is an `Err`. Dropping this future
    /// cancels the exchange.
    pub async fn wait(mut self) -> Result<(RelayHead, RelayReader)> {
        let joined = match self.task.as_mut() {
            Some(task) => task.await,
            None => return Err(Error::Connection("relay exchange cancelled".into())),
        };
        self.task = None;
        match joined {
            Ok(res) => res,
            Err(e) if e.is_cancelled() => Err(Error::Connection("relay exchange cancelled".into())),
            Err(e) => Err(Error::Transport(anyhow::anyhow!("relay task failed: {e}"))),
        }
    }

    /// Cancel the exchange: the request is reset and [`wait`](Self::wait)
    /// fails. No effect once the head is in.
    pub fn cancel(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Drop for RelayResponse {
    fn drop(&mut self) {
        self.cancel();
    }
}

// ── upload ──────────────────────────────────────────────────────────────────

struct UploadInner {
    tx: Mutex<Option<BodySender>>,
    /// Set by `finish` / `abort`: an in-flight split write stops queuing.
    ended: AtomicBool,
    max_chunk: usize,
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
        self.inner.ended.store(true, Ordering::Release);
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(BodyError::Finished)
    }

    /// The largest piece one write queues.
    pub fn max_chunk(&self) -> usize {
        self.inner.max_chunk
    }

    /// Queue `chunk`, split into `max_chunk` pieces, waiting while the
    /// buffer is full (backpressure). [`BodyError::Cancelled`]: the request
    /// side is gone (the relay failed or was dropped); stop producing. A
    /// write cancelled partway may have queued a prefix; abort after it.
    pub async fn write(&self, chunk: Bytes) -> std::result::Result<(), BodyError> {
        let tx = self.sender()?;
        let mut rest = chunk;
        while !rest.is_empty() {
            // `finish` / `abort` from another thread is a barrier: the
            // remaining pieces are not queued.
            if self.inner.ended.load(Ordering::Acquire) {
                return Err(BodyError::Finished);
            }
            let piece = rest.split_to(self.inner.max_chunk.min(rest.len()));
            tx.send(piece).await.map_err(|_| BodyError::Cancelled)?;
        }
        if tx.is_closed() {
            return Err(BodyError::Cancelled);
        }
        Ok(())
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

#[derive(Default)]
struct Closed {
    flag: AtomicBool,
    notify: Notify,
}

impl Closed {
    fn set(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
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

struct ReaderState {
    body: Option<RelayBody>,
    pending: Bytes,
    /// Sticky failure: every read after a failed one fails the same way.
    failed: Option<BodyError>,
}

struct ReaderInner {
    state: tokio::sync::Mutex<ReaderState>,
    closed: Closed,
    max_chunk: usize,
    runtime: tokio::runtime::Handle,
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
                closed: Closed::default(),
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
    /// after the other.
    pub async fn read(&self, max: usize) -> std::result::Result<Option<Bytes>, BodyError> {
        let inner = &*self.inner;
        let max = max.clamp(1, inner.max_chunk);
        let mut guard = tokio::select! {
            g = inner.state.lock() => g,
            () = inner.closed.wait() => return Err(BodyError::Cancelled),
        };
        let st = &mut *guard;
        loop {
            if inner.closed.is_set() {
                st.body = None;
                return Err(BodyError::Cancelled);
            }
            if let Some(e) = &st.failed {
                return Err(e.clone());
            }
            if !st.pending.is_empty() {
                let n = max.min(st.pending.len());
                return Ok(Some(st.pending.split_to(n)));
            }
            let Some(body) = st.body.as_mut() else {
                return Ok(None);
            };
            let frame = tokio::select! {
                f = body.frame() => f,
                () = inner.closed.wait() => {
                    st.body = None;
                    return Err(BodyError::Cancelled);
                }
            };
            match frame {
                None => st.body = None,
                Some(Err(e)) => {
                    st.body = None;
                    st.failed = Some(BodyError::Read(e.to_string()));
                }
                // The relay hop carries no trailers yet; skip any.
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        st.pending = data;
                    }
                }
            }
        }
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
