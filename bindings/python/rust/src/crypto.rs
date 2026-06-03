use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::PyBytesResult;
use aster_transport_core::hpke_envelope::{
    hpke_generate_keypair as core_hpke_generate_keypair, hpke_open as core_hpke_open,
    hpke_public_key_from_private as core_hpke_public_key_from_private, hpke_seal as core_hpke_seal,
    HpkeEnvelope as CoreHpkeEnvelope, HPKE_ENVELOPE_ALG,
};
use aster_transport_core::namespace::{
    namespace_secret_id as core_namespace_secret_id, CoreNamespaceCapability,
};

fn checked_32(bytes: Vec<u8>, label: &str) -> PyResult<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| PyValueError::new_err(format!("{label} must be 32 bytes")))
}

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
    Ok(PyBytesResult(
        signing_key.verifying_key().to_bytes().to_vec(),
    ))
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
    let Ok(pk_bytes) = <[u8; 32]>::try_from(pubkey) else {
        return false;
    };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let Ok(verifying_key) = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    use ed25519_dalek::Verifier;
    verifying_key.verify(message, &sig).is_ok()
}

#[pyclass(name = "NamespaceCapability")]
struct PyNamespaceCapability {
    inner: CoreNamespaceCapability,
}

#[pymethods]
impl PyNamespaceCapability {
    #[staticmethod]
    fn read(namespace_id: Vec<u8>) -> PyResult<Self> {
        Ok(Self {
            inner: CoreNamespaceCapability::Read(checked_32(namespace_id, "namespace id")?),
        })
    }

    #[staticmethod]
    fn write(namespace_secret: Vec<u8>) -> PyResult<Self> {
        Ok(Self {
            inner: CoreNamespaceCapability::Write(checked_32(
                namespace_secret,
                "namespace secret",
            )?),
        })
    }

    #[staticmethod]
    fn decode_fory(data: &[u8]) -> PyResult<Self> {
        Ok(Self {
            inner: CoreNamespaceCapability::decode_fory(data).map_err(err_to_value)?,
        })
    }

    #[staticmethod]
    fn decode_canonical(data: &[u8]) -> PyResult<Self> {
        Self::decode_fory(data)
    }

    fn encode_fory(&self) -> PyBytesResult {
        PyBytesResult(self.inner.encode_fory())
    }

    fn encode_canonical(&self) -> PyBytesResult {
        self.encode_fory()
    }

    #[getter]
    fn kind(&self) -> &'static str {
        match self.inner {
            CoreNamespaceCapability::Read(_) => "read",
            CoreNamespaceCapability::Write(_) => "write",
        }
    }

    #[getter]
    fn can_write(&self) -> bool {
        self.inner.can_write()
    }

    fn namespace_id(&self) -> PyBytesResult {
        PyBytesResult(self.inner.namespace_id().to_vec())
    }

    /// Return the read id bytes for Read or the secret bytes for Write.
    /// Callers should avoid logging this value.
    fn material(&self) -> PyBytesResult {
        match &self.inner {
            CoreNamespaceCapability::Read(id) => PyBytesResult(id.to_vec()),
            CoreNamespaceCapability::Write(secret) => PyBytesResult(secret.to_vec()),
        }
    }

    fn namespace_secret(&self) -> Option<PyBytesResult> {
        match &self.inner {
            CoreNamespaceCapability::Read(_) => None,
            CoreNamespaceCapability::Write(secret) => Some(PyBytesResult(secret.to_vec())),
        }
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            CoreNamespaceCapability::Read(id) => {
                format!("NamespaceCapability.read({})", hex::encode(id))
            }
            CoreNamespaceCapability::Write(_) => {
                "NamespaceCapability.write(<redacted>)".to_string()
            }
        }
    }
}

#[pyclass(name = "HpkeEnvelope")]
struct PyHpkeEnvelope {
    inner: CoreHpkeEnvelope,
}

#[pymethods]
impl PyHpkeEnvelope {
    #[new]
    fn new(encapped_key: Vec<u8>, ciphertext: Vec<u8>) -> PyResult<Self> {
        if encapped_key.len() != 32 {
            return Err(PyValueError::new_err("HPKE encapped key must be 32 bytes"));
        }
        if ciphertext.len() < 16 {
            return Err(PyValueError::new_err(
                "HPKE ciphertext must include a 16-byte authentication tag",
            ));
        }
        Ok(Self {
            inner: CoreHpkeEnvelope::new(encapped_key, ciphertext),
        })
    }

