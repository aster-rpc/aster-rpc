//! Per-call client surface for the multiplexed-streams architecture.
//!
//! napi-rs mirror of `bindings/python/rust/src/call.rs`. See
//! `ffi_spec/Aster-multiplexed-streams.md` §5/§8 for the design.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::framing::{FLAG_ROW_SCHEMA, FLAG_TRAILER, MAX_FRAME_SIZE};
use aster_transport_core::pool::{AcquireError, PoolKey};
use aster_transport_core::{
    CoreRecvStream, CoreSendStream, MultiplexedStream, MultiplexedStreamHandle,
};

use crate::net::IrohConnection;

/// `recvFrame` kind discriminators. Kept in sync with Python's
/// `RECV_OK` / `RECV_END_OF_STREAM` / `RECV_TIMEOUT`.
pub const RECV_OK: i32 = 0;
pub const RECV_END_OF_STREAM: i32 = 1;
pub const RECV_TIMEOUT: i32 = 2;

fn pool_key_for(session_id: u32) -> PoolKey {
    if session_id == 0 {
        None
    } else {
        Some(session_id.to_le_bytes().to_vec())
    }
}

fn acquire_reason(err: &AcquireError) -> &'static str {
    match err {
        AcquireError::PoolFull => "POOL_FULL",
        AcquireError::QuicLimitReached => "QUIC_LIMIT_REACHED",
        AcquireError::Timeout => "TIMEOUT",
        AcquireError::PeerStreamLimitTooLow { .. } => "PEER_STREAM_LIMIT_TOO_LOW",
        AcquireError::StreamOpenFailed(_) => "STREAM_OPEN_FAILED",
        AcquireError::Closed => "POOL_CLOSED",
        _ => "UNKNOWN",
    }
}

fn acquire_err_to_napi(err: AcquireError) -> Error {
    let reason = acquire_reason(&err);
    // Prefix the message with the reason so the TS wrapper can parse it
    // into a typed `StreamAcquireError` with a `reason` field. Matches the
    // Python side's `exc.reason` attribute.
    Error::from_reason(format!("StreamAcquireError:{reason}: {err}"))
}

enum RecvErr {
    Eof,
    Invalid,
}

async fn read_one_frame(recv: &CoreRecvStream) -> std::result::Result<(Vec<u8>, u8), RecvErr> {
    let mut len_bytes = [0u8; 4];
    if recv.read_exact_into(&mut len_bytes).await.is_err() {
        return Err(RecvErr::Eof);
    }
    let frame_body_len = u32::from_le_bytes(len_bytes) as usize;
    if frame_body_len == 0 || frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(RecvErr::Invalid);
    }
    let mut flags_buf = [0u8; 1];
    if recv.read_exact_into(&mut flags_buf).await.is_err() {
        return Err(RecvErr::Eof);
    }
    let flags = flags_buf[0];
    let payload_len = frame_body_len - 1;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 && recv.read_exact_into(&mut payload).await.is_err() {
        return Err(RecvErr::Eof);
    }
    Ok((payload, flags))
}

/// Result of a `recvFrame` call, returned as a JS object. `kind` is one
/// of `RECV_OK` / `RECV_END_OF_STREAM` / `RECV_TIMEOUT`; on EOS or timeout
/// `payload` is empty and `flags` is `0`.
#[napi(object)]
pub struct RecvFrameResult {
    pub payload: Buffer,
    pub flags: u8,
    pub kind: i32,
}

/// Result of `AsterCall.unaryFastPath`. Mirrors `aster_call_unary`'s
/// C-ABI out params in a JS-friendly shape.
///
/// - `response` is the first non-row-schema data frame's payload, or
///   an empty buffer when the dispatcher skipped straight to the
///   trailer (typical for error trailers).
/// - `responseFlags` is the flags byte on that data frame, or `0`
///   when `response` is empty.
/// - `trailer` is the trailer frame's payload (an encoded
///   `RpcStatus`). Empty means "clean OK trailer".
#[napi(object)]
pub struct UnaryFastPathResult {
    pub response: Buffer,
    pub response_flags: u8,
    pub trailer: Buffer,
}

/// The underlying stream for a call. Two shapes per spec §3:
/// - `Pooled` — handle into the per-connection `MultiplexedStreamPool`;
///   used for unary calls.
/// - `Streaming` — dedicated substream that bypasses the pool entirely;
///   used for server-stream / client-stream / bidi per spec §3 line 65
///   ("streaming substreams don't count against any pool").
enum CallStream {
    Pooled(MultiplexedStreamHandle),
    Streaming(MultiplexedStream),
}

impl CallStream {
    fn get(&self) -> &MultiplexedStream {
        match self {
            Self::Pooled(h) => h.get(),
            Self::Streaming(s) => s,
        }
    }

