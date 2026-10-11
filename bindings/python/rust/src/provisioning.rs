//! Thin, runtime-free adapters over the shared provisioning implementation.
use crate::PyBytesResult;
use aster::binding_support::provisioning as shared;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn error(error: shared::ProvisioningError) -> PyErr {
    PyValueError::new_err(
        serde_json::json!({"code": error.code(), "detail": error.detail()}).to_string(),
    )
}
#[pyfunction]
fn _provisioning_canonical_genesis(json: &str) -> PyResult<PyBytesResult> {
    shared::canonical_genesis_json(json)
        .map(PyBytesResult)
        .map_err(error)
}
#[pyfunction]
fn _provisioning_canonical_join_permit(json: &str) -> PyResult<PyBytesResult> {
    shared::canonical_join_permit_json(json)
        .map(PyBytesResult)
        .map_err(error)
}
#[pyfunction]
fn _provisioning_genesis_signing_bytes(document_bytes: &[u8]) -> PyBytesResult {
    PyBytesResult(shared::genesis_signing_bytes(document_bytes))
}
#[pyfunction]
fn _provisioning_join_signing_bytes(document_bytes: &[u8]) -> PyBytesResult {
    PyBytesResult(shared::join_signing_bytes(document_bytes))
}
#[pyfunction]
fn _provisioning_genesis_hash(document_bytes: &[u8]) -> PyBytesResult {
    PyBytesResult(shared::genesis_hash(document_bytes).to_vec())
}
#[pyfunction]
fn _provisioning_signed_document_from_parts(
    document_bytes: &[u8],
    authority_public_key: &[u8],
    signature: &[u8],
) -> PyResult<PyBytesResult> {
    shared::signed_document_from_parts(document_bytes, authority_public_key, signature)
        .map(PyBytesResult)
        .map_err(error)
}
#[pyfunction]
fn _provisioning_canonical_candidate_ticket(node_id: &[u8]) -> PyResult<String> {
    shared::canonical_candidate_ticket(node_id).map_err(error)
}
#[pyfunction]
fn _provisioning_verify_genesis(
    authority_public_key: &[u8],
    authority_domain: &str,
    signed_bytes: &[u8],
) -> PyResult<String> {
    shared::verify_genesis_json(authority_public_key, authority_domain, signed_bytes).map_err(error)
}
#[pyfunction]
fn _provisioning_verify_join_permit_for_start(
    authority_public_key: &[u8],
    authority_domain: &str,
    candidate_node_id: &[u8],
    signed_bytes: &[u8],
) -> PyResult<String> {
    shared::verify_join_permit_for_start_json(
        authority_public_key,
        authority_domain,
        candidate_node_id,
        signed_bytes,
    )
    .map_err(error)
}
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_provisioning_canonical_genesis, m)?)?;
    m.add_function(wrap_pyfunction!(_provisioning_canonical_join_permit, m)?)?;
    m.add_function(wrap_pyfunction!(_provisioning_genesis_signing_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(_provisioning_join_signing_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(_provisioning_genesis_hash, m)?)?;
    m.add_function(wrap_pyfunction!(
        _provisioning_signed_document_from_parts,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        _provisioning_canonical_candidate_ticket,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(_provisioning_verify_genesis, m)?)?;
    m.add_function(wrap_pyfunction!(
        _provisioning_verify_join_permit_for_start,
        m
    )?)?;
    Ok(())
}
