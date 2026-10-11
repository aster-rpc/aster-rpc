//! Key-material arguments and Python value construction shared by the P7
//! modules (attestation, HPKE identity keys, grants, leases, topology).
//!
//! 32-byte inputs take the typed id class, raw `bytes`, or 64 hex
//! characters. Value types are frozen dataclasses defined in the Python
//! package (`aster.lease`, `aster.topology`, ...); [`py_value!`] builds one
//! from Rust by keyword.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::ids::{NodeId, PublicKey, SecretKey};

/// Build a Python value `module.cls(**{k: v, ...})`. Evaluates to
/// `PyResult<Bound<'py, PyAny>>`.
macro_rules! py_value {
    ($py:expr, $module:literal, $cls:literal { $($k:ident : $v:expr),* $(,)? }) => {{
        (|| -> pyo3::PyResult<pyo3::Bound<'_, pyo3::PyAny>> {
            let kw = pyo3::types::PyDict::new($py);
            $( kw.set_item(stringify!($k), $v)?; )*
            $py.import($module)?.getattr($cls)?.call((), Some(&kw))
        })()
    }};
}
pub(crate) use py_value;

pub(crate) fn invalid(msg: impl Into<String>) -> PyErr {
    PyValueError::new_err(msg.into())
}

/// A 32-byte value given as raw bytes or 64 hex characters.
pub(crate) fn key32(what: &str, obj: &Bound<'_, PyAny>) -> PyResult<[u8; 32]> {
    let raw = if let Ok(b) = obj.cast::<PyBytes>() {
        b.as_bytes().to_vec()
    } else if let Ok(s) = obj.extract::<String>() {
        hex::decode(s.trim())
            .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))?
    } else if let Ok(v) = obj.extract::<Vec<u8>>() {
        v
    } else {
        return Err(invalid(format!(
            "{what} must be 64 hex characters or 32 bytes"
        )));
    };
    raw.try_into()
        .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))
}

/// Exactly 32 raw bytes.
pub(crate) fn bytes32(what: &str, v: &[u8]) -> PyResult<[u8; 32]> {
    v.try_into()
        .map_err(|_| invalid(format!("{what} must be 32 bytes, got {}", v.len())))
}

/// A secret key: `SecretKey`, 32 bytes, or 64 hex characters.
pub(crate) fn secret_in(what: &str, obj: &Bound<'_, PyAny>) -> PyResult<aster::SecretKey> {
    if let Ok(k) = obj.cast::<SecretKey>() {
        return Ok(k.get().inner.clone());
    }
    Ok(aster::SecretKey::from_bytes(key32(what, obj)?))
}

/// A public key: `PublicKey`, `NodeId`, 32 bytes, or 64 hex characters.
pub(crate) fn public_in(what: &str, obj: &Bound<'_, PyAny>) -> PyResult<aster::PublicKey> {
    if let Ok(k) = obj.cast::<PublicKey>() {
        return Ok(k.get().inner);
    }
    if let Ok(id) = obj.cast::<NodeId>() {
        let raw = hex::decode(&id.get().hex).map_err(|e| invalid(e.to_string()))?;
        return Ok(aster::PublicKey::from_bytes(bytes32(what, &raw)?));
    }
    Ok(aster::PublicKey::from_bytes(key32(what, obj)?))
}

/// A node id as lowercase hex: `NodeId`, `PublicKey`, or a string (passed
/// through, lower-cased; the Rust side validates where it matters).
pub(crate) fn node_hex(obj: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(id) = obj.cast::<NodeId>() {
        return Ok(id.get().hex.clone());
    }
    if let Ok(k) = obj.cast::<PublicKey>() {
        return Ok(k.get().inner.to_hex());
    }
    obj.extract::<String>()
        .map(|s| s.trim().to_ascii_lowercase())
        .map_err(|_| invalid("expected a NodeId or a hex node id string"))
}

/// A lease actor (holder / writer / authority): a `NodeId` or `PublicKey`
/// (as hex), or any string, unchanged.
pub(crate) fn actor_in(obj: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(id) = obj.cast::<NodeId>() {
        return Ok(id.get().hex.clone());
    }
    if let Ok(k) = obj.cast::<PublicKey>() {
        return Ok(k.get().inner.to_hex());
    }
    obj.extract::<String>()
        .map_err(|_| invalid("expected a NodeId or a str actor id"))
}

/// A typed `NodeId` for a hex string the Rust side produced.
pub(crate) fn node_id_out(hex: impl Into<String>) -> NodeId {
    NodeId::from_trusted_hex(hex.into())
}

/// Bytes argument: `bytes` or `str` (UTF-8).
pub(crate) fn bytes_or_str(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(b.as_bytes().to_vec());
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(s.into_bytes());
    }
    obj.extract::<Vec<u8>>()
        .map_err(|_| invalid("expected bytes or str"))
}

/// Non-negative, finite seconds as a `Duration`.
pub(crate) fn secs_in(field: &str, secs: f64) -> PyResult<std::time::Duration> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(invalid(format!(
            "{field} must be a non-negative number of seconds"
        )));
    }
    std::time::Duration::try_from_secs_f64(secs)
        .map_err(|_| invalid(format!("{field} is outside the supported duration range")))
}
