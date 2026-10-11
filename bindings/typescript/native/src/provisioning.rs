//! Offline coordination bytes and verification; authority secrets never enter this adapter.
use aster::binding_support::provisioning as core;
use napi::bindgen_prelude::*;
use napi_derive::napi;

fn error(value: core::ProvisioningError) -> Error {
    Error::from_reason(format!(
        "ASTER_PROVISIONING:{}",
        serde_json::json!({"code": value.code(), "detail": value.detail()})
    ))
}

#[napi]
pub fn coordination_canonical_genesis(json: String) -> Result<Buffer> {
    core::canonical_genesis_json(&json)
        .map(Buffer::from)
        .map_err(error)
}
#[napi]
pub fn coordination_canonical_join_permit(json: String) -> Result<Buffer> {
    core::canonical_join_permit_json(&json)
        .map(Buffer::from)
        .map_err(error)
}
#[napi]
pub fn coordination_genesis_signing_bytes(document: Buffer) -> Buffer {
    core::genesis_signing_bytes(&document).into()
}
#[napi]
pub fn coordination_join_signing_bytes(document: Buffer) -> Buffer {
    core::join_signing_bytes(&document).into()
}
#[napi]
pub fn coordination_genesis_hash(document: Buffer) -> Buffer {
    core::genesis_hash(&document).to_vec().into()
}
#[napi]
pub fn coordination_signed_document_from_parts(
    document: Buffer,
    key: Buffer,
    signature: Buffer,
) -> Result<Buffer> {
    core::signed_document_from_parts(&document, &key, &signature)
        .map(Buffer::from)
        .map_err(error)
}
#[napi]
pub fn coordination_canonical_candidate_ticket(node: Buffer) -> Result<String> {
    core::canonical_candidate_ticket(&node).map_err(error)
}
#[napi]
pub fn coordination_verify_genesis(
    key: Buffer,
    domain: String,
    envelope: Buffer,
) -> Result<String> {
    core::verify_genesis_json(&key, &domain, &envelope).map_err(error)
}
#[napi]
pub fn coordination_verify_join_permit_for_start(
    key: Buffer,
    domain: String,
    candidate: Buffer,
    envelope: Buffer,
) -> Result<String> {
    core::verify_join_permit_for_start_json(&key, &domain, &candidate, &envelope).map_err(error)
}
