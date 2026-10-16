//! RPC client: a connection on the [`RPC_ALPN`](super::server::RPC_ALPN) and
//! per-pattern call helpers.
//!
//! The helpers operate on raw payload **bytes** — a generated stub (or a
//! hand-written caller) supplies the request/response encode/decode via its own
//! payload `Fory` ([`new_payload_fory`](super::codec::new_payload_fory)).
//!
//! **Streams.** Unary calls run on the connection's multiplexed-stream pool
//! (`CoreConnection::acquire_stream`), exactly like every binding's v0.3 fast
//! path (`ffi::aster_call_unary`): one write for header plus request, read to
//! the trailer, then the stream goes back to the pool — or is discarded if the
//! call did not end cleanly. The SHARED pool is keyed by session id 0; a session
//! gets its own pool (one stream, so a session's calls are sequential). Each
//! streaming call opens a dedicated substream that bypasses the pool (spec
//! `Aster-multiplexed-streams.md` §3).
//!
//! **Options.** [`CallOptions`] carries a session id, a deadline, extra
//! metadata and a serialization-mode override. The `*_with` / `*_open` methods
//! take them; the plain methods are wrappers with default options. A deadline
//! is written into the header in whole seconds rounded **up**
//! ([`deadline_header_secs`](super::codec::deadline_header_secs)) and enforced
//! client-side for every call shape: when it passes, the call is cancelled and
//! fails with `DEADLINE_EXCEEDED`. For unary retries, each transport attempt
//! receives that budget; interceptor hooks and retry backoff are outside it.
//! Streaming deadlines bound the whole call from opening the stream.
//!
//! **Cancellation.** [`RequestSink::cancel`], [`ResponseStream::cancel`] and
//! [`ResponseFuture::cancel`] write a `FLAG_CANCEL` frame (unless a frame write
//! was cut off mid-way, which would make it unparseable) and then reset the
//! send side with `RESET_CODE_CANCELLED`. Dropping a sink without
//! [`finish`](RequestSink::finish), or a response stream before its trailer,
//! cancels the call the same way. A unary call whose future is dropped part-way
//! discards its pooled stream and cancels it, so the pool never hands out a
//! stream with stale bytes on it.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aster_transport_core::framing::{
    encode_frame, encode_payload_frame, payload_of, FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER,
    FLAG_ROW_SCHEMA, FLAG_TRAILER, MAX_FRAME_SIZE, RESET_CODE_CANCELLED,
};
use aster_transport_core::pool::{AcquireError, PoolKey};
use aster_transport_core::{
    CoreConnection, CoreRecvStream, CoreSendStream, MultiplexedStreamHandle,
};
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::Instrument;

use super::codec::{
    deadline_header_secs, decode_rpc_status, encode_stream_header, SerializationMode, StreamHeader,
};
use super::interceptor::{CircuitBreaker, ClientCallContext, Interceptor, Pipeline, RetryPolicy};
use super::observe::{client_status, error_status, ClientObs};
use super::status::{status_to_error, StatusCode};
use crate::error::{Error, Result};

/// How long a cancel waits to write its `FLAG_CANCEL` frame (and then its
/// reset) before giving up. A send side blocked on flow control cannot take
/// the frame; the reset still goes out once the lock frees.
const CANCEL_WRITE_GRACE: Duration = Duration::from_millis(250);

/// Target size of one write when a call's requests are all known up front
/// (`client_stream` with a `Vec`): frames are batched up to this many bytes.
const WRITE_BATCH: usize = 64 * 1024;

/// Per-call options for the `*_with` / `*_open` methods. `Default` is "no
/// options": the header is sent as given.
///
/// Each set field overrides the corresponding [`StreamHeader`] field:
/// - `session_id`: non-zero replaces `header.session_id` (and selects that
///   session's pooled stream for unary calls). Get one from
///   [`RpcConnection::open_session`].
/// - `deadline`: a client-side timeout (`DEADLINE_EXCEEDED`), written into
///   the header as whole seconds rounded up. For unary calls, each transport
///   attempt (including pool wait) receives this budget; interceptor hooks and
///   retry backoff are outside it. For streaming calls, it bounds the whole
///   call from opening the stream, including sending and receiving.
/// - `metadata`: appended to the header's metadata.
/// - `serialization_mode`: replaces `header.serialization_mode`.
#[derive(Clone, Debug, Default)]
pub struct CallOptions {
    /// Session id; `0` keeps the header's.
    pub session_id: u32,
    /// Per-attempt unary or whole-stream budget; `None` = no client deadline.
    pub deadline: Option<Duration>,
    /// Extra `(key, value)` metadata, appended to the header's.
    pub metadata: Vec<(String, String)>,
    /// Serialization-mode override.
    pub serialization_mode: Option<SerializationMode>,
}

impl CallOptions {
    /// No options (same as `Default`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the session id.
    pub fn session(mut self, session_id: u32) -> Self {
        self.session_id = session_id;
        self
    }

    /// Set the per-attempt unary or whole-stream budget.
    /// See the pattern-specific timing contract on [`CallOptions`].
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Add one metadata entry.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.push((key.into(), value.into()));
        self
    }

    /// Override the serialization mode.
    pub fn serialization_mode(mut self, mode: SerializationMode) -> Self {
        self.serialization_mode = Some(mode);
        self
    }

    /// The header actually sent: `header` with these options applied and the
    /// effective `deadline` written in whole seconds (rounded up).
    fn apply(&self, header: &StreamHeader, deadline: Option<Duration>) -> StreamHeader {
        let mut h = header.clone();
        if self.session_id != 0 {
            h.session_id = self.session_id as i32;
        }
        if let Some(d) = deadline {
            h.deadline = deadline_header_secs(d);
        }
        for (k, v) in &self.metadata {
            h.metadata_keys.push(k.clone());
            h.metadata_values.push(v.clone());
        }
        if let Some(mode) = self.serialization_mode {
            h.serialization_mode = mode.as_i8();
        }
        h
    }
}

