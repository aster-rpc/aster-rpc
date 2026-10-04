//! Typed identifiers over `aster::{SecretKey, PublicKey, NodeId, Hash}`.
//!
//! `NodeId` and `Hash` compare (and hash) equal to their lowercase hex string,
//! so they can replace the hex strings the rest of the API still passes
//! without breaking dict lookups or equality checks. Pass `str(x)` where an
//! API still takes a hex string.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher};

use pyo3::basic::CompareOp;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::error::aster_err_to_py;

fn key32(data: &[u8], what: &str) -> PyResult<[u8; 32]> {
    data.try_into()
        .map_err(|_| PyValueError::new_err(format!("{what} must be 32 bytes, got {}", data.len())))
}

fn normalize_hex32(s: &str, what: &str) -> PyResult<String> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PyValueError::new_err(format!(
            "{what} must be 64 hex characters, got {s:?}"
        )));
    }
    Ok(s.to_ascii_lowercase())
}

fn str_hash(s: &str) -> isize {
    // Python requires a == b => hash(a) == hash(b). These ids compare equal to
    // their hex string, so hash them like Python hashes that string.
    Python::attach(|py| {
        pyo3::types::PyString::new(py, s)
            .hash()
            .unwrap_or_else(|_| {
                let mut h = DefaultHasher::new();
                s.hash(&mut h);
                h.finish() as isize
            })
    })
}

// ============================================================================
// SecretKey
// ============================================================================

/// A node secret key (32-byte Ed25519 seed). Its repr never shows the key.
///
/// The on-disk form is the **secret key file**: 64 hex characters plus a
/// newline (`read_file` / `write_file` / `load_or_generate`), shared with the
/// Rust SDK and every other binding.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct SecretKey {
    pub(crate) inner: aster::SecretKey,
}

#[pymethods]
impl SecretKey {
    /// Generate a fresh random key from the OS CSPRNG.
    #[staticmethod]
    fn generate() -> Self {
        Self {
            inner: aster::SecretKey::generate(),
        }
    }

    /// Construct from the raw 32 bytes.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        Ok(Self {
            inner: aster::SecretKey::from_bytes(key32(data, "secret key")?),
        })
    }

    /// Parse 64 hex characters.
    #[staticmethod]
    fn from_hex(s: &str) -> PyResult<Self> {
        aster::SecretKey::from_hex(s)
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    /// Read a secret key file. Raises `ValueError` if it is missing or
    /// malformed.
    #[staticmethod]
    fn read_file(path: std::path::PathBuf) -> PyResult<Self> {
        aster::SecretKey::read_file(path)
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    /// Like `read_file`, but `None` when the file does not exist.
    #[staticmethod]
    fn read_file_if_exists(path: std::path::PathBuf) -> PyResult<Option<Self>> {
        aster::SecretKey::read_file_if_exists(path)
            .map(|k| k.map(|inner| Self { inner }))
            .map_err(aster_err_to_py)
    }

    /// Load the key file at `path`, or generate a key and write it there.
    #[staticmethod]
    fn load_or_generate(path: std::path::PathBuf) -> PyResult<Self> {
        aster::SecretKey::load_or_generate(path)
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    /// Write this key as a secret key file (mode 0600, parents created).
    /// Raises `ValueError` if the file already exists.
    fn write_file(&self, path: std::path::PathBuf) -> PyResult<()> {
        self.inner.write_file(path).map_err(aster_err_to_py)
    }

    /// The raw 32 bytes.
    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.to_bytes())
    }

    /// The 64-char lowercase hex form (the key file's content).
    fn to_hex(&self) -> String {
        self.inner.to_hex()
    }

    /// The matching public key.
    fn public_key(&self) -> PublicKey {
        PublicKey {
            inner: aster::attestation::public_key(&self.inner),
        }
    }

    /// The node id a node started with this key has.
    fn node_id(&self) -> NodeId {
        NodeId {
            hex: aster::attestation::public_key(&self.inner).to_hex(),
        }
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        match other.cast::<SecretKey>() {
            Ok(o) => o.get().inner.to_bytes() == self.inner.to_bytes(),
            Err(_) => false,
        }
    }

    fn __hash__(&self) -> isize {
        // Hash the public half: equal keys hash equal, nothing secret leaks.
        str_hash(&aster::attestation::public_key(&self.inner).to_hex())
    }

    fn __repr__(&self) -> String {
        "SecretKey(<redacted>)".into()
    }
}

