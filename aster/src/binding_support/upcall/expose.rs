//! expose implemented by foreign code (`expose`): the per-`(connection,
//! service)` [`AuthorizeFn`] (kind 48), an edge's [`RoutePolicy`] (kind 50)
//! and, R9, a bytes-level HTTP handler (kind 49) behind the ordinary
//! [`HttpHandler`] / `tower::Service` shape, so [`Expose::expose_http`] and
//! [`Expose::serve_routes`] take a binding's handler unchanged.
//!
//! **HTTP exchange.** Each request is **one** upcall: the request head (UCv1
//! `ExposeHttpRequest`, with the body inline when it is small and of known
//! length) goes out, the response head comes back through
//! [`Upcalls::respond`] (`ExposeHttpResponse`, body inline or streamed).
//! Bodies that stream do not travel over upcalls: every exchange gets a body
//! handle (`stream_id`) in an [`HttpBodies`] table, and the binding pulls the
//! request body and pushes the response body through it in chunks
//! ([`HttpBodies::read`] / [`write`](HttpBodies::write) /
//! [`finish`](HttpBodies::finish) / [`abort`](HttpBodies::abort), each with a
//! blocking variant for poll threads). No function pointers cross.
//!
//! **Backpressure and limits.** The request body is pulled: nothing is read
//! off the wire until the binding asks. The response body is a bounded queue
//! of [`ForeignHttpOptions::response_buffer`] chunks of at most
//! [`max_chunk`](ForeignHttpOptions::max_chunk) bytes; a write waits while it
//! is full. Optional caps on the request and response body sizes fail the
//! exchange instead of buffering (`413` up front when a declared
//! `content-length` is over the cap).
//!
//! **Cancellation.** The relay drops the handler when the consumer abandons
//! the request (stops reading the stream or closes the connection) before the
//! response head; the upcall is then cancelled like any abandoned waiter
//! (`Cancel{abandoned}`), and the exchange's body handle closes, so pending
//! reads and writes fail with [`BodyError::Cancelled`]. After the head, a
//! vanished peer surfaces at the next write. The kind's [`UpcallPolicy`]
//! timeout bounds the time to the response head (default 5 s; raise it for
//! long-polling handlers), not the body.
//!
//! [`UpcallPolicy`]: super::UpcallPolicy
//! [`Expose::expose_http`]: crate::expose::Expose::expose_http
//! [`Expose::serve_routes`]: crate::expose::Expose::serve_routes

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue, CONTENT_LENGTH};
use http::{HeaderMap, Method, StatusCode, Version};
use http_body::{Body, Frame};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use tokio::sync::Notify;

use super::codec::{
    self, ExposeHttpRequest, ExposeHttpResponse, ExposeRouteSpec, HttpBodyMode, HttpHeader,
};
use super::{UpcallError, UpcallKind, Upcalls};
use crate::expose::{
    body_channel, AuthenticatedPeer, AuthorizeFn, BodySender, BoxFut, ChannelBody, Expose,
    HttpHandler, PeerContext, RelayError, ResponseBody, RoutePolicy, RouteProtocol, RouteSpec,
};
use crate::log_targets::UPCALL;

// ── authorize ───────────────────────────────────────────────────────────────

/// Build an [`AuthorizeFn`] for `service_id` that asks foreign code (kind
/// [`UpcallKind::ExposeAuthorize`]): a `true` decision admits, `false`
/// refuses, and no answer applies the kind's failure mode (fail-closed by
/// default).
pub fn foreign_authorize_fn(upcalls: Upcalls, service_id: &str) -> AuthorizeFn {
    let service_id: Arc<str> = service_id.into();
    Arc::new(move |peer: PeerContext| {
        let upcalls = upcalls.clone();
        let service_id = service_id.clone();
        Box::pin(async move {
            let answer = upcalls
                .call(
                    UpcallKind::ExposeAuthorize,
                    codec::encode_expose_authorize_request(
                        &service_id,
                        &peer.peer_id,
                        &peer.metadata,
                    ),
                )
                .await;
            if upcalls.decide(UpcallKind::ExposeAuthorize, answer, codec::decode_decision) {
                Ok(())
            } else {
                anyhow::bail!(
                    "expose: peer {} not authorized for {service_id}",
                    peer.peer_id
                )
            }
        })
    })
}

