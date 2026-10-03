//! RPC envelope (AEv1) and payload compression — thin wrappers over
//! `aster_transport_core::{envelope, framing}`.
//!
//! The envelope types (`StreamHeader` / `CallHeader` / `RpcStatus`) are
//! encoded by core's hand-rolled AEv1 codec (Aster-SPEC §6.9), never by
//! Fory, so every binding produces identical bytes. zstd payload
//! compression (Aster-SPEC §5.6: threshold, level 3, only-if-smaller,
//! 16 MiB streaming decompression cap) also lives in core; these functions
//! expose it so the TypeScript binding has exactly one implementation on
//! Node and Bun alike.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::envelope as env;
use aster_transport_core::framing;

// ── constants ───────────────────────────────────────────────────────────────

/// Frame flag: payload is zstd-compressed.
#[napi]
pub const FLAG_COMPRESSED: u32 = framing::FLAG_COMPRESSED as u32;
/// Frame flag: trailing status frame (payload is an encoded `RpcStatus`).
#[napi]
pub const FLAG_TRAILER: u32 = framing::FLAG_TRAILER as u32;
/// Frame flag: stream header (payload is an encoded `StreamHeader`).
#[napi]
pub const FLAG_HEADER: u32 = framing::FLAG_HEADER as u32;
/// Frame flag: Fory row schema (reserved).
#[napi]
pub const FLAG_ROW_SCHEMA: u32 = framing::FLAG_ROW_SCHEMA as u32;
/// Frame flag: per-call header in a session stream (payload is a `CallHeader`).
#[napi]
pub const FLAG_CALL: u32 = framing::FLAG_CALL as u32;
/// Frame flag: cancel the current call.
#[napi]
pub const FLAG_CANCEL: u32 = framing::FLAG_CANCEL as u32;
/// Frame flag: last request frame of a call.
#[napi]
pub const FLAG_END_STREAM: u32 = framing::FLAG_END_STREAM as u32;
/// Maximum frame body size (flags byte + payload), bytes.
#[napi]
pub const MAX_FRAME_SIZE: u32 = framing::MAX_FRAME_SIZE;
/// Payloads strictly larger than this many bytes are compressed on send.
#[napi]
pub const COMPRESSION_THRESHOLD: u32 = framing::COMPRESSION_THRESHOLD as u32;
/// zstd level used on send.
#[napi]
pub const ZSTD_LEVEL: i32 = framing::ZSTD_LEVEL;
/// Largest size a compressed payload may expand to, bytes.
#[napi]
pub const MAX_DECOMPRESSED_SIZE: u32 = framing::MAX_DECOMPRESSED_SIZE as u32;

// ── envelope ────────────────────────────────────────────────────────────────

/// `StreamHeader` fields (first frame of every stream, `FLAG_HEADER`).
#[napi(object)]
pub struct NativeStreamHeader {
    pub service: String,
    pub method: String,
    pub version: i32,
    pub call_id: i32,
    /// Relative seconds, int16 range; `0` = no deadline.
    pub deadline: i32,
    /// int8 range: XLANG=0 (NATIVE=1, ROW=2 reserved).
    pub serialization_mode: i32,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
    /// `0` = stateless shared-pool stream; non-zero = session-bound.
    pub session_id: i32,
}

/// `CallHeader` fields (per-call header in a session stream, `FLAG_CALL`).
#[napi(object)]
pub struct NativeCallHeader {
    pub method: String,
    pub call_id: i32,
    /// Relative seconds, int16 range; `0` = no deadline.
    pub deadline: i32,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
}

/// `RpcStatus` fields (trailing status frame, `FLAG_TRAILER`).
#[napi(object)]
pub struct NativeRpcStatus {
    pub code: i32,
    pub message: String,
    pub detail_keys: Vec<String>,
    pub detail_values: Vec<String>,
}

fn reason(e: impl std::fmt::Display) -> Error {
    Error::from_reason(format!("{e:#}"))
}

fn narrow<T: TryFrom<i32>>(v: i32, field: &str) -> Result<T> {
    T::try_from(v).map_err(|_| reason(format!("envelope: {field} {v} out of range")))
}

/// Encode a `StreamHeader` to AEv1 bytes.
#[napi]
pub fn encode_stream_header(h: NativeStreamHeader) -> Result<Buffer> {
    let core = env::StreamHeader {
        service: h.service,
        method: h.method,
        version: h.version,
        call_id: h.call_id,
        deadline: narrow(h.deadline, "deadline")?,
        serialization_mode: narrow(h.serialization_mode, "serialization_mode")?,
        metadata_keys: h.metadata_keys,
        metadata_values: h.metadata_values,
        session_id: h.session_id,
    };
    env::encode_stream_header(&core)
        .map(Buffer::from)
        .map_err(reason)
}

