//! `_aster.wire` — thin wrappers over core's RPC wire codec.
//!
//! Python no longer implements any wire byte layout itself:
//!
//! - the AEv1 envelope (`StreamHeader` / `CallHeader` / `RpcStatus`,
//!   Aster-SPEC §6.9) is `aster_transport_core::envelope`;
//! - payload compression (zstd, Aster-SPEC §5.6) and frame encoding are
//!   `aster_transport_core::framing`.
//!
//! The functions take and return plain bytes / tuples so the Python dataclasses
//! in `aster/wire.py` stay ordinary Python objects.

use aster_transport_core::envelope::{self, CallHeader, RpcStatus, StreamHeader};
use aster_transport_core::framing;
use pyo3::exceptions::{PyOverflowError, PyValueError};
use pyo3::prelude::*;

use crate::PyBytesResult;

fn value_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// `(service, method, version, call_id, deadline, serialization_mode,
///   metadata_keys, metadata_values, session_id)`
type StreamHeaderTuple = (
    String,
    String,
    i32,
    i32,
    i16,
    i8,
    Vec<String>,
    Vec<String>,
    i32,
);

/// Encode a `StreamHeader` (AEv1, tag `0xA1`).
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn encode_stream_header(
    service: String,
    method: String,
    version: i32,
    call_id: i32,
    deadline: i16,
    serialization_mode: i8,
    metadata_keys: Vec<String>,
    metadata_values: Vec<String>,
    session_id: i32,
) -> PyResult<PyBytesResult> {
    let h = StreamHeader {
        service,
        method,
        version,
        call_id,
        deadline,
        serialization_mode,
        metadata_keys,
        metadata_values,
        session_id,
    };
    envelope::encode_stream_header(&h)
        .map(PyBytesResult)
        .map_err(value_err)
}

/// Decode an AEv1 `StreamHeader`; raises `ValueError` on any malformed input.
#[pyfunction]
fn decode_stream_header(data: &[u8]) -> PyResult<StreamHeaderTuple> {
    let h = envelope::decode_stream_header(data).map_err(value_err)?;
    Ok((
        h.service,
        h.method,
        h.version,
        h.call_id,
        h.deadline,
        h.serialization_mode,
        h.metadata_keys,
        h.metadata_values,
        h.session_id,
    ))
}

/// `(method, call_id, deadline, metadata_keys, metadata_values)`
type CallHeaderTuple = (String, i32, i16, Vec<String>, Vec<String>);

/// `(code, message, detail_keys, detail_values)`
type RpcStatusTuple = (i32, String, Vec<String>, Vec<String>);

/// Encode a `CallHeader` (AEv1, tag `0xA2`).
#[pyfunction]
fn encode_call_header(
    method: String,
    call_id: i32,
    deadline: i16,
    metadata_keys: Vec<String>,
    metadata_values: Vec<String>,
) -> PyResult<PyBytesResult> {
    let h = CallHeader {
        method,
        call_id,
        deadline,
        metadata_keys,
        metadata_values,
    };
    envelope::encode_call_header(&h)
        .map(PyBytesResult)
        .map_err(value_err)
}

/// Decode an AEv1 `CallHeader` →
/// `(method, call_id, deadline, metadata_keys, metadata_values)`.
#[pyfunction]
fn decode_call_header(data: &[u8]) -> PyResult<CallHeaderTuple> {
    let h = envelope::decode_call_header(data).map_err(value_err)?;
    Ok((
        h.method,
        h.call_id,
        h.deadline,
        h.metadata_keys,
        h.metadata_values,
    ))
}

/// Encode an `RpcStatus` (AEv1, tag `0xA3`).
#[pyfunction]
fn encode_rpc_status(
    code: i32,
    message: String,
    detail_keys: Vec<String>,
    detail_values: Vec<String>,
) -> PyResult<PyBytesResult> {
    let s = RpcStatus {
        code,
        message,
        detail_keys,
        detail_values,
    };
    envelope::encode_rpc_status(&s)
        .map(PyBytesResult)
        .map_err(value_err)
}

/// Decode an AEv1 `RpcStatus` → `(code, message, detail_keys, detail_values)`.
#[pyfunction]
fn decode_rpc_status(data: &[u8]) -> PyResult<RpcStatusTuple> {
    let s = envelope::decode_rpc_status(data).map_err(value_err)?;
    Ok((s.code, s.message, s.detail_keys, s.detail_values))
}