// ── route policy ────────────────────────────────────────────────────────────

/// Build a [`RoutePolicy`] (for [`Expose::serve_routes`]) that asks foreign
/// code (kind [`UpcallKind::ExposeRoutePolicy`]) which of the requested
/// routes to grant. The answer lists indexes into the request; an index out
/// of range is a malformed answer. With no usable answer the failure mode
/// decides: fail-closed grants nothing, fail-open grants every route.
///
/// [`Expose::serve_routes`]: crate::expose::Expose::serve_routes
pub fn foreign_route_policy(upcalls: Upcalls) -> RoutePolicy {
    Arc::new(move |peer: PeerContext, specs: Vec<RouteSpec>| {
        let upcalls = upcalls.clone();
        Box::pin(async move {
            let wire: Vec<ExposeRouteSpec> = specs
                .iter()
                .map(|s| ExposeRouteSpec {
                    host: s.host.clone(),
                    port: s.port,
                    protocol: match s.protocol {
                        RouteProtocol::Http => 0,
                        RouteProtocol::Tcp => 1,
                    },
                    service_id: s.service_id.clone(),
                })
                .collect();
            let answer = upcalls
                .call(
                    UpcallKind::ExposeRoutePolicy,
                    codec::encode_expose_route_policy_request(&peer.peer_id, &peer.metadata, &wire),
                )
                .await
                .and_then(|bytes| {
                    let idx = codec::decode_expose_route_grant(&bytes)
                        .map_err(|e| UpcallError::Decode(e.to_string()))?;
                    match idx.iter().find(|i| **i as usize >= specs.len()) {
                        Some(bad) => Err(UpcallError::Decode(format!(
                            "granted route index {bad} out of range ({} requested)",
                            specs.len()
                        ))),
                        None => Ok(idx),
                    }
                });
            Ok(match answer {
                Ok(idx) => idx.into_iter().map(|i| specs[i as usize].clone()).collect(),
                Err(e) => {
                    let fallback = upcalls.policy(UpcallKind::ExposeRoutePolicy).fallback();
                    tracing::warn!(
                        target: UPCALL,
                        kind = %UpcallKind::ExposeRoutePolicy,
                        error = %e,
                        fallback,
                        "upcall decision unavailable; applying failure mode"
                    );
                    if fallback {
                        specs
                    } else {
                        Vec::new()
                    }
                }
            })
        })
    })
}

// ── body handles ────────────────────────────────────────────────────────────

/// Why a body operation failed. [`code`](Self::code) is the stable ABI code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    /// No open exchange has this id: never issued, or already over (the
    /// response was inline or finished and the relay is done with it).
    #[error("unknown or finished body stream {0}")]
    UnknownStream(u64),
    /// The exchange was cancelled: the peer went away or the request was
    /// abandoned.
    #[error("HTTP exchange cancelled")]
    Cancelled,
    /// A body went over its configured size cap. A request body over the
    /// cap stops reading; a response body over it is aborted.
    #[error("body exceeds the {0}-byte limit")]
    TooLarge(u64),
    /// One write was larger than [`ForeignHttpOptions::max_chunk`]; split it.
    #[error("chunk of {got} bytes exceeds the {max}-byte write limit")]
    ChunkTooLarge { got: usize, max: usize },
    /// The response body was already finished or aborted (or the response
    /// was inline / to a HEAD request).
    #[error("response body already finished")]
    Finished,
    /// A blocking call timed out; nothing was consumed or queued.
    #[error("body operation timed out")]
    Timeout,
    /// Reading the request body off the wire failed.
    #[error("request body read failed: {0}")]
    Read(String),
    /// Trailers were not valid HTTP header fields.
    #[error("invalid trailers: {0}")]
    InvalidTrailers(String),
    /// A blocking call was made on a tokio runtime thread.
    #[error("blocking body call on a runtime thread")]
    BlockingOnRuntime,
}

