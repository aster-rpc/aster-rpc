//! Payload-level RPC client over `aster::rpc::RpcConnection` (design
//! `dynamic-dispatch-design.md` §3; plan T4).
//!
//! Rust builds the `StreamHeader`, frames, compresses, pools unary streams
//! and decodes trailers; JS passes the call coordinates plus request
//! payloads and gets response payloads back.
//!
//! ```text
//!   new RpcConnection(conn)
//!   conn.unary(service, method, version, …, request, cancel?) → Buffer
//!   conn.serverStream / clientStream / bidi(…) → ClientCall
//!     call.send(payload) / sendMany(payloads, last) / finish()
//!     call.next() → Buffer | null          (server-stream, bidi)
//!     call.response() → Buffer             (client-stream)
//!     call.cancel()
//! ```
//!
//! A failed call rejects with an `Error` whose message is
//! [`RPC_ERROR_PREFIX`] followed by `{"code", "message", "details"}` JSON;
//! the JS side turns it into an `RpcError`. Client-side failures map to
//! codes as in the C ABI: the deadline to `DEADLINE_EXCEEDED`, a cancel to
//! `CANCELLED`, a lost connection or pool exhaustion to `UNAVAILABLE`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::Notify;

use aster::rpc::{
    status_from_error, CallOptions, RequestSink, ResponseFuture, ResponseStream, RpcStatus,
    SerializationMode, StatusCode, StreamHeader,
};

use crate::net::IrohConnection;

/// Prefix of the message of an `Error` a failed call rejects with; JSON
/// `{code, message, details}` follows.
pub const RPC_ERROR_PREFIX: &str = "aster-rpc-status:";

/// The status a failed call reports (as the C ABI's `aster_rpc_*`).
fn error_status(e: &aster::Error) -> RpcStatus {
    match e {
        aster::Error::Rpc { .. } => status_from_error(e),
        aster::Error::Connection(_) | aster::Error::Transport(_) => {
            RpcStatus::error(StatusCode::Unavailable, e.to_string())
        }
        aster::Error::InvalidArgument(m) => RpcStatus::error(StatusCode::InvalidArgument, m),
        _ => status_from_error(e),
    }
}

fn status_err(st: &RpcStatus) -> Error {
    let details: serde_json::Map<String, serde_json::Value> = st
        .detail_keys
        .iter()
        .cloned()
        .zip(
            st.detail_values
                .iter()
                .map(|v| serde_json::Value::String(v.clone())),
        )
        .collect();
    let body = serde_json::json!({
        "code": st.code,
        "message": st.message,
        "details": details,
    });
    Error::from_reason(format!("{RPC_ERROR_PREFIX}{body}"))
}

pub(crate) fn rpc_err(e: aster::Error) -> Error {
    status_err(&error_status(&e))
}

fn cancelled_err() -> Error {
    status_err(&RpcStatus::error(StatusCode::Cancelled, "call cancelled"))
}

// ── cancellation ────────────────────────────────────────────────────────────

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelInner {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Resolves once cancelled (immediately if already).
    async fn wait(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// A cancellation handle for one unary call (wired to an `AbortSignal` by
/// the JS side). Cancelling a call that already finished does nothing.
#[napi]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[napi]
impl CancelToken {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        CancelToken {
            inner: Arc::new(CancelInner::default()),
        }
    }

    /// Cancel the call this token was passed to.
    #[napi]
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    #[napi(getter)]
    pub fn cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

// ── call options ────────────────────────────────────────────────────────────

/// Positional call coordinates, lowered to a header plus `CallOptions`.
#[allow(clippy::too_many_arguments)]
fn lower(
    service: String,
    method: String,
    version: i32,
    session_id: u32,
    deadline_ms: f64,
    metadata: Option<Vec<String>>,
    serialization_mode: Option<i32>,
    call_id: Option<i32>,
) -> Result<(StreamHeader, CallOptions)> {
    if service.is_empty() || method.is_empty() {
        return Err(Error::new(
            Status::InvalidArg,
            "service and method are required",
        ));
    }
    let header = StreamHeader {
        service,
        method,
        version,
        call_id: call_id.unwrap_or(0),
        session_id: 0,
        ..Default::default()
    };
    let mut options = CallOptions::new().session(session_id);
    match serialization_mode.unwrap_or(0) {
        0 => {}
        1 => options = options.serialization_mode(SerializationMode::Native),
        2 => options = options.serialization_mode(SerializationMode::Row),
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("unknown serialization mode {other}"),
            ))
        }
    }
    if deadline_ms.is_finite() && deadline_ms > 0.0 {
        options = options.deadline(Duration::from_secs_f64(deadline_ms / 1000.0));
    }
    if let Some(kv) = metadata {
        if kv.len() % 2 != 0 {
            return Err(Error::new(
                Status::InvalidArg,
                "metadata must be a flat [key, value, …] list",
            ));
        }
        let mut it = kv.into_iter();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            options.metadata.push((k, v));
        }
    }
    Ok((header, options))
}