    fn discard(self) {
        match self {
            Self::Pooled(h) => h.discard(),
            Self::Streaming(_) => { /* drops */ }
        }
    }
}

/// Client-side per-call handle. Wraps either a pool-backed stream
/// (unary) or a dedicated streaming substream (server/client/bidi).
#[napi]
pub struct AsterCall {
    handle: Arc<StdMutex<Option<CallStream>>>,
}

#[napi]
impl AsterCall {
    /// Acquire a call handle from the connection's per-connection
    /// multiplexed-stream pool. `sessionId == 0` selects the SHARED
    /// pool; any non-zero value selects the session pool keyed by that
    /// id. Throws a `StreamAcquireError`-tagged error on pool/quic
    /// failure — parse the message prefix for the reason.
    #[napi(factory)]
    pub async fn acquire(conn: &IrohConnection, session_id: u32) -> Result<AsterCall> {
        let core = conn.core_clone();
        let key = pool_key_for(session_id);
        let handle = core
            .acquire_stream(key)
            .await
            .map_err(acquire_err_to_napi)?;
        Ok(AsterCall {
            handle: Arc::new(StdMutex::new(Some(CallStream::Pooled(handle)))),
        })
    }

    /// Acquire a **streaming** call handle. Unlike `acquire`, this
    /// bypasses the per-connection pool and opens a dedicated
    /// multiplexed substream via `open_bi` — per
    /// `ffi_spec/Aster-multiplexed-streams.md` §3 line 65, "streaming
    /// substreams don't count against any pool." Use this for
    /// server-stream / client-stream / bidi calls; use `acquire` for
    /// unary.
    ///
    /// `sessionId` is not used by this entry point — the binding
    /// carries the session id in the `StreamHeader` it sends itself.
    #[napi(factory)]
    pub async fn acquire_streaming(conn: &IrohConnection) -> Result<AsterCall> {
        let core = conn.core_clone();
        let stream = core
            .open_streaming_substream()
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(AsterCall {
            handle: Arc::new(StdMutex::new(Some(CallStream::Streaming(stream)))),
        })
    }

    /// **Unary fast-path** (spec §8). Collapses the full
    /// acquire → send header → send request → recv response →
    /// recv trailer → release sequence into ONE napi round-trip.
    ///
    /// `requestPair` is a single buffer holding the already-framed
    /// StreamHeader frame concatenated with the already-framed
    /// request frame (`[4B LE len][1B flags][payload]` twice). The
    /// Rust side writes the whole buffer in one `write_all` so Quinn
    /// sees one logical write, then reads frames from the same
    /// pooled stream until a `FLAG_TRAILER` frame appears. The first
    /// non-row-schema data frame is returned as the response; any
    /// extra data frames are ignored (unary contract).
    ///
    /// Why: v1 `IrohTransport.unary` does ~10 napi round-trips per
    /// unary call (openBi + 2 writeFrames + finish + 2x readFrame
    /// with 3 internal read_exact each). On macOS arm64 each hop is
    /// ~30-50 µs, so ~0.4 ms of the per-call budget is pure
    /// napi-boundary overhead. This method reduces it to ONE hop.
    /// Java already uses the equivalent FFI fast path via
    /// `ffi::aster_call_unary` and sees ~3k req/s vs TS's ~1k.
    ///
    /// On acquire failure the error carries the same
    /// `StreamAcquireError:<REASON>:` prefix as `AsterCall.acquire`.
    /// On transport failure the pool slot is discarded (poisoned)
    /// so parked waiters wake with a retry signal.
    #[napi]
    pub async fn unary_fast_path(
        conn: &IrohConnection,
        session_id: u32,
        request_pair: Buffer,
    ) -> Result<UnaryFastPathResult> {
        let core = conn.core_clone();
        let key = pool_key_for(session_id);
        let handle = core
            .acquire_stream(key)
            .await
            .map_err(acquire_err_to_napi)?;

        // Pull send/recv refs before any awaits so `handle` stays
        // owned by this scope (RAII drop returns the stream to the
        // pool on success, discards on failure).
        let (send, recv) = {
            let pair = handle.get();
            (pair.0.clone(), pair.1.clone())
        };

        let bytes = request_pair.to_vec();
        if bytes.is_empty() {
            handle.discard();
            return Err(Error::from_reason(
                "unaryFastPath: requestPair is empty".to_string(),
            ));
        }

        // 1. Single write_all for header+request.
        if let Err(e) = send.write_all(bytes).await {
            handle.discard();
            return Err(Error::from_reason(format!(
                "unaryFastPath: write_all failed: {e}"
            )));
        }

        // 2. Drain frames until trailer. First non-row-schema data
        //    frame is captured as the response; extras are ignored.
        let mut response: Option<(Vec<u8>, u8)> = None;
        let trailer_payload = loop {
            match read_one_frame(&recv).await {
                Ok((payload, flags)) => {
                    if flags & FLAG_TRAILER != 0 {
                        break payload;
                    }
                    if flags & FLAG_ROW_SCHEMA != 0 {
                        continue;
                    }
                    if response.is_none() {
                        response = Some((payload, flags));
                    }
                    // Extra data frames beyond the first are ignored.
                }
                Err(_) => {
                    handle.discard();
                    return Err(Error::from_reason(
                        "unaryFastPath: read failed before trailer".to_string(),
                    ));
                }
            }
        };

        // 3. Stream returns to the pool on drop (success path).
        drop(handle);

        let (resp_buf, resp_flags) = match response {
            Some((p, f)) => (Buffer::from(p), f),
            None => (Buffer::from(Vec::<u8>::new()), 0u8),
        };
        Ok(UnaryFastPathResult {
            response: resp_buf,
            response_flags: resp_flags,
            trailer: Buffer::from(trailer_payload),
        })
    }