// ============================================================================
// PublicKey
// ============================================================================

/// A 32-byte Ed25519 public key (an identity / trust anchor).
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct PublicKey {
    pub(crate) inner: aster::PublicKey,
}

#[pymethods]
impl PublicKey {
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        Ok(Self {
            inner: aster::PublicKey::from_bytes(key32(data, "public key")?),
        })
    }

    #[staticmethod]
    fn from_hex(s: &str) -> PyResult<Self> {
        let hex = normalize_hex32(s, "public key")?;
        aster::PublicKey::from_hex(&hex)
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.to_bytes())
    }

    fn to_hex(&self) -> String {
        self.inner.to_hex()
    }

    /// The node id for this key (same 32 bytes, typed as a `NodeId`).
    fn node_id(&self) -> NodeId {
        NodeId {
            hex: self.inner.to_hex(),
        }
    }

    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        let eq = if let Ok(o) = other.cast::<PublicKey>() {
            Some(o.get().inner == self.inner)
        } else if let Ok(s) = other.extract::<String>() {
            Some(s.eq_ignore_ascii_case(&self.inner.to_hex()))
        } else {
            None
        };
        richcmp_eq(py, eq, op)
    }

    fn __hash__(&self) -> isize {
        str_hash(&self.inner.to_hex())
    }

    fn __str__(&self) -> String {
        self.inner.to_hex()
    }

    fn __repr__(&self) -> String {
        format!("PublicKey('{}')", self.inner.to_hex())
    }
}

fn richcmp_eq(py: Python<'_>, eq: Option<bool>, op: CompareOp) -> PyResult<Py<PyAny>> {
    match (eq, op) {
        (Some(eq), CompareOp::Eq) => Ok(eq.into_pyobject(py)?.to_owned().into_any().unbind()),
        (Some(eq), CompareOp::Ne) => Ok((!eq).into_pyobject(py)?.to_owned().into_any().unbind()),
        _ => Ok(py.NotImplemented()),
    }
}

// ============================================================================
// NodeId / Hash -- hex-backed ids
// ============================================================================

macro_rules! hex_id_class {
    ($name:ident, $what:literal, $doc:literal, { $($extra:tt)* }) => {
        #[doc = $doc]
        #[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
        #[derive(Clone)]
        pub struct $name {
            pub(crate) hex: String,
        }

        #[pymethods]
        impl $name {
            /// Parse a 64-char hex id (either case).
            #[new]
            fn new(hex: &str) -> PyResult<Self> {
                Ok(Self {
                    hex: normalize_hex32(hex, $what)?,
                })
            }

            #[staticmethod]
            fn from_hex(hex: &str) -> PyResult<Self> {
                Self::new(hex)
            }

            #[staticmethod]
            fn from_bytes(data: &[u8]) -> PyResult<Self> {
                Ok(Self {
                    hex: hex::encode(key32(data, $what)?),
                })
            }

            fn to_hex(&self) -> String {
                self.hex.clone()
            }

            fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
                let raw = hex::decode(&self.hex).map_err(|e| PyValueError::new_err(e.to_string()))?;
                Ok(PyBytes::new(py, &raw))
            }

            fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
                let py = other.py();
                let eq = if let Ok(o) = other.cast::<$name>() {
                    Some(o.get().hex == self.hex)
                } else if let Ok(s) = other.extract::<String>() {
                    Some(s.eq_ignore_ascii_case(&self.hex))
                } else {
                    None
                };
                richcmp_eq(py, eq, op)
            }

            fn __hash__(&self) -> isize {
                str_hash(&self.hex)
            }

            fn __str__(&self) -> String {
                self.hex.clone()
            }

            fn __repr__(&self) -> String {
                format!(concat!(stringify!($name), "('{}')"), self.hex)
            }

            $($extra)*
        }
    };
}