impl BodyError {
    /// The stable ABI code (C ABI status mapping).
    pub fn code(&self) -> u32 {
        match self {
            BodyError::UnknownStream(_) => 1,
            BodyError::Cancelled => 2,
            BodyError::TooLarge(_) => 3,
            BodyError::ChunkTooLarge { .. } => 4,
            BodyError::Finished => 5,
            BodyError::Timeout => 6,
            BodyError::Read(_) => 7,
            BodyError::InvalidTrailers(_) => 8,
            BodyError::BlockingOnRuntime => 9,
        }
    }
}

/// Size limits and buffering for a [`ForeignHttpHandler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForeignHttpOptions {
    /// A request whose `content-length` is at most this travels inline in
    /// the upcall (one round trip, no reads); larger or unknown-length
    /// bodies stream. Default 64 KiB.
    pub inline_request_max: usize,
    /// Cap on the request body (`None`: unbounded). A declared length over
    /// it is answered `413` without an upcall; a streamed body that grows
    /// past it fails the read with [`BodyError::TooLarge`]. Default `None`.
    pub max_request_body: Option<u64>,
    /// Cap on the response body, inline or streamed (`None`: unbounded).
    /// An inline body over it becomes a `502`; a streamed one is aborted.
    /// Default `None`.
    pub max_response_body: Option<u64>,
    /// Largest single body write, and the most one read returns. Default
    /// 1 MiB.
    pub max_chunk: usize,
    /// Response chunks queued before a write waits (backpressure). Memory
    /// per exchange is bounded by `response_buffer * max_chunk`. Default 8.
    pub response_buffer: usize,
}

impl Default for ForeignHttpOptions {
    fn default() -> Self {
        Self {
            inline_request_max: 64 * 1024,
            max_request_body: None,
            max_response_body: None,
            max_chunk: 1 << 20,
            response_buffer: 8,
        }
    }
}

/// Set once when the exchange ends; wakes body calls parked on it.
#[derive(Default)]
struct Gone {
    flag: AtomicBool,
    notify: Notify,
}

impl Gone {
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

type RequestBody = UnsyncBoxBody<Bytes, RelayError>;

struct RequestReader {
    body: Option<RequestBody>,
    pending: Bytes,
    total: u64,
}

struct Exchange {
    request: tokio::sync::Mutex<RequestReader>,
    response: Mutex<Option<BodySender>>,
    written: AtomicU64,
    options: ForeignHttpOptions,
    gone: Gone,
    runtime: tokio::runtime::Handle,
}

#[derive(Default)]
struct BodiesInner {
    exchanges: Mutex<HashMap<u64, Arc<Exchange>>>,
    next_id: AtomicU64,
}

/// The body-handle table for foreign HTTP exchanges: one per binding
/// runtime (clone-cheap), shared by every [`ForeignHttpHandler`]. The
/// binding reads request bodies and writes response bodies through it by
/// the `stream_id` of the `ExposeHttpRequest`. Every call works from any
/// thread; the `_blocking` variants are for poll threads (never a tokio
/// runtime thread).
///
/// Order of operations for a streamed response: [`Upcalls::respond`] with
/// body mode `Stream`, then [`write`](Self::write) chunks, then
/// [`finish`](Self::finish) (or [`abort`](Self::abort)). Writes made before
/// the respond are queued (up to the buffer) and sent after the head. Read
/// the request body before finishing the response: once the relay is done
/// with the exchange its handle is gone.
#[derive(Clone, Default)]
pub struct HttpBodies {
    inner: Arc<BodiesInner>,
}

impl std::fmt::Debug for HttpBodies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpBodies")
            .field("open", &self.open_count())
            .finish()
    }
}

/// Closes an exchange when the Rust side is done with it (response sent,
/// handler dropped, peer gone).
struct ExchangeGuard {
    bodies: HttpBodies,
    id: u64,
}

impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        self.bodies.close(self.id);
    }
}

