//! Contract submodule — exposes core contract identity, framing, and signing
//! functions to Python via `_aster.contract`.

use pyo3::prelude::*;

use crate::PyBytesResult;

/// Compute contract_id from a ServiceContract JSON string.
/// Returns 64-char hex string.
#[pyfunction]
fn compute_contract_id_from_json(json_str: &str) -> PyResult<String> {
    aster_transport_core::contract::compute_contract_id_from_json(json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

/// Compute canonical bytes from a JSON-serialized type.
/// type_name: "ServiceContract", "TypeDef", or "MethodDef"
#[pyfunction]
fn canonical_bytes_from_json(type_name: &str, json_str: &str) -> PyResult<PyBytesResult> {
    let bytes = aster_transport_core::contract::canonical_bytes_from_json(type_name, json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(PyBytesResult(bytes))
}

/// Decode canonical XLANG bytes of a `ServiceContract`, `TypeDef`, or
/// `MethodDef` back to a JSON string. Dynamic clients that fetched
/// `types/{hash}.bin` blobs from a publisher use this to walk the
/// canonical type graph without reimplementing the reader in Python.
#[pyfunction]
fn canonical_bytes_to_json(type_name: &str, data: &[u8]) -> PyResult<String> {
    aster_transport_core::contract::canonical_bytes_to_json(type_name, data)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

/// BLAKE3 hash of input bytes -> 32-byte digest.
#[pyfunction]
fn compute_type_hash(data: &[u8]) -> PyBytesResult {
    let hash = aster_transport_core::contract::compute_type_hash(data);
    PyBytesResult(hash.to_vec())
}

/// Encode a frame: returns [4-byte LE length][flags][payload].
#[pyfunction]
fn encode_frame(payload: &[u8], flags: u8) -> PyResult<PyBytesResult> {
    let frame = aster_transport_core::framing::encode_frame(payload, flags)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(PyBytesResult(frame))
}

/// Decode a frame from bytes. Returns (payload_bytes, flags, bytes_consumed).
#[pyfunction]
fn decode_frame(data: &[u8]) -> PyResult<(PyBytesResult, u8, usize)> {
    let (payload, flags, consumed) = aster_transport_core::framing::decode_frame(data)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok((PyBytesResult(payload), flags, consumed))
}

/// Compute canonical signing bytes from credential JSON.
#[pyfunction]
fn canonical_signing_bytes_from_json(json_str: &str) -> PyResult<PyBytesResult> {
    let bytes = aster_transport_core::signing::canonical_signing_bytes_from_json(json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(PyBytesResult(bytes))
}

/// Canonical JSON: sorts keys, compact separators.
/// Input: JSON object string. Output: canonical JSON bytes.
#[pyfunction]
fn canonical_json(json_str: &str) -> PyResult<PyBytesResult> {
    let map: std::collections::BTreeMap<String, String> = serde_json::from_str(json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    let bytes = aster_transport_core::signing::canonical_json(&map);
    Ok(PyBytesResult(bytes))
}

/// Register all functions in _aster.contract submodule.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "contract")?;
    m.add_function(wrap_pyfunction!(compute_contract_id_from_json, &m)?)?;
    m.add_function(wrap_pyfunction!(canonical_bytes_from_json, &m)?)?;
    m.add_function(wrap_pyfunction!(canonical_bytes_to_json, &m)?)?;
    m.add_function(wrap_pyfunction!(compute_type_hash, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_frame, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_frame, &m)?)?;
    m.add_function(wrap_pyfunction!(canonical_signing_bytes_from_json, &m)?)?;
    m.add_function(wrap_pyfunction!(canonical_json, &m)?)?;
    parent.add_submodule(&m)?;
    Ok(())
}
