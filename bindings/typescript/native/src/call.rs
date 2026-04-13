//! Per-call client surface for the multiplexed-streams architecture.
//!
//! napi-rs mirror of `bindings/python/rust/src/call.rs`. See
//! `ffi_spec/Aster-multiplexed-streams.md` §5/§8 for the design.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::framing::MAX_FRAME_SIZE;
use aster_transport_core::pool::{AcquireError, PoolKey};
use aster_transport_core::{CoreRecvStream, CoreSendStream, MultiplexedStreamHandle};

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

/// Client-side per-call handle over a pooled multiplexed bi-stream.
#[napi]
pub struct AsterCall {
    handle: Arc<StdMutex<Option<MultiplexedStreamHandle>>>,
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
        let handle = core.acquire_stream(key).await.map_err(acquire_err_to_napi)?;
        Ok(AsterCall {
            handle: Arc::new(StdMutex::new(Some(handle))),
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
        if let Some(h) = guard.take() {
            h.discard();
        }
        Ok(())
    }
}

impl AsterCall {
    fn clone_send(&self) -> Option<CoreSendStream> {
        let guard = self.handle.lock().ok()?;
        guard.as_ref().map(|h| h.get().0.clone())
    }

    fn clone_recv(&self) -> Option<CoreRecvStream> {
        let guard = self.handle.lock().ok()?;
        guard.as_ref().map(|h| h.get().1.clone())
    }
}

/// Exported kind-tag constants for TS consumers.
#[napi]
pub const RECV_KIND_OK: i32 = RECV_OK;
#[napi]
pub const RECV_KIND_END_OF_STREAM: i32 = RECV_END_OF_STREAM;
#[napi]
pub const RECV_KIND_TIMEOUT: i32 = RECV_TIMEOUT;