impl HttpBodies {
    pub fn new() -> Self {
        Self::default()
    }

    /// Exchanges currently open.
    pub fn open_count(&self) -> usize {
        self.inner.exchanges.lock().unwrap().len()
    }

    fn open(
        &self,
        body: Option<RequestBody>,
        response: BodySender,
        options: ForeignHttpOptions,
    ) -> (u64, ExchangeGuard) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let exchange = Arc::new(Exchange {
            request: tokio::sync::Mutex::new(RequestReader {
                body,
                pending: Bytes::new(),
                total: 0,
            }),
            response: Mutex::new(Some(response)),
            written: AtomicU64::new(0),
            options,
            gone: Gone::default(),
            runtime: tokio::runtime::Handle::current(),
        });
        self.inner.exchanges.lock().unwrap().insert(id, exchange);
        (
            id,
            ExchangeGuard {
                bodies: self.clone(),
                id,
            },
        )
    }

    fn close(&self, id: u64) {
        let removed = self.inner.exchanges.lock().unwrap().remove(&id);
        if let Some(ex) = removed {
            ex.gone.set();
            ex.response.lock().unwrap().take();
        }
    }

    fn get(&self, id: u64) -> Result<Arc<Exchange>, BodyError> {
        self.inner
            .exchanges
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(BodyError::UnknownStream(id))
    }

    /// Read the next request-body chunk of at most `max` bytes (clamped to
    /// `1..=max_chunk`). `Ok(None)` is the end of the body. Pull-based:
    /// nothing is read off the wire until asked. Concurrent reads on one
    /// stream are served one after the other.
    pub async fn read(&self, stream_id: u64, max: usize) -> Result<Option<Vec<u8>>, BodyError> {
        let ex = self.get(stream_id)?;
        let max = max.clamp(1, ex.options.max_chunk.max(1));
        let mut guard = tokio::select! {
            g = ex.request.lock() => g,
            () = ex.gone.wait() => return Err(BodyError::Cancelled),
        };
        let rd = &mut *guard;
        loop {
            if ex.gone.is_set() {
                return Err(BodyError::Cancelled);
            }
            if !rd.pending.is_empty() {
                let n = max.min(rd.pending.len());
                return Ok(Some(rd.pending.split_to(n).to_vec()));
            }
            let Some(body) = rd.body.as_mut() else {
                return Ok(None);
            };
            let frame = tokio::select! {
                f = body.frame() => f,
                () = ex.gone.wait() => return Err(BodyError::Cancelled),
            };
            match frame {
                None => {
                    rd.body = None;
                    return Ok(None);
                }
                Some(Err(e)) => {
                    rd.body = None;
                    return Err(BodyError::Read(e.to_string()));
                }
                Some(Ok(frame)) => {
                    // Request trailers are not carried by the relay; skip any.
                    if let Ok(data) = frame.into_data() {
                        rd.total += data.len() as u64;
                        if let Some(limit) = ex.options.max_request_body {
                            if rd.total > limit {
                                rd.body = None;
                                return Err(BodyError::TooLarge(limit));
                            }
                        }
                        rd.pending = data;
                    }
                }
            }
        }
    }

    /// [`read`](Self::read), blocking the calling thread for up to
    /// `timeout`. A timed-out read consumes nothing.
    pub fn read_blocking(
        &self,
        stream_id: u64,
        max: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, BodyError> {
        let rt = self.blocking_runtime(stream_id)?;
        rt.block_on(async {
            tokio::time::timeout(timeout, self.read(stream_id, max))
                .await
                .unwrap_or(Err(BodyError::Timeout))
        })
    }

    fn blocking_runtime(&self, stream_id: u64) -> Result<tokio::runtime::Handle, BodyError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(BodyError::BlockingOnRuntime);
        }
        Ok(self.get(stream_id)?.runtime.clone())
    }

    /// Account a write of `len` bytes and hand back the sender.
    fn writer(&self, stream_id: u64, len: usize) -> Result<(Arc<Exchange>, BodySender), BodyError> {
        let ex = self.get(stream_id)?;
        let max = ex.options.max_chunk;
        if len > max {
            return Err(BodyError::ChunkTooLarge { got: len, max });
        }
        let tx = {
            let mut slot = ex.response.lock().unwrap();
            let tx = slot.as_ref().ok_or(BodyError::Finished)?.clone();
            let total = ex.written.fetch_add(len as u64, Ordering::AcqRel) + len as u64;
            if let Some(limit) = ex.options.max_response_body {
                if total > limit {
                    if let Some(tx) = slot.take() {
                        tx.abort(format!("response body exceeds {limit} bytes"));
                    }
                    return Err(BodyError::TooLarge(limit));
                }
            }
            tx
        };
        if tx.is_closed() {
            return Err(BodyError::Cancelled);
        }
        Ok((ex, tx))
    }

    /// Queue one response-body chunk (at most `max_chunk` bytes), waiting
    /// while the buffer is full. [`BodyError::Cancelled`] means the peer is
    /// gone: stop producing.
    pub async fn write(&self, stream_id: u64, chunk: Vec<u8>) -> Result<(), BodyError> {
        let (ex, tx) = self.writer(stream_id, chunk.len())?;
        tokio::select! {
            r = tx.send(Bytes::from(chunk)) => r.map_err(|_| BodyError::Cancelled),
            () = ex.gone.wait() => Err(BodyError::Cancelled),
        }
    }

    /// Queue one chunk if there is room now: `Ok(false)` when the buffer is
    /// full (nothing queued; try again later).
    pub fn try_write(&self, stream_id: u64, chunk: Vec<u8>) -> Result<bool, BodyError> {
        let len = chunk.len();
        let (ex, tx) = self.writer(stream_id, len)?;
        match tx.try_send(Bytes::from(chunk)) {
            Ok(true) => Ok(true),
            Ok(false) => {
                ex.written.fetch_sub(len as u64, Ordering::AcqRel);
                Ok(false)
            }
            Err(_) => Err(BodyError::Cancelled),
        }
    }

    /// [`write`](Self::write), blocking the calling thread for up to
    /// `timeout`. A timed-out write queued nothing.
    pub fn write_blocking(
        &self,
        stream_id: u64,
        chunk: Vec<u8>,
        timeout: Duration,
    ) -> Result<(), BodyError> {
        let rt = self.blocking_runtime(stream_id)?;
        let len = chunk.len() as u64;
        let res = rt.block_on(async {
            tokio::time::timeout(timeout, self.write(stream_id, chunk))
                .await
                .unwrap_or(Err(BodyError::Timeout))
        });
        if res == Err(BodyError::Timeout) {
            if let Ok(ex) = self.get(stream_id) {
                ex.written.fetch_sub(len, Ordering::AcqRel);
            }
        }
        res
    }

    /// End the response body cleanly, optionally with trailers (dropped by
    /// the relay hop until it carries them -- design V2 -- but visible to an
    /// in-process consumer). Queued chunks are still delivered.
    pub fn finish(&self, stream_id: u64, trailers: Vec<HttpHeader>) -> Result<(), BodyError> {
        let ex = self.get(stream_id)?;
        let trailers = if trailers.is_empty() {
            None
        } else {
            Some(header_map(&trailers).map_err(BodyError::InvalidTrailers)?)
        };
        let tx = ex
            .response
            .lock()
            .unwrap()
            .take()
            .ok_or(BodyError::Finished)?;
        match trailers {
            Some(t) => tx.finish_with_trailers(t),
            None => tx.finish(),
        }
        Ok(())
    }

    /// End the response body with an error: queued chunks are discarded and
    /// the relay resets the stream, so the consumer sees a failed response,
    /// never a silently truncated one. `message` is logged, not sent.
    pub fn abort(&self, stream_id: u64, message: &str) -> Result<(), BodyError> {
        let ex = self.get(stream_id)?;
        let tx = ex
            .response
            .lock()
            .unwrap()
            .take()
            .ok_or(BodyError::Finished)?;
        tx.abort(message);
        Ok(())
    }
}