/// Decode AEv1 bytes to a `StreamHeader`. Throws on any malformed input.
#[napi]
pub fn decode_stream_header(data: &[u8]) -> Result<NativeStreamHeader> {
    let h = env::decode_stream_header(data).map_err(reason)?;
    Ok(NativeStreamHeader {
        service: h.service,
        method: h.method,
        version: h.version,
        call_id: h.call_id,
        deadline: h.deadline.into(),
        serialization_mode: h.serialization_mode.into(),
        metadata_keys: h.metadata_keys,
        metadata_values: h.metadata_values,
        session_id: h.session_id,
    })
}

/// Encode a `CallHeader` to AEv1 bytes.
#[napi]
pub fn encode_call_header(h: NativeCallHeader) -> Result<Buffer> {
    let core = env::CallHeader {
        method: h.method,
        call_id: h.call_id,
        deadline: narrow(h.deadline, "deadline")?,
        metadata_keys: h.metadata_keys,
        metadata_values: h.metadata_values,
    };
    env::encode_call_header(&core)
        .map(Buffer::from)
        .map_err(reason)
}

/// Decode AEv1 bytes to a `CallHeader`. Throws on any malformed input.
#[napi]
pub fn decode_call_header(data: &[u8]) -> Result<NativeCallHeader> {
    let h = env::decode_call_header(data).map_err(reason)?;
    Ok(NativeCallHeader {
        method: h.method,
        call_id: h.call_id,
        deadline: h.deadline.into(),
        metadata_keys: h.metadata_keys,
        metadata_values: h.metadata_values,
    })
}

/// Encode an `RpcStatus` to AEv1 bytes.
#[napi]
pub fn encode_rpc_status(s: NativeRpcStatus) -> Result<Buffer> {
    let core = env::RpcStatus {
        code: s.code,
        message: s.message,
        detail_keys: s.detail_keys,
        detail_values: s.detail_values,
    };
    env::encode_rpc_status(&core)
        .map(Buffer::from)
        .map_err(reason)
}

/// Decode AEv1 bytes to an `RpcStatus`. Throws on any malformed input.
#[napi]
pub fn decode_rpc_status(data: &[u8]) -> Result<NativeRpcStatus> {
    let s = env::decode_rpc_status(data).map_err(reason)?;
    Ok(NativeRpcStatus {
        code: s.code,
        message: s.message,
        detail_keys: s.detail_keys,
        detail_values: s.detail_values,
    })
}

// ── payload compression ─────────────────────────────────────────────────────

/// zstd-compress `payload` if it is strictly larger than `threshold` bytes
/// (default [`COMPRESSION_THRESHOLD`]) and the result is smaller. Returns
/// `null` when the payload should be sent uncompressed.
#[napi]
pub fn maybe_compress(payload: &[u8], threshold: Option<u32>) -> Result<Option<Buffer>> {
    let t = threshold.map_or(framing::COMPRESSION_THRESHOLD, |t| t as usize);
    framing::maybe_compress(payload, t)
        .map(|o| o.map(Buffer::from))
        .map_err(reason)
}

/// Decompress a `FLAG_COMPRESSED` payload, refusing output larger than
/// `limit` bytes (default [`MAX_DECOMPRESSED_SIZE`]).
#[napi]
pub fn decompress_payload(data: &[u8], limit: Option<u32>) -> Result<Buffer> {
    let l = limit.map_or(framing::MAX_DECOMPRESSED_SIZE, |l| l as usize);
    framing::decompress_with_limit(data, l)
        .map(Buffer::from)
        .map_err(reason)
}

/// Encode a data frame (`[len LE u32][flags][payload]`), compressing the
/// payload and setting `FLAG_COMPRESSED` when it exceeds `threshold`
/// (default [`COMPRESSION_THRESHOLD`]). A caller-supplied `FLAG_COMPRESSED`
/// is ignored: core decides.
#[napi]
pub fn encode_payload_frame(payload: &[u8], flags: u8, threshold: Option<u32>) -> Result<Buffer> {
    let t = threshold.map_or(framing::COMPRESSION_THRESHOLD, |t| t as usize);
    framing::encode_payload_frame_with_threshold(payload, flags, t)
        .map(Buffer::from)
        .map_err(reason)
}

/// The logical payload of a received frame: decompressed (with the 16 MiB
/// cap) if `flags` carries `FLAG_COMPRESSED`, otherwise a copy of `payload`.
#[napi]
pub fn payload_of(payload: &[u8], flags: u8) -> Result<Buffer> {
    framing::payload_of(payload.to_vec(), flags)
        .map(Buffer::from)
        .map_err(reason)
}
