//! Contract identity + framing — exposes core canonical encoding,
//! BLAKE3 hashing, and frame encode/decode to JavaScript.

use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Compute contract_id from a ServiceContract JSON string.
/// Returns 64-char hex BLAKE3 hash.
#[napi]
pub fn compute_contract_id_from_json(json_str: String) -> Result<String> {
    aster_transport_core::contract::compute_contract_id_from_json(&json_str)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))
}

/// Compute canonical bytes from a JSON-serialized type.
/// type_name: "ServiceContract", "TypeDef", or "MethodDef"
#[napi]
pub fn canonical_bytes_from_json(type_name: String, json_str: String) -> Result<Buffer> {
    let bytes = aster_transport_core::contract::canonical_bytes_from_json(&type_name, &json_str)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))?;
    Ok(Buffer::from(bytes))
}

/// Decode canonical bytes of a `ServiceContract`, `TypeDef`, or
/// `MethodDef` back to the JSON form used by `canonical_bytes_from_json`.
/// Used by dynamic clients that need to walk the canonical type graph.
#[napi]
pub fn canonical_bytes_to_json(type_name: String, data: Buffer) -> Result<String> {
    aster_transport_core::contract::canonical_bytes_to_json(&type_name, &data)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))
}

/// BLAKE3 hash of input bytes -> 32-byte digest.
#[napi]
pub fn compute_type_hash(data: Buffer) -> Buffer {
    let hash = aster_transport_core::contract::compute_type_hash(&data);
    Buffer::from(hash.to_vec())
}

/// Compute canonical signing bytes from credential JSON.
#[napi]
pub fn canonical_signing_bytes_from_json(json_str: String) -> Result<Buffer> {
    let bytes = aster_transport_core::signing::canonical_signing_bytes_from_json(&json_str)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))?;
    Ok(Buffer::from(bytes))
}

/// Canonical JSON: sorts keys, compact separators.
#[napi]
pub fn canonical_json(json_str: String) -> Result<Buffer> {
    let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&json_str)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))?;
    let bytes = aster_transport_core::signing::canonical_json(&map);
    Ok(Buffer::from(bytes))
}

/// Encode a frame: returns [4-byte LE length][flags][payload].
#[napi]
pub fn encode_frame_native(payload: Buffer, flags: u8) -> Result<Buffer> {
    let frame = aster_transport_core::framing::encode_frame(&payload, flags)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))?;
    Ok(Buffer::from(frame))
}

/// Decode a frame from bytes. Returns { payload, flags, bytesConsumed }.
#[napi(object)]
pub struct DecodedFrame {
    pub payload: Buffer,
    pub flags: u8,
    pub bytes_consumed: u32,
}

#[napi]
pub fn decode_frame_native(data: Buffer) -> Result<DecodedFrame> {
    let (payload, flags, consumed) = aster_transport_core::framing::decode_frame(&data)
        .map_err(|e| napi::Error::from_reason(format!("{:#}", e)))?;
    Ok(DecodedFrame {
        payload: Buffer::from(payload),
        flags,
        bytes_consumed: consumed as u32,
    })
}
