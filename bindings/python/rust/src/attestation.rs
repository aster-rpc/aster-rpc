//! Ownership attestations -- wraps [`aster::attestation`] (plan P7).
//!
//! A chain is opaque wire bytes; `aster.attestation` wraps them in the
//! `AttestationChain` / `VerifiedChain` dataclasses. Verification is fully
//! offline. The native functions here are private (`_attest_*`).

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use aster::attestation as att;

use crate::keys::{public_in, secret_in};

create_exception!(
    aster,
    AttestationError,
    PyException,
    "An attestation chain failed to decode or verify."
);

fn att_err(e: aster::Error) -> PyErr {
    match e {
        aster::Error::InvalidArgument(msg) => pyo3::exceptions::PyValueError::new_err(msg),
        other => AttestationError::new_err(other.to_string()),
    }
}

/// Mint a single-edge root<->node chain; returns its wire bytes.
#[pyfunction]
#[pyo3(name = "_attest_root_node")]
fn attest_root_node<'py>(
    py: Python<'py>,
    root_secret: &Bound<'py, PyAny>,
    node_secret: &Bound<'py, PyAny>,
    epoch: i64,
    not_before: i64,
    not_after: i64,
) -> PyResult<Bound<'py, PyBytes>> {
    let root = secret_in("root secret", root_secret)?;
    let node = secret_in("node secret", node_secret)?;
    let opts = att::AttestOptions {
        epoch,
        not_before,
        not_after,
    };
    let chain = att::attest_root_node(&root, &node, &opts).map_err(att_err)?;
    Ok(PyBytes::new(py, chain.as_bytes()))
}

/// Verify `chain` offline. Returns `(node_hex, anchor_hex, [intermediate_hex],
/// depth)`; raises `AttestationError` on the first failure.
#[pyfunction]
#[pyo3(name = "_attestation_verify_chain")]
fn verify_chain(
    chain: Vec<u8>,
    trusted_anchors: Vec<Bound<'_, PyAny>>,
    expected_node: &Bound<'_, PyAny>,
) -> PyResult<(String, String, Vec<String>, usize)> {
    let anchors = trusted_anchors
        .iter()
        .map(|a| public_in("trusted anchor", a))
        .collect::<PyResult<Vec<_>>>()?;
    let expected = public_in("expected node", expected_node)?;
    let v =
        att::verify_chain(&att::Chain::from_bytes(chain), &anchors, &expected).map_err(att_err)?;
    Ok((
        v.node.to_hex(),
        v.anchor.to_hex(),
        v.intermediates.iter().map(|p| p.to_hex()).collect(),
        v.depth,
    ))
}

/// Chain bytes as `aster.attestation.chain.v1:<base64url>` text.
#[pyfunction]
#[pyo3(name = "_attestation_chain_to_text")]
fn chain_to_text(chain: Vec<u8>) -> String {
    att::Chain::from_bytes(chain).to_text()
}

/// Decode `aster.attestation.chain.v1:<base64url>` text into chain bytes.
#[pyfunction]
#[pyo3(name = "_attestation_chain_from_text")]
fn chain_from_text<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyBytes>> {
    let chain = att::Chain::from_text(text).map_err(att_err)?;
    Ok(PyBytes::new(py, chain.as_bytes()))
}

/// The Ed25519 public key (64 hex) of a 32-byte secret.
#[pyfunction]
#[pyo3(name = "_attestation_public_key")]
fn attestation_public_key(secret: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(att::public_key(&secret_in("secret", secret)?).to_hex())
}

pub(crate) fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("AttestationError", py.get_type::<AttestationError>())?;
    m.add_function(wrap_pyfunction!(attest_root_node, m)?)?;
    m.add_function(wrap_pyfunction!(verify_chain, m)?)?;
    m.add_function(wrap_pyfunction!(chain_to_text, m)?)?;
    m.add_function(wrap_pyfunction!(chain_from_text, m)?)?;
    m.add_function(wrap_pyfunction!(attestation_public_key, m)?)?;
    Ok(())
}