// ── RpcConnection ───────────────────────────────────────────────────────────

/// A client over one `aster/1` connection: pooled unary calls, streaming
/// calls and session ids from the connection's shared counter.
#[napi]
pub struct RpcConnection {
    inner: aster::rpc::RpcConnection,
}

#[napi]
impl RpcConnection {
    /// Wrap a connection dialled on `aster/1`. Clients on the same
    /// connection share its stream pool and session-id counter.
    #[napi(constructor)]
    pub fn new(connection: &IrohConnection) -> Self {
        RpcConnection {
            inner: aster::rpc::RpcConnection::from_core_connection(connection.core_clone()),
        }
    }

    /// A fresh session id from the connection's counter (shared with every
    /// client on the connection, Rust included, so ids never collide).
    #[napi]
    pub fn next_session_id(&self) -> Result<u32> {
        self.inner
            .core_connection()
            .next_session_id()
            .ok_or_else(|| {
                status_err(&RpcStatus::error(
                    StatusCode::ResourceExhausted,
                    "session ids exhausted on this connection",
                ))
            })
    }

    /// Unary calls on the wire at once (the SHARED pool's stream count).
    #[napi(getter)]
    pub fn shared_pool_size(&self) -> u32 {
        self.inner.shared_pool_size() as u32
    }

    /// Change the SHARED pool size (minimum 1; connection-wide).
    #[napi]
    pub fn set_shared_pool_size(&self, size: u32) {
        self.inner.set_shared_pool_size(size.max(1) as usize);
    }