/// A client connection to a peer's RPC server. Obtain one via
/// [`Node::rpc_connect`](crate::Node::rpc_connect). Cheap to clone; clones
/// share the underlying QUIC connection, its stream pool and its session-id
/// counter.
///
/// Attach client-side policies with the builder methods
/// ([`with_deadline`](RpcConnection::with_deadline),
/// [`with_retry`](RpcConnection::with_retry),
/// [`with_circuit_breaker`](RpcConnection::with_circuit_breaker),
/// [`with_interceptor`](RpcConnection::with_interceptor)); they apply to unary
/// calls. With no policies configured the pipeline is a no-op.
#[derive(Clone)]
pub struct RpcConnection {
    inner: CoreConnection,
    pipeline: Pipeline,
}

impl RpcConnection {
    pub(crate) fn from_core(inner: CoreConnection) -> Self {
        Self {
            inner,
            pipeline: Pipeline::default(),
        }
    }

    /// Binding support: wrap a transport-core connection already dialled on
    /// [`RPC_ALPN`](crate::RPC_ALPN). Not part of the stable API.
    #[cfg(feature = "binding-support")]
    #[doc(hidden)]
    pub fn from_core_connection(inner: CoreConnection) -> Self {
        Self::from_core(inner)
    }

    /// Binding support: borrow the underlying transport-core connection. Not
    /// part of the stable API.
    #[cfg(feature = "binding-support")]
    #[doc(hidden)]
    pub fn core_connection(&self) -> &CoreConnection {
        &self.inner
    }

    /// Add a client [`Interceptor`] (runs `on_request`/`on_response`/`on_error`
    /// around unary calls). Builder-style.
    pub fn with_interceptor(mut self, interceptor: impl Interceptor) -> Self {
        self.pipeline.interceptors.push(Arc::new(interceptor));
        self
    }

