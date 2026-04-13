//! Per-call client surface for the multiplexed-streams architecture.
//!
//! Mirrors `ffi/src/call.rs` (the C ABI that the Java binding uses via
//! FFM) but as a direct PyO3 pyclass — Python links `core` through this
//! crate, so there is no C hop. Shape and lifecycle are identical:
//!
//! ```text
//!   AsterCall.acquire(conn, session_id)  → handle
//!     ├── send_frame(framed_bytes)*      # request frames
//!     ├── recv_frame(timeout_ms) → (payload, flags, kind)  # response frames
//!     ├── release()                       # success: stream returns to pool
//!     └── discard()                       # error: stream poisoned, slot freed
//! ```
//!
//! See `ffi_spec/Aster-multiplexed-streams.md` §5/§8 for the design.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::framing::MAX_FRAME_SIZE;
use aster_transport_core::pool::{AcquireError, PoolKey};
use aster_transport_core::{
    CoreRecvStream, CoreSendStream, MultiplexedStream, MultiplexedStreamHandle,
};

use crate::error::err_to_py;
use crate::net::IrohConnection;
use crate::PyBytesResult;

pyo3::create_exception!(aster, StreamAcquireError, pyo3::exceptions::PyException);

/// `recv_frame` result discriminators.
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

fn acquire_err_to_py(err: AcquireError) -> PyErr {
    let reason = acquire_reason(&err);
    let msg = format!("{reason}: {err}");
    Python::attach(|py| {
        let exc = StreamAcquireError::new_err(msg);
        if let Ok(bound) = exc.value(py).setattr("reason", reason) {
            let _ = bound;
        }
        exc
    })
}

enum RecvErr {
    Eof,
    Invalid,
}

async fn read_one_frame(recv: &CoreRecvStream) -> Result<(Vec<u8>, u8), RecvErr> {
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

/// The underlying stream for a call. Two shapes per spec §3:
/// - `Pooled` — handle into the per-connection `MultiplexedStreamPool`;
///   used for unary calls; RAII-returns to the pool on drop.
/// - `Streaming` — dedicated substream opened via `open_bi` that
///   bypasses the pool entirely; used for server-stream / client-stream
///   / bidi calls per spec §3 line 65 ("streaming substreams don't
///   count against any pool"). Closes on drop — no return-to-pool.
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
#[pyclass]
pub struct AsterCall {
    handle: Arc<StdMutex<Option<CallStream>>>,
}

#[pymethods]
impl AsterCall {
    /// Acquire a call handle from the connection's per-connection
    /// multiplexed-stream pool. `session_id == 0` selects the SHARED
    /// pool; any non-zero value selects the session pool keyed by that
    /// id. Raises `StreamAcquireError` on pool/quic/transport failure.
    #[staticmethod]
    fn acquire<'py>(
        py: Python<'py>,
        conn: &IrohConnection,
        session_id: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let core = conn.core_clone();
        future_into_py(py, async move {
            let key = pool_key_for(session_id);
            let handle = core.acquire_stream(key).await.map_err(acquire_err_to_py)?;
            Ok(AsterCall {
                handle: Arc::new(StdMutex::new(Some(CallStream::Pooled(handle)))),
            })
        })
    }

    /// Acquire a **streaming** call handle. Unlike `acquire`, this
    /// bypasses the per-connection pool entirely and opens a dedicated
    /// multiplexed substream via `open_bi` — per
    /// `ffi_spec/Aster-multiplexed-streams.md` §3 line 65, "streaming
    /// substreams don't count against any pool." Use this for
    /// server-stream / client-stream / bidi calls; use `acquire` for
    /// unary.
    ///
    /// `session_id` is not used here — the binding carries the session
    /// id in the `StreamHeader` it sends itself. Kept out of the
    /// signature to make the "no pool involvement" intent unambiguous.
    #[staticmethod]
    fn acquire_streaming<'py>(
        py: Python<'py>,
        conn: &IrohConnection,
    ) -> PyResult<Bound<'py, PyAny>> {
        let core = conn.core_clone();
        future_into_py(py, async move {
            let stream = core
                .open_streaming_substream()
                .await
                .map_err(|e| err_to_py(e.to_string()))?;
            Ok(AsterCall {
                handle: Arc::new(StdMutex::new(Some(CallStream::Streaming(stream)))),
            })
        })
    }

    /// Write a pre-framed byte slice onto the call's send side.
    /// `frame_bytes` must already include the 4-byte LE length prefix,
    /// the 1-byte flags, and the payload (see `aster.framing.write_frame`).
    fn send_frame<'py>(
        &self,
        py: Python<'py>,
        frame_bytes: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let send = self
            .clone_send()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("call already terminated"))?;
        future_into_py(py, async move {
            if frame_bytes.is_empty() {
                return Ok(());
            }
            send.write_all(frame_bytes).await.map_err(err_to_py)?;
            Ok(())
        })
    }

    /// Pull the next frame on the recv side. Returns a 3-tuple
    /// `(payload: bytes, flags: int, kind: int)` where `kind` is one of
    /// `RECV_OK` / `RECV_END_OF_STREAM` / `RECV_TIMEOUT`. On EOS or
    /// timeout the payload is empty and flags is 0.
    #[pyo3(signature = (timeout_ms=0))]
    fn recv_frame<'py>(
        &self,
        py: Python<'py>,
        timeout_ms: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let recv = self
            .clone_recv()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("call already terminated"))?;
        future_into_py(py, async move {
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
                    Err(_) => return Ok((PyBytesResult(Vec::new()), 0u8, RECV_TIMEOUT)),
                }
            };
            match result {
                Ok((payload, flags)) => Ok((PyBytesResult(payload), flags, RECV_OK)),
                Err(RecvErr::Eof) => Ok((PyBytesResult(Vec::new()), 0u8, RECV_END_OF_STREAM)),
                Err(RecvErr::Invalid) => Err(err_to_py("invalid frame on recv side")),
            }
        })
    }

    /// Success path. Drops the pool handle so its RAII return-to-pool
    /// fires. After release, send/recv on this call raise
    /// `RuntimeError: call already terminated`.
    ///
    /// MUST NOT be called while a concurrent `send_frame` or
    /// `recv_frame` is in flight on this call — the contract matches
    /// `ffi::aster_call_release`.
    fn release(&self) -> PyResult<()> {
        let mut guard = self
            .handle
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("call handle poisoned"))?;
        let _ = guard.take();
        Ok(())
    }

    /// Error path. Poisons the pool handle so the underlying stream is
    /// dropped (not returned to the pool) and the pool slot is freed.
    /// Parked waiters on the same key are woken with a retry signal.
    fn discard(&self) -> PyResult<()> {
        let mut guard = self
            .handle
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("call handle poisoned"))?;
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

pub fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AsterCall>()?;
    m.add("StreamAcquireError", py.get_type::<StreamAcquireError>())?;
    m.add("RECV_OK", RECV_OK)?;
    m.add("RECV_END_OF_STREAM", RECV_END_OF_STREAM)?;
    m.add("RECV_TIMEOUT", RECV_TIMEOUT)?;
    Ok(())
}
