use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;

use crate::PyBytesResult;

#[pyfunction]
fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

#[pyfunction]
fn blake3_digest(data: &[u8]) -> PyBytesResult {
    PyBytesResult(blake3::hash(data).as_bytes().to_vec())
}

#[pyfunction]
fn ed25519_generate_keypair() -> PyResult<(PyBytesResult, PyBytesResult)> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| PyValueError::new_err(format!("RNG error: {e}")))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let secret = signing_key.to_bytes().to_vec();
    let public = signing_key.verifying_key().to_bytes().to_vec();
    Ok((PyBytesResult(secret), PyBytesResult(public)))
}

#[pyfunction]
fn ed25519_public_from_secret(secret: &[u8]) -> PyResult<PyBytesResult> {
    let bytes: [u8; 32] = secret
        .try_into()
        .map_err(|_| PyValueError::new_err("secret key must be 32 bytes"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&bytes);
    Ok(PyBytesResult(signing_key.verifying_key().to_bytes().to_vec()))
}

#[pyfunction]
fn ed25519_sign(secret: &[u8], message: &[u8]) -> PyResult<PyBytesResult> {
    let bytes: [u8; 32] = secret
        .try_into()
        .map_err(|_| PyValueError::new_err("secret key must be 32 bytes"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&bytes);
    use ed25519_dalek::Signer;
    let sig = signing_key.sign(message);
    Ok(PyBytesResult(sig.to_bytes().to_vec()))
}

#[pyfunction]
fn ed25519_verify(pubkey: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(pk_bytes) = <[u8; 32]>::try_from(pubkey) else { return false };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(signature) else { return false };
    let Ok(verifying_key) = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes) else { return false };
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    use ed25519_dalek::Verifier;
    verifying_key.verify(message, &sig).is_ok()
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(blake3_hex, m)?)?;
    m.add_function(wrap_pyfunction!(blake3_digest, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_generate_keypair, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_public_from_secret, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_sign, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_verify, m)?)?;
    Ok(())
}