    /// Set the [`RetryPolicy`] for unary calls. Enable only for idempotent calls.
    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.pipeline.retry = Some(policy);
        self
    }

    /// Set the [`CircuitBreaker`] guarding unary calls.
    pub fn with_circuit_breaker(mut self, breaker: CircuitBreaker) -> Self {
        self.pipeline.circuit_breaker = Some(breaker);
        self
    }

    /// Set a default per-call deadline for unary calls: each unary attempt is
    /// bounded by `deadline` (exceeding it yields `DEADLINE_EXCEEDED`), and the
    /// relative seconds are written into the wire header. A
    /// [`CallOptions::deadline`] overrides it.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.pipeline.deadline = Some(deadline);
        self
    }

    /// The most unary calls this connection keeps on the wire at once (the
    /// SHARED pool's stream count). Further calls wait for a free stream, up to
    /// the pool's acquire timeout (5 s), then fail with `UNAVAILABLE`.
    pub fn shared_pool_size(&self) -> usize {
        self.inner.pool().shared_pool_size()
    }

    /// Change [`shared_pool_size`](RpcConnection::shared_pool_size) (minimum
    /// 1; default 8). Connection-wide: it applies to every clone of this
    /// connection and to binding clients sharing it. Growing takes effect at
    /// once; shrinking closes surplus streams as calls return them.
    pub fn set_shared_pool_size(&self, size: usize) {
        self.inner.pool().set_shared_pool_size(size);
    }

    /// Open a client session on this connection: a fresh session id from the
    /// connection's counter (shared with every client on the connection, so ids
    /// never collide), bound to an [`RpcSession`] that stamps it on each call.
    /// The server creates the session instance on the first call that carries
    /// the id. Fails only if the connection has used up its 2³¹−1 ids.
    pub fn open_session(&self) -> Result<RpcSession> {
        let id = self.inner.next_session_id().ok_or_else(|| {
            Error::rpc(
                StatusCode::ResourceExhausted,
                "session ids exhausted on this connection",
            )
        })?;
        Ok(RpcSession {
            conn: self.clone(),
            id,
        })
    }

    // ── unary ───────────────────────────────────────────────────────────────

    /// Unary call: send one request payload, get the response payload back. A
    /// non-OK trailer surfaces as [`Error::Rpc`]. Runs the configured pipeline
    /// (interceptors / retry / deadline / circuit-breaker); a no-op pipeline
    /// goes straight to the transport.
    pub async fn unary(&self, header: &StreamHeader, request: Vec<u8>) -> Result<Vec<u8>> {
        self.unary_with(header, request, &CallOptions::default())
            .await
    }

    /// [`unary`](RpcConnection::unary) with [`CallOptions`]. `options.deadline`
    /// overrides the connection's [`with_deadline`](RpcConnection::with_deadline);
    /// with retries configured it bounds each attempt.
    pub async fn unary_with(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
        options: &CallOptions,
    ) -> Result<Vec<u8>> {
        let deadline = options.deadline.or(self.pipeline.deadline);
        let mut h = options.apply(header, deadline);
        // One client span and one completion per logical call, retries
        // included; it injects the call's trace context into `h`.
        let mut obs = ClientObs::start(&mut h);
        let r = self
            .unary_attempts(&h, request, deadline)
            .instrument(obs.span())
            .await;
        obs.finish(client_status(&r));
        r
    }

    /// The pipeline (or the bare transport) for one unary call.
    async fn unary_attempts(
        &self,
        h: &StreamHeader,
        request: Vec<u8>,
        deadline: Option<Duration>,
    ) -> Result<Vec<u8>> {
        if self.pipeline.is_passthrough() {
            return match deadline {
                None => self.transport_unary(h, request).await,
                Some(d) => match tokio::time::timeout(d, self.transport_unary(h, request)).await {
                    Ok(r) => r,
                    Err(_) => Err(deadline_exceeded(d)),
                },
            };
        }
        let ctx = ClientCallContext {
            service: h.service.clone(),
            method: h.method.clone(),
            attempt: 1,
            deadline,
            metadata: h
                .metadata_keys
                .iter()
                .cloned()
                .zip(h.metadata_values.iter().cloned())
                .collect(),
        };
        self.pipeline
            .run_unary(ctx, request, |req| self.transport_unary(h, req))
            .await
    }

    /// One pooled round trip (no pipeline, no deadline): header + request in
    /// one write, read to the trailer, return the stream to the pool.
    async fn transport_unary(&self, header: &StreamHeader, request: Vec<u8>) -> Result<Vec<u8>> {
        let mut pair = encode_frame(&encode_stream_header(header)?, FLAG_HEADER)?;
        pair.extend_from_slice(&encode_payload_frame(&request, FLAG_END_STREAM)?);
        let handle = self
            .inner
            .acquire_stream(pool_key(header.session_id))
            .await
            .map_err(acquire_error)?;
        let mut call = PooledCall::new(handle);
        let (send, recv) = call.streams();

        if let Err(e) = send.write_all(pair).await {
            call.discard();
            return Err(e.into());
        }
        call.writing = false;

        let mut response: Option<(Vec<u8>, u8)> = None;
        let mut extra = false;
        let trailer = loop {
            match read_frame(&recv).await {
                Ok(Some((payload, flags))) => {
                    if flags & FLAG_TRAILER != 0 {
                        break payload;
                    }
                    if flags & FLAG_ROW_SCHEMA != 0 {
                        continue;
                    }
                    if response.is_none() {
                        response = Some((payload, flags));
                    } else {
                        extra = true;
                    }
                }
                Ok(None) => {
                    call.discard();
                    return Err(if response.is_some() {
                        // The handler wrote its response and dropped the call
                        // without a trailer: an incomplete response, not OK.
                        Error::Connection(
                            "RPC response missing trailer (incomplete response)".into(),
                        )
                    } else {
                        Error::Transport(anyhow::anyhow!("stream ended before response"))
                    });
                }
                Err(e) => {
                    call.discard();
                    return Err(e);
                }
            }
        };
        // At the trailer the stream is back at a call boundary: reusable.
        call.release();

        check_trailer(&trailer)?;
        if extra {
            return Err(Error::Transport(anyhow::anyhow!(
                "received multiple data frames in unary call"
            )));
        }
        match response {
            Some((payload, flags)) => Ok(payload_of(payload, flags)?),
            // OK trailer with no data frame: the handler responded with an
            // empty payload (a data frame cannot carry one).
            None => Ok(Vec::new()),
        }
    }

    // ── server streaming ────────────────────────────────────────────────────

    /// Server-streaming call: send one request, read a stream of responses.
    pub async fn server_stream(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
    ) -> Result<ResponseStream> {
        self.server_stream_with(header, request, &CallOptions::default())
            .await
    }

    /// [`server_stream`](RpcConnection::server_stream) with [`CallOptions`]. The
    /// deadline bounds the whole call, including reading the stream.
    pub async fn server_stream_with(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
        options: &CallOptions,
    ) -> Result<ResponseStream> {
        // The request ends the request stream and goes out in the header's
        // write. The send side stays open (not QUIC-finished) so a later cancel
        // can still write `FLAG_CANCEL`.
        let request = encode_payload_frame(&request, FLAG_END_STREAM)?;
        let ctl = self.open_call(header, options, Some(request)).await?;
        Ok(ResponseStream::new(ctl))
    }

    // ── client streaming ────────────────────────────────────────────────────

    /// Client-streaming call: send many requests (≥1), get one response.
    pub async fn client_stream(
        &self,
        header: &StreamHeader,
        requests: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        self.client_stream_with(header, requests, &CallOptions::default())
            .await
    }

    /// [`client_stream`](RpcConnection::client_stream) with [`CallOptions`].
    pub async fn client_stream_with(
        &self,
        header: &StreamHeader,
        requests: Vec<Vec<u8>>,
        options: &CallOptions,
    ) -> Result<Vec<u8>> {
        if requests.is_empty() {
            return Err(Error::InvalidArgument(
                "client-streaming requires at least one request".into(),
            ));
        }
        let (sink, response) = self.client_stream_open(header, options).await?;
        // All requests are known up front, so frame them into batches of up
        // to WRITE_BATCH bytes: one write per batch rather than per message.
        let n = requests.len();
        let mut batch = Vec::new();
        for (i, req) in requests.into_iter().enumerate() {
            let last = i + 1 == n;
            let flags = if last { FLAG_END_STREAM } else { 0 };
            batch.extend_from_slice(&encode_payload_frame(&req, flags)?);
            if last || batch.len() >= WRITE_BATCH {
                if let Err(sent) = sink.send_framed(std::mem::take(&mut batch), last).await {
                    return settle_failed_send(sent, response).await;
                }
            }
        }
        response.await
    }

    /// Open a client-streaming call for **incremental** sending: a
    /// [`RequestSink`] to [`send`](RequestSink::send) requests as they are
    /// produced (each flushed at once) and [`finish`](RequestSink::finish), plus
    /// the [`ResponseFuture`] to await the single response. Mirrors
    /// [`bidi_open`](RpcConnection::bidi_open).
    ///
    /// The server dispatches the call on the first request frame, so a call
    /// must send at least one request; finishing with none fails with
    /// `INVALID_ARGUMENT` and cancels the call.
    pub async fn client_stream_open(
        &self,
        header: &StreamHeader,
        options: &CallOptions,
    ) -> Result<(RequestSink, ResponseFuture)> {
        let ctl = self.open_call(header, options, None).await?;
        Ok((RequestSink::new(ctl.clone()), ResponseFuture::new(ctl)))
    }

    // ── bidi ────────────────────────────────────────────────────────────────

    /// Bidirectional call: send many requests (≥1) while reading responses. The
    /// requests are written by a background task so the caller can read the
    /// returned [`ResponseStream`] concurrently. For *interactive* streams where
    /// requests are produced over time (e.g. a remote shell), use
    /// [`bidi_open`](RpcConnection::bidi_open) instead.
    pub async fn bidi(
        &self,
        header: &StreamHeader,
        requests: Vec<Vec<u8>>,
    ) -> Result<ResponseStream> {
        self.bidi_with(header, requests, &CallOptions::default())
            .await
    }

    /// [`bidi`](RpcConnection::bidi) with [`CallOptions`]. The deadline bounds
    /// the whole call, as for [`bidi_open_with`](RpcConnection::bidi_open_with).
    pub async fn bidi_with(
        &self,
        header: &StreamHeader,
        requests: Vec<Vec<u8>>,
        options: &CallOptions,
    ) -> Result<ResponseStream> {
        if requests.is_empty() {
            return Err(Error::InvalidArgument(
                "bidi streaming requires at least one request".into(),
            ));
        }
        let (sink, resp) = self.bidi_open_with(header, options).await?;
        tokio::spawn(async move {
            let n = requests.len();
            for (i, req) in requests.into_iter().enumerate() {
                let sent = if i + 1 == n {
                    sink.send_last(req).await
                } else {
                    sink.send(req).await
                };
                if sent.is_err() {
                    return;
                }
            }
        });
        Ok(resp)
    }

    /// Open a bidirectional call for **incremental** sending: returns a
    /// [`BidiSink`] whose [`send`](RequestSink::send) flushes each request frame
    /// immediately (no buffering), plus the [`ResponseStream`] to read
    /// concurrently. This is the handle an interactive client needs — a remote
    /// shell pumps keystrokes through `send` as they're typed while draining the
    /// response stream to its terminal.
    ///
    /// The server does not dispatch the call until the first request frame
    /// arrives, so send an initial frame promptly (an interactive client
    /// typically opens by sending its starting window size). Call
    /// [`RequestSink::finish`] when there are no more requests.
    pub async fn bidi_open(&self, header: &StreamHeader) -> Result<(BidiSink, ResponseStream)> {
        self.bidi_open_with(header, &CallOptions::default()).await
    }

    /// [`bidi_open`](RpcConnection::bidi_open) with [`CallOptions`]. The deadline
    /// bounds the whole call: sends and reads past it fail with
    /// `DEADLINE_EXCEEDED` and the call is cancelled.
    pub async fn bidi_open_with(
        &self,
        header: &StreamHeader,
        options: &CallOptions,
    ) -> Result<(BidiSink, ResponseStream)> {
        let ctl = self.open_call(header, options, None).await?;
        Ok((RequestSink::new(ctl.clone()), ResponseStream::new(ctl)))
    }

    /// Open a dedicated streaming substream and write the call header (plus
    /// `first`, already framed, in the same write). The deadline clock starts
    /// here.
    async fn open_call(
        &self,
        header: &StreamHeader,
        options: &CallOptions,
        first: Option<Vec<u8>>,
    ) -> Result<Arc<CallCtl>> {
        let deadline = options.deadline;
        let at = deadline.map(|d| Instant::now() + d);
        let mut h = options.apply(header, deadline);
        let mut obs = ClientObs::start(&mut h);
        let opened = async {
            let mut header_frame = encode_frame(&encode_stream_header(&h)?, FLAG_HEADER)?;
            if let Some(first) = first {
                header_frame.extend_from_slice(&first);
            }
            let open = self.inner.open_streaming_substream();
            let (send, recv) = match (at, deadline) {
                (Some(at), Some(d)) => match tokio::time::timeout_at(at, open).await {
                    Ok(r) => r?,
                    Err(_) => return Err(deadline_exceeded(d)),
                },
                _ => open.await?,
            };
            Ok((send, recv, header_frame))
        }
        .await;
        if opened.is_err() {
            obs.finish(client_status(&opened));
        }
        let (send, recv, header_frame) = opened?;
        let ctl = Arc::new(CallCtl {
            send,
            recv,
            deadline: at.zip(deadline),
            state: AtomicU8::new(OPEN),
            torn: AtomicBool::new(false),
            write_lock: tokio::sync::Mutex::new(()),
            cancelled: Notify::new(),
            obs: std::sync::Mutex::new(obs),
        });
        let written = ctl.write_timed(header_frame).await;
        if written.is_err() {
            ctl.observe(client_status(&written));
        }
        written?;
        Ok(ctl)
    }
}