/// A streamed response body that closes its exchange when dropped.
struct ForeignBody {
    inner: ChannelBody,
    _guard: ExchangeGuard,
}

impl Body for ForeignBody {
    type Data = Bytes;
    type Error = RelayError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, RelayError>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

// ── the handler ─────────────────────────────────────────────────────────────

struct HandlerInner {
    upcalls: Upcalls,
    bodies: HttpBodies,
    service_id: String,
    options: ForeignHttpOptions,
}

/// An HTTP handler implemented by foreign code (kind
/// [`UpcallKind::ExposeHttp`]). Implements `tower::Service` and converts to
/// an [`HttpHandler`] ([`handler`](Self::handler)), so it plugs into
/// [`Expose::expose_http`](crate::expose::Expose::expose_http) like any Rust
/// handler. Never fails at the HTTP level: a handler that cannot answer
/// becomes a `5xx` (`500` handler error, `501` no handler, `502` malformed
/// answer, `503` binding overloaded or closed, `504` timeout).
#[derive(Clone)]
pub struct ForeignHttpHandler {
    inner: Arc<HandlerInner>,
}

impl ForeignHttpHandler {
    pub fn new(
        upcalls: Upcalls,
        bodies: HttpBodies,
        service_id: &str,
        options: ForeignHttpOptions,
    ) -> Self {
        Self {
            inner: Arc::new(HandlerInner {
                upcalls,
                bodies,
                service_id: service_id.to_owned(),
                options,
            }),
        }
    }

