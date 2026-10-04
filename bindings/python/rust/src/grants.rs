//! Sealed grants -- wraps [`aster::grants`] (plan P7).
//!
//! The AAD layout, seal/open, the role rule, the reconcile decision and the
//! grant key all run in Rust (`aster_transport_core::grants`, vectors in
//! `conformance/vectors/grants.json`); nothing wire-defining is reimplemented
//! in Python. The private `_grant*` functions back `aster.grants` (which owns
//! the `GrantContext` / `GrantOutcome` / `GrantRecord` dataclasses);
//! [`PolicyDoc`] gives pinned-author reads over a replicated doc -- the
//! authenticity half HPKE Base does not provide.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;

use aster::grants::{self as g, GrantRecord, GrantState};
use aster::{AuthorId, NamespaceCapability, NodeId};

use crate::crypto::PyNamespaceCapability;
use crate::docs::{handle, DocEntry, DocEvent, DocEventReceiver, DocHandle, DocsClient};
use crate::error::aster_err_to_py;
use crate::keys::{bytes32, bytes_or_str, invalid, key32, node_hex, secret_in};
use crate::PyBytesResult;
use aster_transport_core::namespace::CoreNamespaceCapability;

/// Grant failures (bad envelope, wrong key or context, role mismatch,
/// malformed record) are `ValueError`s.
fn grant_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// An owned `GrantContext`, read off the Python dataclass by attribute.
struct Ctx {
    app: String,
    granter: NodeId,
    recipient: NodeId,
    resource: Vec<u8>,
    path: String,
    role: String,
}

impl<'a, 'py> FromPyObject<'a, 'py> for Ctx {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let get = |name: &str| obj.getattr(name);
        let resource = get("resource")?;
        let resource = resource
            .cast::<PyBytes>()
            .map(|b| b.as_bytes().to_vec())
            .or_else(|_| resource.extract::<Vec<u8>>())
            .map_err(|_| invalid("GrantContext.resource must be bytes"))?;
        Ok(Self {
            app: get("app")?.extract()?,
            granter: NodeId::from_hex(node_hex(&get("granter")?)?),
            recipient: NodeId::from_hex(node_hex(&get("recipient")?)?),
            resource,
            path: get("path")?.extract()?,
            role: get("role")?.extract()?,
        })
    }
}