/// A client session: an [`RpcConnection`] plus a session id from
/// [`RpcConnection::open_session`], stamped on every call made through it.
/// Calls on a session-scoped service reach the same server-side instance; a
/// session's unary calls share one pooled stream, so they run one at a time.
/// Cheap to clone (clones are the same session).
///
/// The server keeps the instance until the connection closes; there is no
/// explicit close on the wire.
#[derive(Clone)]
pub struct RpcSession {
    conn: RpcConnection,
    id: u32,
}

impl RpcSession {
    /// The session id (non-zero).
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The connection the session runs on.
    pub fn connection(&self) -> &RpcConnection {
        &self.conn
    }

    /// Default options for this session (`session_id` set).
    pub fn options(&self) -> CallOptions {
        CallOptions::default().session(self.id)
    }

    fn bind(&self, options: &CallOptions) -> CallOptions {
        let mut o = options.clone();
        o.session_id = self.id;
        o
    }

    /// [`RpcConnection::unary`] in this session.
    pub async fn unary(&self, header: &StreamHeader, request: Vec<u8>) -> Result<Vec<u8>> {
        self.conn.unary_with(header, request, &self.options()).await
    }

    /// [`RpcConnection::unary_with`] in this session (`options.session_id` is
    /// replaced by the session's id).
    pub async fn unary_with(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
        options: &CallOptions,
    ) -> Result<Vec<u8>> {
        self.conn
            .unary_with(header, request, &self.bind(options))
            .await
    }