    /// As an [`HttpHandler`] for [`Expose::expose_http`](crate::expose::Expose::expose_http).
    pub fn handler(&self) -> HttpHandler {
        let this = self.clone();
        Arc::new(move |req| {
            let this = this.clone();
            Box::pin(async move { Ok(this.serve(req).await) })
        })
    }

    /// Serve one request through the foreign handler.
    pub async fn serve<B>(&self, request: http::Request<B>) -> http::Response<ResponseBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<RelayError>,
    {
        let o = self.inner.options;
        let (parts, body) = request.into_parts();
        let peer_id = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .map(|p| p.as_str().to_owned());
        let declared = parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        if let (Some(len), Some(limit)) = (declared, o.max_request_body) {
            if len > limit {
                return status_only(StatusCode::PAYLOAD_TOO_LARGE);
            }
        }
        let mut body: RequestBody = body.map_err(Into::into).boxed_unsync();
        let (mode, request_body) = match declared {
            Some(len) if len <= o.inline_request_max as u64 => {
                match read_exact_body(&mut body, len).await {
                    Ok(bytes) => (HttpBodyMode::Inline(bytes), None),
                    Err(e) => {
                        tracing::debug!(target: UPCALL, "expose_http: inline request body: {e}");
                        return status_only(StatusCode::BAD_REQUEST);
                    }
                }
            }
            _ => (HttpBodyMode::Stream, Some(body)),
        };

        let (tx, rx) = body_channel(o.response_buffer);
        let (stream_id, guard) = self.inner.bodies.open(request_body, tx, o);
        let payload = codec::encode_expose_http_request(&ExposeHttpRequest {
            service_id: self.inner.service_id.clone(),
            peer_id,
            stream_id,
            version: version_code(parts.version),
            method: parts.method.as_str().to_owned(),
            uri: parts.uri.to_string(),
            headers: parts
                .headers
                .iter()
                .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
                .collect(),
            body: mode,
        });
        // Dropping this future (the peer went away) cancels the upcall and,
        // through `guard`, closes the body handle.
        let answer = self
            .inner
            .upcalls
            .call(UpcallKind::ExposeHttp, payload)
            .await
            .and_then(|b| {
                codec::decode_expose_http_response(&b)
                    .map_err(|e| UpcallError::Decode(e.to_string()))
            });
        let resp = match answer {
            Ok(r) => r,
            Err(e) => return upcall_failure(&self.inner.service_id, e),
        };
        let ExposeHttpResponse {
            status,
            headers,
            body,
        } = resp;
        let (Ok(status), Ok(headers)) = (StatusCode::from_u16(status), header_map(&headers)) else {
            tracing::debug!(target: UPCALL, service = %self.inner.service_id, "expose_http: invalid response head");
            return status_only(StatusCode::BAD_GATEWAY);
        };
        let body: ResponseBody = if parts.method == Method::HEAD {
            // No body on a HEAD response; `guard` drops and closes the handle.
            empty_body()
        } else {
            match body {
                HttpBodyMode::Inline(bytes) => {
                    if o.max_response_body
                        .is_some_and(|limit| bytes.len() as u64 > limit)
                    {
                        return status_only(StatusCode::BAD_GATEWAY);
                    }
                    Full::new(Bytes::from(bytes))
                        .map_err(|e: Infallible| -> RelayError { match e {} })
                        .boxed_unsync()
                }
                HttpBodyMode::Stream => ForeignBody {
                    inner: rx,
                    _guard: guard,
                }
                .boxed_unsync(),
            }
        };
        let mut response = http::Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }
}

impl<B> tower::Service<http::Request<B>> for ForeignHttpHandler
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<RelayError>,
{
    type Response = http::Response<ResponseBody>;
    type Error = Infallible;
    type Future = BoxFut<Result<http::Response<ResponseBody>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.serve(request).await) })
    }
}