/// Compress `payload` if it is strictly larger than `threshold` bytes and the
/// compressed form is smaller (Aster-SPEC §5.6). Returns the compressed bytes,
/// or `None` to send `payload` unchanged. A negative threshold disables
/// compression.
#[pyfunction]
#[pyo3(signature = (payload, threshold = framing::COMPRESSION_THRESHOLD as i64))]
fn maybe_compress(
    py: Python<'_>,
    payload: &[u8],
    threshold: i64,
) -> PyResult<Option<PyBytesResult>> {
    if threshold < 0 {
        return Ok(None);
    }
    let threshold = usize::try_from(threshold).unwrap_or(usize::MAX);
    if payload.len() <= threshold {
        return Ok(None);
    }
    let out = py
        .detach(|| framing::maybe_compress(payload, threshold))
        .map_err(value_err)?;
    Ok(out.map(PyBytesResult))
}

/// Streaming zstd decompression, refusing output larger than `limit` bytes
/// (default 16 MiB). Raises `OverflowError` when the limit is exceeded and
/// `ValueError` for corrupt input.
#[pyfunction]
#[pyo3(signature = (data, limit = framing::MAX_DECOMPRESSED_SIZE))]
fn decompress(py: Python<'_>, data: &[u8], limit: usize) -> PyResult<PyBytesResult> {
    py.detach(|| framing::decompress_with_limit(data, limit))
        .map(PyBytesResult)
        .map_err(|e| {
            let msg = e.to_string();
            // core reports the bomb case with this fixed prefix.
            if msg.starts_with("decompressed payload exceeds limit") {
                PyOverflowError::new_err(msg)
            } else {
                PyValueError::new_err(msg)
            }
        })
}

/// Encode a frame `[u32 LE length][flags][payload]`. Raises `ValueError` for
/// an empty payload outside the control frames (TRAILER, CANCEL, END_STREAM)
/// or a body over `MAX_FRAME_SIZE`.
#[pyfunction]
fn encode_frame(payload: &[u8], flags: u8) -> PyResult<PyBytesResult> {
    framing::encode_frame(payload, flags)
        .map(PyBytesResult)
        .map_err(value_err)
}

/// Decode one frame from the front of `data` →
/// `(payload, flags, bytes_consumed)`.
#[pyfunction]
fn decode_frame(data: &[u8]) -> PyResult<(PyBytesResult, u8, usize)> {
    let (payload, flags, consumed) = framing::decode_frame(data).map_err(value_err)?;
    Ok((PyBytesResult(payload), flags, consumed))
}

/// Register the `_aster.wire` submodule.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "wire")?;
    m.add_function(wrap_pyfunction!(encode_stream_header, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream_header, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_call_header, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_call_header, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_rpc_status, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_rpc_status, &m)?)?;
    m.add_function(wrap_pyfunction!(maybe_compress, &m)?)?;
    m.add_function(wrap_pyfunction!(decompress, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_frame, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_frame, &m)?)?;

    m.add("FLAG_COMPRESSED", framing::FLAG_COMPRESSED)?;
    m.add("FLAG_TRAILER", framing::FLAG_TRAILER)?;
    m.add("FLAG_HEADER", framing::FLAG_HEADER)?;
    m.add("FLAG_ROW_SCHEMA", framing::FLAG_ROW_SCHEMA)?;
    m.add("FLAG_CALL", framing::FLAG_CALL)?;
    m.add("FLAG_CANCEL", framing::FLAG_CANCEL)?;
    m.add("FLAG_END_STREAM", framing::FLAG_END_STREAM)?;
    m.add("FLAG_TUNNEL", framing::FLAG_TUNNEL)?;
    m.add("MAX_FRAME_SIZE", framing::MAX_FRAME_SIZE)?;
    m.add("COMPRESSION_THRESHOLD", framing::COMPRESSION_THRESHOLD)?;
    m.add("ZSTD_LEVEL", framing::ZSTD_LEVEL)?;
    m.add("MAX_DECOMPRESSED_SIZE", framing::MAX_DECOMPRESSED_SIZE)?;
    m.add("TAG_STREAM_HEADER", envelope::TAG_STREAM_HEADER)?;
    m.add("TAG_CALL_HEADER", envelope::TAG_CALL_HEADER)?;
    m.add("TAG_RPC_STATUS", envelope::TAG_RPC_STATUS)?;
    m.add("ENVELOPE_VERSION", envelope::ENVELOPE_VERSION)?;
    m.add("FORY_VERSION", aster_transport_core::xlang::FORY_VERSION)?;

    parent.add_submodule(&m)?;
    Ok(())
}