hex_id_class!(
    NodeId,
    "node id",
    "A node / endpoint id (the node's Ed25519 public key, 64 hex chars). \
     Equal to (and hashes like) its hex string.",
    {
        /// The public key this id is.
        fn public_key(&self) -> PyResult<PublicKey> {
            aster::PublicKey::from_hex(&self.hex)
                .map(|inner| PublicKey { inner })
                .map_err(aster_err_to_py)
        }
    }
);
hex_id_class!(
    Hash,
    "hash",
    "A BLAKE3 content hash (64 hex chars). Equal to (and hashes like) its \
     hex string.",
    {}
);

impl NodeId {
    pub(crate) fn from_trusted_hex(hex: String) -> Self {
        Self {
            hex: hex.to_ascii_lowercase(),
        }
    }
}
/// Extract a secret key from `SecretKey` or 32 raw bytes.
pub(crate) fn extract_secret_key(obj: &Bound<'_, PyAny>) -> PyResult<aster::SecretKey> {
    if let Ok(k) = obj.cast::<SecretKey>() {
        return Ok(k.get().inner.clone());
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(aster::SecretKey::from_bytes(key32(
            b.as_bytes(),
            "secret_key",
        )?));
    }
    Err(PyValueError::new_err(
        "secret_key must be a SecretKey or 32 bytes",
    ))
}

/// Extract a public key from `PublicKey`, 32 raw bytes, or 64 hex chars.
pub(crate) fn extract_public_key(obj: &Bound<'_, PyAny>) -> PyResult<aster::PublicKey> {
    if let Ok(k) = obj.cast::<PublicKey>() {
        return Ok(k.get().inner);
    }
    if let Ok(s) = obj.extract::<String>() {
        let hex = normalize_hex32(&s, "root_pubkey")?;
        return aster::PublicKey::from_hex(&hex).map_err(aster_err_to_py);
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(aster::PublicKey::from_bytes(key32(
            b.as_bytes(),
            "root_pubkey",
        )?));
    }
    Err(PyValueError::new_err(
        "root_pubkey must be a PublicKey, 32 bytes, or 64 hex characters",
    ))
}

/// A node id argument: a `NodeId` or a 64-hex string (normalized to
/// lowercase; anything else raises `ValueError`).
pub(crate) struct NodeIdArg(pub String);

impl<'a, 'py> FromPyObject<'a, 'py> for NodeIdArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(id) = obj.cast::<NodeId>() {
            return Ok(Self(id.get().hex.clone()));
        }
        let s: String = obj
            .extract()
            .map_err(|_| PyValueError::new_err("expected a NodeId or a 64-hex node id string"))?;
        Ok(Self(normalize_hex32(&s, "node id")?))
    }
}

/// A hex id argument for the data-plane APIs: a `Hash`, a `NodeId`, or a hex
/// string passed through unchanged (the Rust side validates it).
pub(crate) struct HexArg(pub String);

impl<'a, 'py> FromPyObject<'a, 'py> for HexArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(h) = obj.cast::<Hash>() {
            return Ok(Self(h.get().hex.clone()));
        }
        if let Ok(id) = obj.cast::<NodeId>() {
            return Ok(Self(id.get().hex.clone()));
        }
        let s: String = obj
            .extract()
            .map_err(|_| PyValueError::new_err("expected a Hash, a NodeId, or a hex string"))?;
        Ok(Self(s))
    }
}

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<SecretKey>()?;
    m.add_class::<PublicKey>()?;
    m.add_class::<NodeId>()?;
    m.add_class::<Hash>()?;
    Ok(())
}