    /// [`RpcConnection::server_stream_with`] in this session.
    pub async fn server_stream(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
        options: &CallOptions,
    ) -> Result<ResponseStream> {
        self.conn
            .server_stream_with(header, request, &self.bind(options))
            .await
    }

    /// [`RpcConnection::client_stream_open`] in this session.
    pub async fn client_stream_open(
        &self,
        header: &StreamHeader,
        options: &CallOptions,
    ) -> Result<(RequestSink, ResponseFuture)> {
        self.conn
            .client_stream_open(header, &self.bind(options))
            .await
    }

    /// [`RpcConnection::bidi_open_with`] in this session.
    pub async fn bidi_open(
        &self,
        header: &StreamHeader,
        options: &CallOptions,
    ) -> Result<(BidiSink, ResponseStream)> {
        self.conn.bidi_open_with(header, &self.bind(options)).await
    }
}

// ── pooled unary stream ─────────────────────────────────────────────────────

/// The pool key for a header's session id: SHARED for 0, otherwise the id's
/// little-endian bytes — the same key `ffi::aster_call_*` and the Python
/// binding use, so every client on a connection shares a session's stream.
fn pool_key(session_id: i32) -> PoolKey {
    if session_id == 0 {
        None
    } else {
        Some((session_id as u32).to_le_bytes().to_vec())
    }
}

fn acquire_error(e: AcquireError) -> Error {
    match e {
        // Opening the stream failed: the connection is the problem, as with
        // the unpooled `open_bi` this replaced.
        AcquireError::StreamOpenFailed(e) => Error::Transport(e),
        // Pool exhaustion, QUIC stream limits, a closed pool: the call never
        // reached the wire. UNAVAILABLE, as the Python binding maps them.
        other => Error::rpc(
            StatusCode::Unavailable,
            format!("stream acquire failed: {other}"),
        ),
    }
}

/// A pooled stream in use by one unary call. [`release`](Self::release) returns
/// it to the pool (only at a call boundary); [`discard`](Self::discard) drops it.
/// Dropped with neither — the call's future was dropped part-way, by a deadline
/// or by the caller — it is discarded **and cancelled**, so the server stops
/// the handler and the pool never reuses a stream with a half-read response.
struct PooledCall {
    handle: Option<MultiplexedStreamHandle>,
    /// A request write is (or may be) part-way: a `FLAG_CANCEL` written now
    /// would land inside that frame, so a cancel only resets.
    writing: bool,
}

impl PooledCall {
    fn new(handle: MultiplexedStreamHandle) -> Self {
        Self {
            handle: Some(handle),
            writing: true,
        }
    }

    fn streams(&self) -> (CoreSendStream, CoreRecvStream) {
        let pair = self.handle.as_ref().expect("live pooled call").get();
        (pair.0.clone(), pair.1.clone())
    }

    fn release(mut self) {
        drop(self.handle.take());
    }

    fn discard(mut self) {
        if let Some(h) = self.handle.take() {
            h.discard();
        }
    }
}

impl Drop for PooledCall {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let (send, recv) = {
                let pair = h.get();
                (pair.0.clone(), pair.1.clone())
            };
            h.discard();
            spawn_cancel(send, recv, self.writing);
        }
    }
}

/// Cancel an abandoned call from a `Drop`: write `FLAG_CANCEL` (unless `torn`),
/// reset the send side, stop the receive side. Needs a tokio runtime to write;
/// without one only the receive side is stopped and the rest is left to the
/// streams' own drop.
fn spawn_cancel(send: CoreSendStream, recv: CoreRecvStream, torn: bool) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn(async move {
                cancel_streams(&send, &recv, torn).await;
            });
        }
        Err(_) => {
            let _ = recv.stop(u64::from(RESET_CODE_CANCELLED));
        }
    }
}

async fn cancel_streams(send: &CoreSendStream, recv: &CoreRecvStream, torn: bool) {
    if !torn {
        if let Ok(frame) = encode_frame(&[], FLAG_CANCEL) {
            let _ = tokio::time::timeout(CANCEL_WRITE_GRACE, send.write_all(frame)).await;
        }
    }
    let _ = tokio::time::timeout(CANCEL_WRITE_GRACE, send.reset(RESET_CODE_CANCELLED)).await;
    // Stop reading too, so a server still producing sees its writes fail
    // (the reactor treats that as a cancel) instead of filling our window.
    // Fails only while a read is in flight; that read's owner is being torn
    // down and its drop stops the stream anyway.
    let _ = recv.stop(u64::from(RESET_CODE_CANCELLED));
}

// ── streaming call state ─────────────────────────────────────────────────────

const OPEN: u8 = 0;
const COMPLETE: u8 = 1;
const CANCELLED: u8 = 2;