    #[staticmethod]
    fn decode_fory(data: &[u8]) -> PyResult<Self> {
        Ok(Self {
            inner: CoreHpkeEnvelope::decode_fory(data).map_err(err_to_value)?,
        })
    }

    #[staticmethod]
    fn decode_canonical(data: &[u8]) -> PyResult<Self> {
        Self::decode_fory(data)
    }

    fn encode_fory(&self) -> PyBytesResult {
        PyBytesResult(self.inner.encode_fory())
    }

    fn encode_canonical(&self) -> PyBytesResult {
        self.encode_fory()
    }

    #[getter]
    fn alg(&self) -> &'static str {
        self.inner.alg()
    }

    #[getter]
    fn encapped_key(&self) -> PyBytesResult {
        PyBytesResult(self.inner.encapped_key.clone())
    }

    #[getter]
    fn ciphertext(&self) -> PyBytesResult {
        PyBytesResult(self.inner.ciphertext.clone())
    }

    fn __repr__(&self) -> String {
        format!(
            "HpkeEnvelope(alg={:?}, encapped_key_len={}, ciphertext_len={})",
            self.inner.alg(),
            self.inner.encapped_key.len(),
            self.inner.ciphertext.len()
        )
    }
}

fn err_to_value(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

#[pyfunction]
fn namespace_secret_id(secret: &[u8]) -> PyResult<PyBytesResult> {
    Ok(PyBytesResult(
        core_namespace_secret_id(secret)
            .map_err(err_to_value)?
            .to_vec(),
    ))
}

#[pyfunction]
fn hpke_envelope_alg() -> &'static str {
    HPKE_ENVELOPE_ALG
}

#[pyfunction]
fn hpke_generate_keypair() -> (PyBytesResult, PyBytesResult) {
    let keypair = core_hpke_generate_keypair();
    (
        PyBytesResult(keypair.private_key().to_vec()),
        PyBytesResult(keypair.public_key().to_vec()),
    )
}

#[pyfunction]
fn hpke_public_key_from_private(private_key: &[u8]) -> PyResult<PyBytesResult> {
    Ok(PyBytesResult(
        core_hpke_public_key_from_private(private_key)
            .map_err(err_to_value)?
            .to_vec(),
    ))
}

#[pyfunction]
fn hpke_seal(
    recipient_public_key: &[u8],
    associated_data: &[u8],
    plaintext: &[u8],
) -> PyResult<PyHpkeEnvelope> {
    Ok(PyHpkeEnvelope {
        inner: core_hpke_seal(recipient_public_key, associated_data, plaintext)
            .map_err(err_to_value)?,
    })
}

#[pyfunction]
fn hpke_open(
    recipient_private_key: &[u8],
    associated_data: &[u8],
    envelope: &PyHpkeEnvelope,
) -> PyResult<PyBytesResult> {
    let payload = core_hpke_open(recipient_private_key, associated_data, &envelope.inner)
        .map_err(err_to_value)?;
    Ok(PyBytesResult(payload.into_secret()))
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyNamespaceCapability>()?;
    m.add_class::<PyHpkeEnvelope>()?;
    m.add_function(wrap_pyfunction!(blake3_hex, m)?)?;
    m.add_function(wrap_pyfunction!(blake3_digest, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_generate_keypair, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_public_from_secret, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_sign, m)?)?;
    m.add_function(wrap_pyfunction!(ed25519_verify, m)?)?;
    m.add_function(wrap_pyfunction!(namespace_secret_id, m)?)?;
    m.add_function(wrap_pyfunction!(hpke_envelope_alg, m)?)?;
    m.add_function(wrap_pyfunction!(hpke_generate_keypair, m)?)?;
    m.add_function(wrap_pyfunction!(hpke_public_key_from_private, m)?)?;
    m.add_function(wrap_pyfunction!(hpke_seal, m)?)?;
    m.add_function(wrap_pyfunction!(hpke_open, m)?)?;
    Ok(())
}