impl Ctx {
    fn with<R>(&self, f: impl FnOnce(&g::GrantContext<'_>) -> R) -> R {
        f(&g::GrantContext {
            app: &self.app,
            granter: &self.granter,
            recipient: &self.recipient,
            resource: &self.resource,
            path: &self.path,
            role: &self.role,
        })
    }
}

fn capability_out(cap: &NamespaceCapability) -> PyResult<PyNamespaceCapability> {
    Ok(PyNamespaceCapability {
        inner: CoreNamespaceCapability::decode_fory(&cap.encode_fory()).map_err(grant_err)?,
    })
}

fn capability_in(cap: &PyNamespaceCapability) -> PyResult<NamespaceCapability> {
    NamespaceCapability::decode_fory(&cap.inner.encode_fory()).map_err(aster_err_to_py)
}

#[pyfunction]
#[pyo3(name = "_grant_aad")]
fn grant_aad(ctx: Ctx) -> PyBytesResult {
    PyBytesResult(ctx.with(|c| c.aad()))
}

#[pyfunction]
#[pyo3(name = "_seal_grant")]
fn seal_grant(ctx: Ctx, plaintext: Vec<u8>) -> PyResult<PyBytesResult> {
    ctx.with(|c| g::seal_grant(c, &plaintext))
        .map(PyBytesResult)
        .map_err(grant_err)
}

#[pyfunction]
#[pyo3(name = "_seal_grant_to_key")]
fn seal_grant_to_key(
    recipient_public_key: Vec<u8>,
    ctx: Ctx,
    plaintext: Vec<u8>,
) -> PyResult<PyBytesResult> {
    let key = bytes32("recipient public key", &recipient_public_key)?;
    ctx.with(|c| g::seal_grant_to_key(&key, c, &plaintext))
        .map(PyBytesResult)
        .map_err(grant_err)
}

#[pyfunction]
#[pyo3(name = "_open_grant")]
fn open_grant(
    identity_secret: &Bound<'_, PyAny>,
    ctx: Ctx,
    envelope: Vec<u8>,
) -> PyResult<PyBytesResult> {
    let secret = secret_in("identity secret", identity_secret)?;
    ctx.with(|c| g::open_grant(&secret, c, &envelope))
        .map(|p| PyBytesResult(p.into_secret()))
        .map_err(grant_err)
}

#[pyfunction]
#[pyo3(name = "_open_grant_with_key")]
fn open_grant_with_key(
    recipient_private_key: Vec<u8>,
    ctx: Ctx,
    envelope: Vec<u8>,
) -> PyResult<PyBytesResult> {
    let key = bytes32("recipient private key", &recipient_private_key)?;
    ctx.with(|c| g::open_grant_with_key(&key, c, &envelope))
        .map(|p| PyBytesResult(p.into_secret()))
        .map_err(grant_err)
}

#[pyfunction]
#[pyo3(name = "_seal_namespace_grant")]
fn seal_namespace_grant(
    ctx: Ctx,
    capability: PyRef<'_, PyNamespaceCapability>,
) -> PyResult<PyBytesResult> {
    let cap = capability_in(&capability)?;
    ctx.with(|c| g::seal_namespace_grant(c, &cap))
        .map(PyBytesResult)
        .map_err(grant_err)
}

#[pyfunction]
#[pyo3(name = "_open_namespace_grant")]
fn open_namespace_grant(
    identity_secret: &Bound<'_, PyAny>,
    ctx: Ctx,
    envelope: Vec<u8>,
) -> PyResult<PyNamespaceCapability> {
    let secret = secret_in("identity secret", identity_secret)?;
    let cap = ctx
        .with(|c| g::open_namespace_grant(&secret, c, &envelope))
        .map_err(grant_err)?;
    capability_out(&cap)
}

/// The role <-> capability rule: `"read"` needs a read capability, `"write"`
/// a write one; other roles carry no implication. Raises `ValueError` on a
/// mismatch.
#[pyfunction]
fn check_grant_role(role: &str, can_write: bool) -> PyResult<()> {
    aster_transport_core::grants::check_grant_role(role, can_write).map_err(grant_err)
}

/// The conventional grant doc key, `<prefix>/grants/<recipient>` -- an
/// address, not key material.
#[pyfunction]
fn grant_key(prefix: &str, recipient: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(g::grant_key(
        prefix,
        &NodeId::from_hex(node_hex(recipient)?),
    ))
}

/// `(kind, value, reason, role, error)` for `aster.grants.GrantOutcome`.
type OutcomeTuple = (
    &'static str,
    Option<Py<PyAny>>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn outcome_out<T>(
    o: g::GrantOutcome<T>,
    value: impl FnOnce(T) -> PyResult<Py<PyAny>>,
) -> PyResult<OutcomeTuple> {
    Ok(match o {
        g::GrantOutcome::Active(v) => ("active", Some(value(v)?), None, None, None),
        g::GrantOutcome::Revoked { reason, role } => {
            ("revoked", None, Some(reason), Some(role), None)
        }
        g::GrantOutcome::Unopenable(e) => ("unopenable", None, None, None, Some(e.to_string())),
        g::GrantOutcome::Absent => ("absent", None, None, None, None),
    })
}

/// Reconcile the record at `ctx.path` (`None` = no record); `payload` is
/// `"raw"` or `"namespace"`.
#[pyfunction]
#[pyo3(name = "_reconcile_grant")]
fn reconcile_grant(
    py: Python<'_>,
    identity_secret: &Bound<'_, PyAny>,
    ctx: Ctx,
    record: Option<Vec<u8>>,
    payload: &str,
) -> PyResult<OutcomeTuple> {
    let secret = secret_in("identity secret", identity_secret)?;
    let record = record.as_deref();
    match payload {
        "raw" => ctx.with(|c| {
            outcome_out(g::reconcile_grant(&secret, c, record), |p| {
                Ok(PyBytes::new(py, &p.into_secret()).into_any().unbind())
            })
        }),
        "namespace" => ctx.with(|c| {
            outcome_out(g::reconcile_namespace_grant(&secret, c, record), |cap| {
                Ok(Py::new(py, capability_out(&cap)?)?.into_any())
            })
        }),
        other => Err(invalid(format!(
            "payload must be 'raw' or 'namespace', got {other:?}"
        ))),
    }
}

/// Encode a grant record (Fory XLANG). An active record needs an envelope
/// and no reason; a revoked one must have no envelope.
#[pyfunction]
#[pyo3(name = "_grant_record_encode")]
fn grant_record_encode(
    state: &str,
    context_version: i32,
    role: String,
    envelope: Vec<u8>,
    reason: String,
) -> PyResult<PyBytesResult> {
    let rec = match state {
        "active" => {
            if envelope.is_empty() {
                return Err(invalid("active grant record must carry an envelope"));
            }
            if !reason.is_empty() {
                return Err(invalid("active grant record must not carry a reason"));
            }
            GrantRecord::active(role, context_version, envelope)
        }
        "revoked" => {
            if !envelope.is_empty() {
                return Err(invalid("revoked grant record must not carry an envelope"));
            }
            let mut rec = GrantRecord::revoked(role, reason);
            rec.context_version = context_version;
            rec
        }
        other => {
            return Err(invalid(format!(
                "grant record state must be 'active' or 'revoked', got {other:?}"
            )))
        }
    };
    Ok(PyBytesResult(rec.encode_fory()))
}

/// Decode grant record bytes -> `(state, context_version, role, envelope,
/// reason)` (validates version and the state/envelope rules).
#[pyfunction]
#[pyo3(name = "_grant_record_decode")]
fn grant_record_decode(
    bytes: Vec<u8>,
) -> PyResult<(&'static str, i32, String, PyBytesResult, String)> {
    let rec = GrantRecord::decode_fory(&bytes).map_err(grant_err)?;
    Ok((
        match rec.state {
            GrantState::Active => "active",
            GrantState::Revoked => "revoked",
        },
        rec.context_version,
        rec.role,
        PyBytesResult(rec.envelope),
        rec.reason,
    ))
}

// ============================================================================
// PolicyDoc
// ============================================================================

/// A replicated doc read through a **pinned author**: only records the
/// pinned author (the root's node id) wrote are visible. This is what makes
/// sealed grants trustworthy -- HPKE Base has no sender authenticity.
///
/// `await PolicyDoc.import_read_only(docs, namespace, author)` or
/// `PolicyDoc.wrap(doc, author)`.
#[pyclass(frozen, module = "aster._aster")]
pub struct PolicyDoc {
    inner: g::PolicyDoc,
}

fn author_in(obj: &Bound<'_, PyAny>) -> PyResult<AuthorId> {
    Ok(AuthorId::from_hex(node_hex(obj)?))
}

#[pymethods]
impl PolicyDoc {
    /// Import `namespace` (32 bytes or 64 hex) read-only (idempotent) and pin
    /// `author` (hex author id / `NodeId`). Awaitable.
    #[staticmethod]
    fn import_read_only<'py>(
        py: Python<'py>,
        docs: PyRef<'py, DocsClient>,
        namespace: &Bound<'py, PyAny>,
        author: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = docs.inner.clone();
        let namespace = aster::NamespaceId::from_bytes(key32("namespace id", namespace)?);
        let author = author_in(author)?;
        future_into_py(py, async move {
            let inner = g::PolicyDoc::import_read_only(&docs, namespace, author)
                .await
                .map_err(aster_err_to_py)?;
            Ok(PolicyDoc { inner })
        })
    }

    /// Wrap an already-opened doc (e.g. joined from a ticket) with a pinned
    /// author.
    #[staticmethod]
    fn wrap(doc: PyRef<'_, DocHandle>, author: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            inner: g::PolicyDoc::wrap(doc.inner.clone(), author_in(author)?),
        })
    }

    /// The pinned author (hex) all reads are scoped to.
    #[getter]
    fn author(&self) -> String {
        self.inner.author().to_string()
    }

    /// The underlying doc (e.g. to start sync or share).
    fn doc(&self) -> DocHandle {
        handle(self.inner.doc().clone())
    }

    /// The pinned author's record at `key` (bytes or str), or `None`.
    fn get<'py>(&self, py: Python<'py>, key: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let key = bytes_or_str(key)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(inner
                .get(key)
                .await
                .map_err(aster_err_to_py)?
                .map(PyBytesResult))
        })
    }

    /// The pinned author's entries under `prefix` (bytes or str).
    fn list_prefix<'py>(
        &self,
        py: Python<'py>,
        prefix: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let prefix = bytes_or_str(prefix)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(inner
                .list_prefix(prefix)
                .await
                .map_err(aster_err_to_py)?
                .into_iter()
                .map(DocEntry::from)
                .collect::<Vec<_>>())
        })
    }

    /// Live doc events (an async iterator); keep only
    /// `is_policy_write(ev)` ones.
    fn subscribe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(DocEventReceiver {
                inner: inner.subscribe().await.map_err(aster_err_to_py)?,
            })
        })
    }

    /// Whether `event` is an insert authored by the pinned author.
    fn is_policy_write(&self, event: PyRef<'_, DocEvent>) -> bool {
        matches!(event.kind.as_str(), "insert_local" | "insert_remote")
            && event
                .entry
                .as_ref()
                .is_some_and(|e| e.get().author_id == self.inner.author().as_str())
    }

    /// The raw grant record addressed to `recipient` at
    /// `<prefix>/grants/<recipient>`, or `None`.
    fn fetch_grant<'py>(
        &self,
        py: Python<'py>,
        prefix: String,
        recipient: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let recipient = NodeId::from_hex(node_hex(recipient)?);
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(inner
                .fetch_grant(&prefix, &recipient)
                .await
                .map_err(aster_err_to_py)?
                .map(PyBytesResult))
        })
    }

    fn __repr__(&self) -> String {
        format!("PolicyDoc(author='{}')", self.inner.author())
    }
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PolicyDoc>()?;
    m.add_function(wrap_pyfunction!(grant_aad, m)?)?;
    m.add_function(wrap_pyfunction!(seal_grant, m)?)?;
    m.add_function(wrap_pyfunction!(seal_grant_to_key, m)?)?;
    m.add_function(wrap_pyfunction!(open_grant, m)?)?;
    m.add_function(wrap_pyfunction!(open_grant_with_key, m)?)?;
    m.add_function(wrap_pyfunction!(seal_namespace_grant, m)?)?;
    m.add_function(wrap_pyfunction!(open_namespace_grant, m)?)?;
    m.add_function(wrap_pyfunction!(check_grant_role, m)?)?;
    m.add_function(wrap_pyfunction!(grant_key, m)?)?;
    m.add_function(wrap_pyfunction!(reconcile_grant, m)?)?;
    m.add_function(wrap_pyfunction!(grant_record_encode, m)?)?;
    m.add_function(wrap_pyfunction!(grant_record_decode, m)?)?;
    Ok(())
}