/// Shared state of one streaming call: both stream halves, the deadline, and
/// whether the call is still open. Held by the call's sink and response side.
struct CallCtl {
    send: CoreSendStream,
    recv: CoreRecvStream,
    /// Absolute deadline plus the budget it came from (for the error message).
    deadline: Option<(Instant, Duration)>,
    state: AtomicU8,
    /// A frame write was cut off part-way, so the byte stream is no longer at
    /// a frame boundary: a later `FLAG_CANCEL` would be swallowed by that frame.
    torn: AtomicBool,
    /// Serialises frame writes with `cancel`, so a cancel sees `torn` settled.
    write_lock: tokio::sync::Mutex<()>,
    /// Woken by `cancel` so in-flight sends and reads give way at once.
    cancelled: Notify,
    /// The call's client telemetry; the first final status wins.
    obs: std::sync::Mutex<ClientObs>,
}

impl CallCtl {
    /// Record the call's final status (telemetry; the first one wins).
    fn observe(&self, code: StatusCode) {
        self.obs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(code);
    }

    fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) == OPEN
    }

    fn closed_error(&self) -> Error {
        if self.state.load(Ordering::Acquire) == CANCELLED {
            Error::rpc(StatusCode::Cancelled, "call cancelled")
        } else {
            Error::InvalidArgument("call already completed".into())
        }
    }

    /// Run `fut` under the call's deadline, giving way to a cancel. On expiry
    /// the call is cancelled and the error is `DEADLINE_EXCEEDED`.
    async fn guarded<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        let cancelled = self.cancelled.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        if self.state.load(Ordering::Acquire) == CANCELLED {
            return Err(self.closed_error());
        }
        let timed = async {
            match self.deadline {
                None => fut.await,
                Some((at, budget)) => match tokio::time::timeout_at(at, fut).await {
                    Ok(r) => r,
                    Err(_) => Err(deadline_exceeded(budget)),
                },
            }
        };
        let out = tokio::select! {
            r = timed => r,
            () = &mut cancelled => return Err(self.closed_error()),
        };
        // Once the call is cancelled locally, every failure is that cancel. The
        // server answers our reset by resetting too, so a pending read can fail
        // with a transport error (UNAVAILABLE) at the same moment, and select!
        // picks between ready branches at random.
        if out.is_err() && self.state.load(Ordering::Acquire) == CANCELLED {
            return Err(self.closed_error());
        }
        if let Err(Error::Rpc { code, .. }) = &out {
            if *code == StatusCode::DeadlineExceeded.as_i32() {
                self.observe(StatusCode::DeadlineExceeded);
                self.cancel().await;
            }
        }
        out
    }

    /// Write one frame (under the deadline, giving way to a cancel). A write
    /// that does not complete marks the stream torn.
    async fn write_timed(&self, frame: Vec<u8>) -> Result<()> {
        if !self.is_open() {
            return Err(self.closed_error());
        }
        self.guarded(async {
            let _lock = self.write_lock.lock().await;
            let mut torn = TornOnDrop {
                torn: &self.torn,
                armed: true,
            };
            let r = self.send.write_all(frame).await;
            torn.armed = r.is_err();
            drop(torn);
            r.map_err(Error::from)
        })
        .await
    }

    /// Cancel the call (idempotent; a no-op once complete). See the module docs.
    async fn cancel(&self) {
        if self
            .state
            .compare_exchange(OPEN, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.observe(StatusCode::Cancelled);
        self.cancelled.notify_waiters();
        // Taking the write lock waits for an in-flight write to give way (it
        // was just woken), so `torn` is settled.
        let _lock = self.write_lock.lock().await;
        cancel_streams(&self.send, &self.recv, self.torn.load(Ordering::Acquire)).await;
    }

    /// [`cancel`](Self::cancel) from a `Drop`.
    fn cancel_in_background(self: &Arc<Self>) {
        if !self.is_open() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let ctl = self.clone();
                rt.spawn(async move { ctl.cancel().await });
            }
            Err(_) => {
                self.state.store(CANCELLED, Ordering::Release);
                self.observe(StatusCode::Cancelled);
                let _ = self.recv.stop(u64::from(RESET_CODE_CANCELLED));
            }
        }
    }

    /// The trailer arrived: the call is over. Half-close the send side so the
    /// server's stream task ends; nothing more will be written on it.
    async fn complete(&self) {
        if self
            .state
            .compare_exchange(OPEN, COMPLETE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancelled.notify_waiters();
            let _ = self.send.finish().await;
        }
    }
}

struct TornOnDrop<'a> {
    torn: &'a AtomicBool,
    armed: bool,
}

impl Drop for TornOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.torn.store(true, Ordering::Release);
        }
    }
}

/// The send half of a client-streaming or bidi call (see
/// [`RpcConnection::client_stream_open`] / [`RpcConnection::bidi_open`]). Each
/// [`send`](RequestSink::send) writes one request frame immediately — no
/// buffering — so keystroke-latency streams flush at once. Methods take
/// `&self`, so wrap it in an `Arc` to send from several tasks (e.g. one for
/// stdin, one for window-resize events).
///
/// Dropping the sink before [`finish`](RequestSink::finish) cancels the call
/// (unless it already completed).
pub struct RequestSink {
    ctl: Arc<CallCtl>,
    sent_any: AtomicBool,
    finished: AtomicBool,
}

/// The send half of a bidi call. Same type as [`RequestSink`].
pub type BidiSink = RequestSink;