/// Register `service_id` on `expose` with both halves foreign: the
/// [`foreign_authorize_fn`] policy and a [`ForeignHttpHandler`]. Returns the
/// handler (clone-cheap) for callers that also want it elsewhere.
pub fn expose_foreign_http(
    expose: &Expose,
    upcalls: &Upcalls,
    bodies: &HttpBodies,
    service_id: &str,
    options: ForeignHttpOptions,
) -> ForeignHttpHandler {
    let handler = ForeignHttpHandler::new(upcalls.clone(), bodies.clone(), service_id, options);
    expose.expose_http(
        service_id,
        foreign_authorize_fn(upcalls.clone(), service_id),
        handler.handler(),
    );
    handler
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// The UCv1 / `aster-expose` head codec version code.
fn version_code(v: Version) -> u8 {
    match v {
        Version::HTTP_09 => 0,
        Version::HTTP_10 => 1,
        Version::HTTP_2 => 3,
        Version::HTTP_3 => 4,
        _ => 2,
    }
}

fn header_map(headers: &[HttpHeader]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| format!("{name:?}: {e}"))?;
        let value = HeaderValue::from_bytes(value).map_err(|e| format!("{name}: {e}"))?;
        map.append(name, value);
    }
    Ok(map)
}

/// Read a body of declared length `len` completely; it must end exactly
/// there.
async fn read_exact_body(body: &mut RequestBody, len: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(len as usize);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Ok(data) = frame.into_data() {
            if out.len() as u64 + data.len() as u64 > len {
                return Err(format!("body longer than content-length {len}"));
            }
            out.extend_from_slice(&data);
        }
    }
    if out.len() as u64 != len {
        return Err(format!("body of {} bytes, content-length {len}", out.len()));
    }
    Ok(out)
}

fn empty_body() -> ResponseBody {
    Full::new(Bytes::new())
        .map_err(|e: Infallible| -> RelayError { match e {} })
        .boxed_unsync()
}

fn status_only(status: StatusCode) -> http::Response<ResponseBody> {
    let mut r = http::Response::new(empty_body());
    *r.status_mut() = status;
    r
}

fn upcall_failure(service_id: &str, e: UpcallError) -> http::Response<ResponseBody> {
    let status = match &e {
        UpcallError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        UpcallError::Overloaded | UpcallError::Closed => StatusCode::SERVICE_UNAVAILABLE,
        UpcallError::Unimplemented(_) => StatusCode::NOT_IMPLEMENTED,
        UpcallError::Decode(_) => StatusCode::BAD_GATEWAY,
        UpcallError::Foreign(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // The foreign message is logged, never sent to the remote peer.
    tracing::debug!(target: UPCALL, service = %service_id, error = %e, %status, "expose_http upcall failed");
    status_only(status)
}
