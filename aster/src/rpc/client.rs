//! RPC client: a connection on the [`RPC_ALPN`](super::server::RPC_ALPN) and
//! per-pattern call helpers.
//!
//! The helpers operate on raw payload **bytes** — a generated stub (or a
//! hand-written caller) supplies the request/response encode/decode via its own
//! payload `Fory` ([`new_payload_fory`](super::codec::new_payload_fory)). Unary
//! uses the core fast path (`unary_call`); streaming opens a dedicated substream
//! (streaming bypasses the shared pool, per the multiplexed-streams spec).

use std::sync::Arc;
use std::time::Duration;

use aster_transport_core::framing::{
    encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER, MAX_FRAME_SIZE,
};
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

use super::codec::{decode_rpc_status, encode_stream_header, StreamHeader};
use super::interceptor::{CallContext, CircuitBreaker, Interceptor, Pipeline, RetryPolicy};
use super::status::{status_to_error, StatusCode};
use crate::error::{Error, Result};

/// A client connection to a peer's RPC server. Obtain one via
/// [`Node::rpc_connect`](crate::Node::rpc_connect). Cheap to clone.
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

    /// Set a per-call deadline: each unary attempt is bounded by `deadline`
    /// (exceeding it yields `DEADLINE_EXCEEDED`), and the relative seconds are
    /// written into the wire header.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.pipeline.deadline = Some(deadline);
        self
    }

    /// Unary call: send one request payload, get the response payload back. A
    /// non-OK trailer surfaces as [`Error::Rpc`]. Runs the configured pipeline
    /// (interceptors / retry / deadline / circuit-breaker); a no-op pipeline
    /// goes straight to the transport.
    pub async fn unary(&self, header: &StreamHeader, request: Vec<u8>) -> Result<Vec<u8>> {
        if self.pipeline.is_noop() {
            return self.transport_unary(header, request).await;
        }
        // Write the pipeline deadline into the wire header (informational for
        // the server; the client enforces it via the per-attempt timeout).
        let mut h = header.clone();
        if let Some(d) = self.pipeline.deadline {
            h.deadline = d.as_secs().min(i16::MAX as u64) as i16;
        }
        let ctx = CallContext {
            service: h.service.clone(),
            method: h.method.clone(),
            attempt: 1,
            deadline: self.pipeline.deadline,
            metadata: h
                .metadata_keys
                .iter()
                .cloned()
                .zip(h.metadata_values.iter().cloned())
                .collect(),
        };
        self.pipeline
            .run_unary(ctx, request, |req| self.transport_unary(&h, req))
            .await
    }

    /// One raw transport round-trip (no pipeline): frame header + request, read
    /// response + trailer.
    async fn transport_unary(&self, header: &StreamHeader, request: Vec<u8>) -> Result<Vec<u8>> {
        let header_frame = encode_frame(&encode_stream_header(header)?, FLAG_HEADER)?;
        let request_frame = encode_frame(&request, FLAG_END_STREAM)?;
        let res = self.inner.unary_call(&header_frame, &request_frame).await?;
        check_trailer(&res.trailer_payload)?;
        Ok(res.response_payload)
    }

    /// Server-streaming call: send one request, read a stream of responses.
    pub async fn server_stream(
        &self,
        header: &StreamHeader,
        request: Vec<u8>,
    ) -> Result<ResponseStream> {
        let (send, recv) = self.inner.open_streaming_substream().await?;
        write_header(&send, header).await?;
        send.write_all(encode_frame(&request, FLAG_END_STREAM)?)
            .await?;
        send.finish().await?;
        Ok(ResponseStream::new(recv))
    }

    /// Client-streaming call: send many requests (≥1), get one response.
    pub async fn client_stream(
        &self,
        header: &StreamHeader,
        requests: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        if requests.is_empty() {
            return Err(Error::InvalidArgument(
                "client-streaming requires at least one request".into(),
            ));
        }
        let (send, recv) = self.inner.open_streaming_substream().await?;
        write_header(&send, header).await?;
        write_requests(&send, requests).await?;
        send.finish().await?;

        let mut stream = ResponseStream::new(recv);
        let resp = match stream.next().await {
            Some(Ok(payload)) => payload,
            Some(Err(e)) => return Err(e),
            // OK trailer with no data frame: a client-streaming call must return
            // exactly one response. Data frames can't carry empty payloads, so an
            // empty response was never sent — surfacing Ok(empty) would invent
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
        if requests.is_empty() {
            return Err(Error::InvalidArgument(
                "bidi streaming requires at least one request".into(),
            ));
        }
        let (sink, resp) = self.bidi_open(header).await?;
        tokio::spawn(async move {
            for req in requests {
                if sink.send(req).await.is_err() {
                    return;
                }
            }
            let _ = sink.finish().await;
        });
        Ok(resp)
    }

    /// Open a bidirectional call for **incremental** sending: returns a
    /// [`BidiSink`] whose [`send`](BidiSink::send) flushes each request frame
    /// immediately (no buffering), plus the [`ResponseStream`] to read
    /// concurrently. This is the handle an interactive client needs — a remote
    /// shell pumps keystrokes through `send` as they're typed while draining the
    /// response stream to its terminal.
    ///
    /// The server does not dispatch the call until the first request frame
    /// arrives, so send an initial frame promptly (an interactive client
    /// typically opens by sending its starting window size). Call
    /// [`BidiSink::finish`] when there are no more requests.
    pub async fn bidi_open(&self, header: &StreamHeader) -> Result<(BidiSink, ResponseStream)> {
        let (send, recv) = self.inner.open_streaming_substream().await?;
        write_header(&send, header).await?;
        Ok((BidiSink { send }, ResponseStream::new(recv)))
    }
}