impl RequestSink {
    fn new(ctl: Arc<CallCtl>) -> Self {
        Self {
            ctl,
            sent_any: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// Send one already-serialized request payload as a data frame, flushed
    /// immediately. (Typed callers go through [`BidiCall`](super::streaming::BidiCall).)
    pub async fn send(&self, payload: Vec<u8>) -> Result<()> {
        self.send_flags(payload, 0).await
    }

    /// Send the last request: the payload frame carries `FLAG_END_STREAM`, so
    /// this both sends and [`finish`](RequestSink::finish)es in one write.
    pub async fn send_last(&self, payload: Vec<u8>) -> Result<()> {
        self.send_flags(payload, FLAG_END_STREAM).await
    }

    /// Send several request payloads, framed into as few writes as possible
    /// (batches of up to 64 KiB); `last` ends the request stream with the
    /// final one. For callers that already hold the requests (bindings'
    /// buffered client-stream / bidi calls). An empty `payloads` is a no-op
    /// unless `last`, which then [`finish`](RequestSink::finish)es.
    pub async fn send_many(&self, payloads: Vec<Vec<u8>>, last: bool) -> Result<()> {
        if payloads.is_empty() {
            return if last { self.finish().await } else { Ok(()) };
        }
        let n = payloads.len();
        let mut batch = Vec::new();
        for (i, p) in payloads.into_iter().enumerate() {
            let end = last && i + 1 == n;
            let flags = if end { FLAG_END_STREAM } else { 0 };
            batch.extend_from_slice(&encode_payload_frame(&p, flags)?);
            if i + 1 == n || batch.len() >= WRITE_BATCH {
                self.send_framed(std::mem::take(&mut batch), end).await?;
            }
        }
        Ok(())
    }

    async fn send_flags(&self, payload: Vec<u8>, flags: u8) -> Result<()> {
        let frame = encode_payload_frame(&payload, flags)?;
        self.send_framed(frame, flags & FLAG_END_STREAM != 0).await
    }

    /// Write already-framed request frames (one or more) in one write; `last`
    /// when they end the request stream.
    async fn send_framed(&self, frames: Vec<u8>, last: bool) -> Result<()> {
        if self.finished.load(Ordering::Acquire) {
            return Err(Error::InvalidArgument(
                "request stream already finished".into(),
            ));
        }
        self.ctl.write_timed(frames).await?;
        self.sent_any.store(true, Ordering::Release);
        if last {
            self.finished.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Signal end-of-requests with an empty `FLAG_END_STREAM` frame. The server
    /// closes its request stream, so its `recv_request`/`RequestStream` ends;
    /// the response half stays open for reading. The send side itself stays
    /// open (no QUIC `finish`) so the call can still be cancelled. Idempotent.
    ///
    /// Finishing before any request was sent fails with `INVALID_ARGUMENT` and
    /// cancels the call: the server only dispatches a call on its first request.
    pub async fn finish(&self) -> Result<()> {
        if self.finished.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if !self.sent_any.load(Ordering::Acquire) {
            self.ctl.cancel().await;
            return Err(Error::InvalidArgument(
                "a streaming call requires at least one request".into(),
            ));
        }
        if !self.ctl.is_open() {
            // Completed (the server answered early) — nothing left to end.
            return match self.ctl.state.load(Ordering::Acquire) {
                COMPLETE => Ok(()),
                _ => Err(self.ctl.closed_error()),
            };
        }
        let frame = encode_frame(&[], FLAG_END_STREAM)?;
        self.ctl.write_timed(frame).await
    }

    /// Cancel the call: `FLAG_CANCEL`, then reset with `RESET_CODE_CANCELLED`.
    /// Pending and later operations on either half fail with `CANCELLED`.
    /// A no-op once the call has completed.
    pub async fn cancel(&self) {
        self.ctl.cancel().await;
    }
}

impl Drop for RequestSink {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            self.ctl.cancel_in_background();
        }
    }
}

/// A stream of response payloads. Call [`next`](ResponseStream::next) until it
/// returns `None` (the OK trailer) or an [`Err`] (a non-OK trailer / transport
/// error). [`collect`](ResponseStream::collect) gathers all items.
///
/// Dropping the stream before it ends cancels the call.
pub struct ResponseStream {
    ctl: Arc<CallCtl>,
    done: bool,
}

impl ResponseStream {
    fn new(ctl: Arc<CallCtl>) -> Self {
        Self { ctl, done: false }
    }

    /// The next response payload: `Some(Ok(_))` for a data frame, `None` at the
    /// OK trailer, `Some(Err(_))` for a non-OK trailer, a read error, the
    /// deadline (`DEADLINE_EXCEEDED`) or a cancel (`CANCELLED`).
    pub async fn next(&mut self) -> Option<Result<Vec<u8>>> {
        loop {
            if self.done {
                return None;
            }
            let read = self.ctl.guarded(read_frame(&self.ctl.recv)).await;
            return match read {
                // We cancelled the call ourselves (e.g. finishing an empty
                // request stream), and the server answered by ending its side.
                Ok(None) if self.ctl.state.load(Ordering::Acquire) == CANCELLED => {
                    self.done = true;
                    Some(Err(self.ctl.closed_error()))
                }
                Ok(None) => {
                    // Stream ended without a trailer — the handler panicked,
                    // dropped its `Call` without finishing, or the connection
                    // broke. A complete RPC always terminates with a trailer, so
                    // this is a failure, not a clean end (mirrors the Python
                    // transport, which errors on EOF-before-trailer).
                    self.done = true;
                    let err = Error::Connection(
                        "RPC stream closed before trailer (incomplete response)".into(),
                    );
                    self.ctl.observe(error_status(&err));
                    self.ctl.cancel().await;
                    Some(Err(err))
                }
                Ok(Some((payload, flags))) if flags & FLAG_TRAILER != 0 => {
                    self.done = true;
                    let end = if payload.is_empty() {
                        None // success with no explicit status
                    } else {
                        match decode_rpc_status(&payload) {
                            Ok(status) if status.code == StatusCode::Ok.as_i32() => None,
                            Ok(status) => Some(Err(status_to_error(status))),
                            Err(e) => Some(Err(e.into())),
                        }
                    };
                    self.ctl.observe(match &end {
                        None => StatusCode::Ok,
                        Some(r) => client_status(r),
                    });
                    self.ctl.complete().await;
                    end
                }
                // Row-schema metadata, not a response item.
                Ok(Some((_, flags))) if flags & FLAG_ROW_SCHEMA != 0 => continue,
                Ok(Some((payload, flags))) => Some(payload_of(payload, flags).map_err(Error::from)),
                Err(e) => {
                    self.done = true;
                    self.ctl.observe(error_status(&e));
                    self.ctl.cancel().await;
                    Some(Err(e))
                }
            };
        }
    }

    /// Collect all remaining response payloads, short-circuiting on the first error.
    pub async fn collect(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        while let Some(item) = self.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    /// Cancel the call: `FLAG_CANCEL`, then reset with `RESET_CODE_CANCELLED`.
    /// The stream then ends; a paired sink's sends fail with `CANCELLED`. A
    /// no-op once the trailer has been read.
    pub async fn cancel(&mut self) {
        self.done = true;
        self.ctl.cancel().await;
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        if !self.done {
            self.ctl.cancel_in_background();
        }
    }
}

/// The single response of a client-streaming call (see
/// [`RpcConnection::client_stream_open`]): a future of the response payload.
/// `.await` it once the requests are sent -- or earlier, it resolves as soon as
/// the server answers. A non-OK trailer is [`Error::Rpc`]; an OK trailer with
/// no response frame is an error too (a client-streaming call returns exactly
/// one response). Dropping it unresolved cancels the call.
pub struct ResponseFuture {
    ctl: Arc<CallCtl>,
    state: ResponseState,
}

enum ResponseState {
    Idle(ResponseStream),
    Waiting(Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>),
    Done,
}

impl ResponseFuture {
    fn new(ctl: Arc<CallCtl>) -> Self {
        Self {
            state: ResponseState::Idle(ResponseStream::new(ctl.clone())),
            ctl,
        }
    }

    /// Cancel the call (see [`RequestSink::cancel`]); awaiting the response
    /// then yields `CANCELLED`.
    pub async fn cancel(&self) {
        self.ctl.cancel().await;
    }
}

/// A client-stream send failed. A server that refuses the call (Gate 3, or a
/// handler that answers early) stops the request side, so the send fails
/// with a transport error ("sending stopped by peer", `UNAVAILABLE`), and then
/// sends the real outcome: read it. A deadline or cancel (an RPC status) is
/// already the outcome; so is the send failure when the response half failed
/// for a transport reason too (the connection was lost).
async fn settle_failed_send(sent: Error, response: ResponseFuture) -> Result<Vec<u8>> {
    if matches!(sent, Error::Rpc { .. }) {
        return Err(sent);
    }
    match response.await {
        Ok(reply) => Ok(reply),
        Err(status @ Error::Rpc { .. }) => Err(status),
        Err(_) => Err(sent),
    }
}

async fn single_response(mut stream: ResponseStream) -> Result<Vec<u8>> {
    let resp = match stream.next().await {
        Some(Ok(payload)) => payload,
        Some(Err(e)) => return Err(e),
        // OK trailer with no data frame: a client-streaming call must return
        // exactly one response. Data frames can't carry empty payloads, so an
        // empty response was never sent -- surfacing Ok(empty) would invent
        // one (Python treats this as a protocol error).
        None => {
            return Err(Error::Connection(
                "client-streaming response missing (server sent no response frame)".into(),
            ))
        }
    };
    // Drain the trailer so a non-OK status still surfaces.
    if let Some(Err(e)) = stream.next().await {
        return Err(e);
    }
    Ok(resp)
}

impl Future for ResponseFuture {
    type Output = Result<Vec<u8>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                ResponseState::Idle(_) => {
                    let ResponseState::Idle(stream) =
                        std::mem::replace(&mut this.state, ResponseState::Done)
                    else {
                        unreachable!("matched Idle above")
                    };
                    this.state = ResponseState::Waiting(Box::pin(single_response(stream)));
                }
                ResponseState::Waiting(fut) => {
                    let out = std::task::ready!(fut.as_mut().poll(cx));
                    this.state = ResponseState::Done;
                    return Poll::Ready(out);
                }
                ResponseState::Done => panic!("ResponseFuture polled after completion"),
            }
        }
    }
}