    /// Unary call on the pooled path. `deadlineMs` 0 = none; `metadata` is
    /// a flat `[key, value, …]` list; `serializationMode` 0 xlang, 1
    /// native, 2 row. Resolves with the response payload (empty when the
    /// handler responded with nothing).
    #[napi(
        ts_args_type = "service: string, method: string, version: number, sessionId: number, deadlineMs: number, metadata: Array<string> | undefined | null, serializationMode: number | undefined | null, callId: number | undefined | null, request: Uint8Array, cancel?: CancelToken | undefined | null",
        ts_return_type = "Promise<Buffer>"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn unary<'env>(
        &self,
        env: &'env Env,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline_ms: f64,
        metadata: Option<Vec<String>>,
        serialization_mode: Option<i32>,
        call_id: Option<i32>,
        request: Uint8ArraySlice,
        cancel: Option<&CancelToken>,
    ) -> Result<PromiseRaw<'env, Buffer>> {
        let (header, options) = lower(
            service,
            method,
            version,
            session_id,
            deadline_ms,
            metadata,
            serialization_mode,
            call_id,
        )?;
        let request = request.to_vec();
        let conn = self.inner.clone();
        let cancel = cancel.map(|c| c.inner.clone());
        env.spawn_future(async move {
            let call = conn.unary_with(&header, request, &options);
            let out = match cancel {
                None => call.await,
                Some(c) => tokio::select! {
                    r = call => r,
                    () = c.wait() => return Err(cancelled_err()),
                },
            };
            out.map(Buffer::from).map_err(rpc_err)
        })
    }

    /// Open a server-streaming call; `request` goes out with the header.
    #[napi(
        ts_args_type = "service: string, method: string, version: number, sessionId: number, deadlineMs: number, metadata: Array<string> | undefined | null, serializationMode: number | undefined | null, callId: number | undefined | null, request: Uint8Array",
        ts_return_type = "Promise<ClientCall>"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn server_stream<'env>(
        &self,
        env: &'env Env,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline_ms: f64,
        metadata: Option<Vec<String>>,
        serialization_mode: Option<i32>,
        call_id: Option<i32>,
        request: Uint8ArraySlice,
    ) -> Result<PromiseRaw<'env, ClientCall>> {
        let (header, options) = lower(
            service,
            method,
            version,
            session_id,
            deadline_ms,
            metadata,
            serialization_mode,
            call_id,
        )?;
        let request = request.to_vec();
        let conn = self.inner.clone();
        env.spawn_future(async move {
            let stream = conn
                .server_stream_with(&header, request, &options)
                .await
                .map_err(rpc_err)?;
            Ok(ClientCall::new(None, Some(stream), None))
        })
    }

    /// Open a client-streaming call: `send` requests, then await
    /// `response()`.
    #[napi(
        ts_args_type = "service: string, method: string, version: number, sessionId: number, deadlineMs: number, metadata: Array<string> | undefined | null, serializationMode: number | undefined | null, callId: number | undefined | null",
        ts_return_type = "Promise<ClientCall>"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn client_stream<'env>(
        &self,
        env: &'env Env,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline_ms: f64,
        metadata: Option<Vec<String>>,
        serialization_mode: Option<i32>,
        call_id: Option<i32>,
    ) -> Result<PromiseRaw<'env, ClientCall>> {
        let (header, options) = lower(
            service,
            method,
            version,
            session_id,
            deadline_ms,
            metadata,
            serialization_mode,
            call_id,
        )?;
        let conn = self.inner.clone();
        env.spawn_future(async move {
            let (sink, response) = conn
                .client_stream_open(&header, &options)
                .await
                .map_err(rpc_err)?;
            Ok(ClientCall::new(Some(sink), None, Some(response)))
        })
    }

    /// Open a bidirectional call: `send` and `next` concurrently.
    #[napi(
        ts_args_type = "service: string, method: string, version: number, sessionId: number, deadlineMs: number, metadata: Array<string> | undefined | null, serializationMode: number | undefined | null, callId: number | undefined | null",
        ts_return_type = "Promise<ClientCall>"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn bidi<'env>(
        &self,
        env: &'env Env,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline_ms: f64,
        metadata: Option<Vec<String>>,
        serialization_mode: Option<i32>,
        call_id: Option<i32>,
    ) -> Result<PromiseRaw<'env, ClientCall>> {
        let (header, options) = lower(
            service,
            method,
            version,
            session_id,
            deadline_ms,
            metadata,
            serialization_mode,
            call_id,
        )?;
        let conn = self.inner.clone();
        env.spawn_future(async move {
            let (sink, stream) = conn
                .bidi_open_with(&header, &options)
                .await
                .map_err(rpc_err)?;
            Ok(ClientCall::new(Some(sink), Some(stream), None))
        })
    }
}

// ── ClientCall ──────────────────────────────────────────────────────────────

/// One open streaming call. Dropping it (garbage collection) before it ends
/// cancels it.
#[napi]
pub struct ClientCall {
    sink: Option<Arc<RequestSink>>,
    stream: Arc<tokio::sync::Mutex<Option<ResponseStream>>>,
    response: Arc<tokio::sync::Mutex<Option<ResponseFuture>>>,
    cancel: Arc<CancelInner>,
}