/// The send half of an interactive bidi call (see
/// [`RpcConnection::bidi_open`]). Each [`send`](BidiSink::send) writes one
/// request frame immediately — no buffering — so keystroke-latency streams flush
/// at once. Methods take `&self`, so wrap it in an `Arc` to send from several
/// tasks (e.g. one for stdin, one for window-resize events).
pub struct BidiSink {
    send: CoreSendStream,
}

impl BidiSink {
    /// Send one already-serialized request payload as a data frame, flushed
    /// immediately. (Typed callers go through [`BidiCall`](super::streaming::BidiCall).)
    pub async fn send(&self, payload: Vec<u8>) -> Result<()> {
        self.send.write_all(encode_frame(&payload, 0)?).await?;
        Ok(())
    }

    /// Signal end-of-requests by half-closing the send side (QUIC `finish`). The
    /// reactor reads this as stream EOF and closes the server's request stream, so
    /// its `recv_request`/`RequestStream` ends. The response half stays open for
    /// reading. Safe to call once.
    pub async fn finish(&self) -> Result<()> {
        self.send.finish().await?;
        Ok(())
    }
}

/// A stream of response payloads. Call [`next`](ResponseStream::next) until it
/// returns `None` (the OK trailer) or an [`Err`] (a non-OK trailer / transport
/// error). [`collect`](ResponseStream::collect) gathers all items.
pub struct ResponseStream {
    recv: CoreRecvStream,
    done: bool,
}

impl ResponseStream {
    fn new(recv: CoreRecvStream) -> Self {
        Self { recv, done: false }
    }

    /// The next response payload: `Some(Ok(_))` for a data frame, `None` at the
    /// OK trailer (or EOF), `Some(Err(_))` for a non-OK trailer or read error.
    pub async fn next(&mut self) -> Option<Result<Vec<u8>>> {
        if self.done {
            return None;
        }
        match read_frame(&self.recv).await {
            Ok(None) => {
                // Stream ended without a trailer — the handler panicked, dropped
                // its `Call` without finishing, or the connection broke. A
                // complete RPC always terminates with a trailer, so this is a
                // failure, not a clean end (mirrors the Python transport, which
                // errors on EOF-before-trailer).
                self.done = true;
                Some(Err(Error::Connection(
                    "RPC stream closed before trailer (incomplete response)".into(),
                )))
            }
            Ok(Some((payload, flags))) if flags & FLAG_TRAILER != 0 => {
                self.done = true;
                if payload.is_empty() {
                    return None; // success with no explicit status
                }
                match decode_rpc_status(&payload) {
                    Ok(status) if status.code == StatusCode::Ok.as_i32() => None,
                    Ok(status) => Some(Err(status_to_error(status))),
                    Err(e) => Some(Err(e.into())),
                }
            }
            Ok(Some((payload, _))) => Some(Ok(payload)),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
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
}

async fn write_header(send: &CoreSendStream, header: &StreamHeader) -> Result<()> {
    let frame = encode_frame(&encode_stream_header(header)?, FLAG_HEADER)?;
    send.write_all(frame).await?;
    Ok(())
}

/// Write request frames; the last carries `FLAG_END_STREAM`. Requires ≥1 request
/// (the reactor needs a first frame after the header). The empty-stream case is
/// a known gap until session/streaming refinements land.
async fn write_requests(send: &CoreSendStream, requests: Vec<Vec<u8>>) -> Result<()> {
    let n = requests.len();
    for (i, req) in requests.into_iter().enumerate() {
        let flags = if i + 1 == n { FLAG_END_STREAM } else { 0 };
        send.write_all(encode_frame(&req, flags)?).await?;
    }
    Ok(())
}

/// Read one length-prefixed frame from a `CoreRecvStream` using the public
/// `read_exact` (core's `read_one_frame` is crate-private). `Ok(None)` = EOF.
async fn read_frame(recv: &CoreRecvStream) -> Result<Option<(Vec<u8>, u8)>> {
    let len_bytes = match recv.read_exact(4).await {
        Ok(b) => b,
        Err(_) => return Ok(None), // stream ended
    };
    let frame_body_len =
        u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    if frame_body_len == 0 || frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(Error::Transport(anyhow::anyhow!(
            "invalid frame length {frame_body_len}"
        )));
    }
    let body = recv.read_exact(frame_body_len).await?;
    let flags = body[0];
    let payload = body[1..].to_vec();
    Ok(Some((payload, flags)))
}

/// Decode a unary trailer payload and convert a non-OK status into an error.
///
/// An empty trailer is an error, not success: an Aster RPC response always ends
/// with a non-empty `RpcStatus` trailer, so `unary_call` only yields an empty
/// trailer when the stream EOF'd after the response but before a real trailer
/// (e.g. the handler wrote a frame with [`Call::send`](super::server::Call::send)
/// then dropped the call). Treating that as OK would mask an incomplete response.
fn check_trailer(trailer_payload: &[u8]) -> Result<()> {
    if trailer_payload.is_empty() {
        return Err(Error::Connection(
            "RPC response missing trailer (incomplete response)".into(),
        ));
    }
    let status = decode_rpc_status(trailer_payload)?;
    if status.is_ok() {
        Ok(())
    } else {
        Err(status_to_error(status))
    }
}