// ── framing helpers ─────────────────────────────────────────────────────────

/// Read one length-prefixed frame (`[4B LE len][1B flags][payload]`).
/// `Ok(None)` = the stream ended at a frame boundary.
async fn read_frame(recv: &CoreRecvStream) -> Result<Option<(Vec<u8>, u8)>> {
    let mut head = [0u8; 5];
    if recv.read_exact_into(&mut head[..4]).await.is_err() {
        return Ok(None); // stream ended
    }
    let frame_body_len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if frame_body_len == 0 || frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(Error::Transport(anyhow::anyhow!(
            "invalid frame length {frame_body_len}"
        )));
    }
    recv.read_exact_into(&mut head[4..]).await?;
    let mut payload = vec![0u8; frame_body_len - 1];
    recv.read_exact_into(&mut payload).await?;
    Ok(Some((payload, head[4])))
}

fn deadline_exceeded(d: Duration) -> Error {
    Error::rpc(
        StatusCode::DeadlineExceeded,
        format!("deadline exceeded after {d:?}"),
    )
}

/// Decode a trailer payload and convert a non-OK status into an error. An empty
/// trailer frame is a clean OK (as for streams and the bindings): a missing
/// trailer is detected separately, by EOF before one arrived.
fn check_trailer(trailer_payload: &[u8]) -> Result<()> {
    if trailer_payload.is_empty() {
        return Ok(());
    }
    let status = decode_rpc_status(trailer_payload)?;
    if status.is_ok() {
        Ok(())
    } else {
        Err(status_to_error(status))
    }
}