impl ClientCall {
    fn new(
        sink: Option<RequestSink>,
        stream: Option<ResponseStream>,
        response: Option<ResponseFuture>,
    ) -> Self {
        ClientCall {
            sink: sink.map(Arc::new),
            stream: Arc::new(tokio::sync::Mutex::new(stream)),
            response: Arc::new(tokio::sync::Mutex::new(response)),
            cancel: Arc::new(CancelInner::default()),
        }
    }

    fn sink(&self) -> Result<Arc<RequestSink>> {
        self.sink.clone().ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "this call has no request stream (server-streaming)",
            )
        })
    }
}

#[napi]
impl ClientCall {
    /// Send one request payload (flushed at once).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn send<'env>(
        &self,
        env: &'env Env,
        payload: Uint8ArraySlice,
    ) -> Result<PromiseRaw<'env, ()>> {
        let sink = self.sink()?;
        let payload = payload.to_vec();
        env.spawn_future(async move { sink.send(payload).await.map_err(rpc_err) })
    }

    /// Send several request payloads in as few writes as possible; `last`
    /// ends the request stream with the final one.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn send_many<'env>(
        &self,
        env: &'env Env,
        payloads: Vec<Uint8Array>,
        last: bool,
    ) -> Result<PromiseRaw<'env, ()>> {
        let sink = self.sink()?;
        let payloads: Vec<Vec<u8>> = payloads.iter().map(|p| p.to_vec()).collect();
        env.spawn_future(async move { sink.send_many(payloads, last).await.map_err(rpc_err) })
    }

    /// End the request stream (idempotent). Fails with `INVALID_ARGUMENT`
    /// (and cancels) when no request was sent.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn finish<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        let sink = self.sink()?;
        env.spawn_future(async move { sink.finish().await.map_err(rpc_err) })
    }

    /// The next response payload; `null` at the OK trailer. Rejects with
    /// the call's status on a non-OK trailer, deadline, cancel or lost
    /// connection.
    #[napi(ts_return_type = "Promise<Buffer | null>")]
    pub fn next<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, Option<Buffer>>> {
        let stream = self.stream.clone();
        let cancel = self.cancel.clone();
        env.spawn_future(async move {
            let mut guard = stream.lock().await;
            let Some(s) = guard.as_mut() else {
                return Err(Error::new(
                    Status::InvalidArg,
                    "this call has no response stream (client-streaming)",
                ));
            };
            if cancel.is_cancelled() {
                return Err(cancelled_err());
            }
            let item = tokio::select! {
                item = s.next() => item,
                () = cancel.wait() => return Err(cancelled_err()),
            };
            match item {
                None => Ok(None),
                Some(Ok(payload)) => Ok(Some(Buffer::from(payload))),
                Some(Err(e)) => Err(rpc_err(e)),
            }
        })
    }

    /// Client-streaming: the single response (await after `finish`, or
    /// earlier: it resolves as soon as the server answers).
    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn response<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, Buffer>> {
        let response = self.response.clone();
        env.spawn_future(async move {
            let Some(fut) = response.lock().await.take() else {
                return Err(Error::new(
                    Status::InvalidArg,
                    "no single response on this call (or already awaited)",
                ));
            };
            fut.await.map(Buffer::from).map_err(rpc_err)
        })
    }

    /// Cancel the call (`FLAG_CANCEL`, then reset). Pending and later
    /// operations reject with `CANCELLED`. A no-op once the call ended.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn cancel<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        self.cancel.cancel();
        let sink = self.sink.clone();
        let stream = self.stream.clone();
        env.spawn_future(async move {
            // Client-streaming and bidi calls have a sink, which shares the
            // call state with the response half: cancelling it wakes a
            // pending read and resets the stream.
            if let Some(sink) = sink {
                sink.cancel().await;
                return Ok(());
            }
            // A pending `next` gave way to the cancel flag, so the lock frees.
            if let Some(s) = stream.lock().await.as_mut() {
                s.cancel().await;
            }
            Ok(())
        })
    }
}

/// The prefix of a failed call's error message (JSON status follows).
#[napi]
pub fn rpc_error_prefix() -> String {
    RPC_ERROR_PREFIX.to_string()
}