    /// Write a pre-framed byte slice onto the call's send side.
    /// `frameBytes` must already include the 4-byte LE length prefix,
    /// the 1-byte flags, and the payload.
    #[napi]
    pub async fn send_frame(&self, frame_bytes: Buffer) -> Result<()> {
        let send = self
            .clone_send()
            .ok_or_else(|| Error::from_reason("call already terminated".to_string()))?;
        let bytes = frame_bytes.to_vec();
        if bytes.is_empty() {
            return Ok(());
        }
        send.write_all(bytes)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    }

    /// Pull the next frame on the recv side. Returns a `RecvFrameResult`
    /// where `kind` discriminates OK / end-of-stream / timeout. On EOS or
    /// timeout the payload is empty and flags is 0. `timeoutMs == 0`
    /// blocks indefinitely.
    #[napi]
    pub async fn recv_frame(&self, timeout_ms: Option<u32>) -> Result<RecvFrameResult> {
        let recv = self
            .clone_recv()
            .ok_or_else(|| Error::from_reason("call already terminated".to_string()))?;
        let timeout_ms = timeout_ms.unwrap_or(0);
        let result = if timeout_ms == 0 {
            read_one_frame(&recv).await
        } else {
            match tokio::time::timeout(
                Duration::from_millis(timeout_ms as u64),
                read_one_frame(&recv),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => {
                    return Ok(RecvFrameResult {
                        payload: Buffer::from(Vec::<u8>::new()),
                        flags: 0,
                        kind: RECV_TIMEOUT,
                    });
                }
            }
        };
        match result {
            Ok((payload, flags)) => Ok(RecvFrameResult {
                payload: Buffer::from(payload),
                flags,
                kind: RECV_OK,
            }),
            Err(RecvErr::Eof) => Ok(RecvFrameResult {
                payload: Buffer::from(Vec::<u8>::new()),
                flags: 0,
                kind: RECV_END_OF_STREAM,
            }),
            Err(RecvErr::Invalid) => Err(Error::from_reason("invalid frame on recv side")),
        }
    }

    /// Success path. Drops the pool handle so its RAII return-to-pool
    /// fires. After release, send/recv on this call throw
    /// `call already terminated`.
    ///
    /// MUST NOT be called while a concurrent `sendFrame` or `recvFrame`
    /// is in flight on this call — matches the Python contract.
    #[napi]
    pub fn release(&self) -> Result<()> {
        let mut guard = self
            .handle
            .lock()
            .map_err(|_| Error::from_reason("call handle poisoned".to_string()))?;
        let _ = guard.take();
        Ok(())
    }

    /// Error path. Poisons the pool handle so the underlying stream is
    /// dropped (not returned to the pool) and the pool slot is freed.
    /// Parked waiters on the same key are woken with a retry signal.
    #[napi]
    pub fn discard(&self) -> Result<()> {
        let mut guard = self
            .handle
            .lock()
            .map_err(|_| Error::from_reason("call handle poisoned".to_string()))?;
        if let Some(s) = guard.take() {
            s.discard();
        }
        Ok(())
    }
}

impl AsterCall {
    fn clone_send(&self) -> Option<CoreSendStream> {
        let guard = self.handle.lock().ok()?;
        guard.as_ref().map(|s| s.get().0.clone())
    }

    fn clone_recv(&self) -> Option<CoreRecvStream> {
        let guard = self.handle.lock().ok()?;
        guard.as_ref().map(|s| s.get().1.clone())
    }
}

/// Exported kind-tag constants for TS consumers.
#[napi]
pub const RECV_KIND_OK: i32 = RECV_OK;
#[napi]
pub const RECV_KIND_END_OF_STREAM: i32 = RECV_END_OF_STREAM;
#[napi]
pub const RECV_KIND_TIMEOUT: i32 = RECV_TIMEOUT;
