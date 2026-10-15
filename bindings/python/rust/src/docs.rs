//! Docs: `DocsClient` / `DocHandle` over the `aster` SDK's `Docs` / `Doc`.
//!
//! Typed values (`ShareMode`, `DownloadPolicy`, `LocalAccess`,
//! `AuthorityReport`, `AuthorityFingerprint`, `RetireOutcome`) mirror the
//! Rust SDK; the older string forms (`share("read")`,
//! `set_download_policy("nothing_except", prefixes)`) are still accepted.
//! Event receivers are async iterators (`async for ev in doc.events()`).
//! Hash / author / peer arguments take a typed id or a hex string; namespace
//! arguments take 32 bytes or 64 hex characters.

use std::time::Duration;

use pyo3::basic::CompareOp;
use pyo3::exceptions::{PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;

use crate::blobs::BlobStatusResult;
use crate::error::aster_err_to_py;
use crate::ids::HexArg;
use crate::node::Node;
use crate::PyBytesResult;

// ============================================================================
// Argument helpers
// ============================================================================

/// A namespace id argument: 32 bytes or 64 hex characters.
struct NamespaceArg(aster::NamespaceId);

impl<'a, 'py> FromPyObject<'a, 'py> for NamespaceArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(b) = obj.cast::<PyBytes>() {
            let raw: [u8; 32] = b.as_bytes().try_into().map_err(|_| {
                PyValueError::new_err(format!(
                    "namespace id must be 32 bytes, got {}",
                    b.as_bytes().len()
                ))
            })?;
            return Ok(Self(aster::NamespaceId::from_bytes(raw)));
        }
        let s: String = obj.extract().map_err(|_| {
            PyValueError::new_err("namespace id must be 32 bytes or 64 hex characters")
        })?;
        aster::NamespaceId::from_hex(s.trim())
            .map(Self)
            .map_err(aster_err_to_py)
    }
}

fn author(a: HexArg) -> aster::AuthorId {
    aster::AuthorId::from_hex(a.0)
}

fn hash(h: HexArg) -> aster::Hash {
    aster::Hash::from_hex(h.0)
}

// ============================================================================
// DocEntry / DocEvent
// ============================================================================

/// A document entry returned from queries, with metadata about who wrote it.
#[pyclass(frozen)]
pub struct DocEntry {
    #[pyo3(get)]
    pub author_id: String,
    #[pyo3(get)]
    pub key: Vec<u8>,
    #[pyo3(get)]
    pub content_hash: String,
    #[pyo3(get)]
    pub content_len: u64,
    #[pyo3(get)]
    pub timestamp: u64,
}

impl From<aster::DocEntry> for DocEntry {
    fn from(e: aster::DocEntry) -> Self {
        Self {
            author_id: e.author.into_string(),
            key: e.key,
            content_hash: e.content_hash.into_string(),
            content_len: e.content_len,
            timestamp: e.timestamp,
        }
    }
}

/// A live document event. Check `kind` to see which fields are populated.
///
/// kind values:
///   "insert_local"     -- entry written by this node  (entry populated)
///   "insert_remote"    -- entry received from peer    (entry + from_peer populated)
///   "content_ready"    -- blob now available locally  (hash populated)
///   "pending_content_ready" -- all queued downloads done
///   "neighbor_up"      -- peer joined the swarm       (peer populated)
///   "neighbor_down"    -- peer left the swarm         (peer populated)
///   "sync_finished"    -- sync round with peer ended  (peer, sync_origin, sync_ok
///                         populated; sync_error too when the round failed)
#[pyclass(frozen)]
pub struct DocEvent {
    #[pyo3(get)]
    pub kind: String,
    #[pyo3(get)]
    pub entry: Option<Py<DocEntry>>,
    #[pyo3(get)]
    pub from_peer: Option<String>,
    #[pyo3(get)]
    pub hash: Option<String>,
    #[pyo3(get)]
    pub peer: Option<String>,
    /// `sync_finished`: "accept" (the peer dialled us) or "connect" (we dialled).
    #[pyo3(get)]
    pub sync_origin: Option<String>,
    /// `sync_finished`: whether the round completed.
    #[pyo3(get)]
    pub sync_ok: Option<bool>,
    /// `sync_finished`: why the round failed; None when it completed.
    #[pyo3(get)]
    pub sync_error: Option<String>,
}

fn event_to_py(py: Python<'_>, ev: aster::DocEvent) -> PyResult<DocEvent> {
    let mut out = DocEvent {
        kind: String::new(),
        entry: None,
        from_peer: None,
        hash: None,
        peer: None,
        sync_origin: None,
        sync_ok: None,
        sync_error: None,
    };
    match ev {
        aster::DocEvent::InsertLocal { entry } => {
            out.kind = "insert_local".into();
            out.entry = Some(Py::new(py, DocEntry::from(entry))?);
        }
        aster::DocEvent::InsertRemote { from, entry } => {
            out.kind = "insert_remote".into();
            out.entry = Some(Py::new(py, DocEntry::from(entry))?);
            out.from_peer = Some(from.into_string());
        }
        aster::DocEvent::ContentReady { hash } => {
            out.kind = "content_ready".into();
            out.hash = Some(hash.into_string());
        }
        aster::DocEvent::PendingContentReady => out.kind = "pending_content_ready".into(),
        aster::DocEvent::NeighborUp { peer } => {
            out.kind = "neighbor_up".into();
            out.peer = Some(peer.into_string());
        }
        aster::DocEvent::NeighborDown { peer } => {
            out.kind = "neighbor_down".into();
            out.peer = Some(peer.into_string());
        }
        aster::DocEvent::SyncFinished {
            peer,
            origin,
            result,
        } => {
            out.kind = "sync_finished".into();
            out.peer = Some(peer.into_string());
            out.sync_origin = Some(
                match origin {
                    aster::SyncOrigin::Accept => "accept",
                    aster::SyncOrigin::Connect => "connect",
                }
                .into(),
            );
            out.sync_ok = Some(result.is_ok());
            out.sync_error = result.err();
        }
    }
    Ok(out)
}

// ============================================================================
// DocEventReceiver (async iterator)
// ============================================================================

/// Live document events: `async for ev in receiver`, or `await recv()`
/// (`None` when the subscription ends).
#[pyclass(frozen)]
pub struct DocEventReceiver {
    pub(crate) inner: aster::DocEvents,
}

#[pymethods]
impl DocEventReceiver {
    /// Receive the next live document event. Returns None when the subscription ends.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.inner.clone();
        future_into_py(py, async move {
            match rx.recv().await.map_err(aster_err_to_py)? {
                None => Ok(None::<DocEvent>),
                Some(ev) => Ok(Some(Python::attach(|py| event_to_py(py, ev))?)),
            }
        })
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.inner.clone();
        future_into_py(py, async move {
            match rx.recv().await.map_err(aster_err_to_py)? {
                None => Err(PyStopAsyncIteration::new_err(())),
                Some(ev) => Python::attach(|py| event_to_py(py, ev)),
            }
        })
    }
}

// ============================================================================
// Typed values
// ============================================================================

/// Read or write share capability.
#[pyclass(eq, eq_int, frozen, from_py_object, module = "aster._aster")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareMode {
    #[pyo3(name = "READ")]
    Read,
    #[pyo3(name = "WRITE")]
    Write,
}

/// A share-mode argument: `ShareMode` or `"read"` / `"write"`.
struct ShareModeArg(aster::ShareMode);

impl<'a, 'py> FromPyObject<'a, 'py> for ShareModeArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(m) = obj.cast::<ShareMode>() {
            return Ok(Self(match *m.get() {
                ShareMode::Read => aster::ShareMode::Read,
                ShareMode::Write => aster::ShareMode::Write,
            }));
        }
        let s: String = obj
            .extract()
            .map_err(|_| PyValueError::new_err("mode must be a ShareMode or 'read' / 'write'"))?;
        match s.as_str() {
            "read" => Ok(Self(aster::ShareMode::Read)),
            "write" => Ok(Self(aster::ShareMode::Write)),
            other => Err(PyValueError::new_err(format!(
                "mode must be 'read' or 'write', got {other:?}"
            ))),
        }
    }
}

/// What access a namespace grants this node locally.
#[pyclass(eq, eq_int, frozen, from_py_object, module = "aster._aster")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalAccess {
    #[pyo3(name = "WRITE")]
    Write,
    #[pyo3(name = "READ")]
    Read,
}

impl From<aster::LocalAccess> for LocalAccess {
    fn from(a: aster::LocalAccess) -> Self {
        match a {
            aster::LocalAccess::Write => LocalAccess::Write,
            aster::LocalAccess::Read => LocalAccess::Read,
        }
    }
}

impl From<LocalAccess> for aster::LocalAccess {
    fn from(a: LocalAccess) -> Self {
        match a {
            LocalAccess::Write => aster::LocalAccess::Write,
            LocalAccess::Read => aster::LocalAccess::Read,
        }
    }
}

/// Which remote entries a document downloads automatically.
///
/// `mode` is `"everything"`, `"nothing_except"` or `"everything_except"`;
/// `prefixes` are the key prefixes the exception applies to.
#[pyclass(frozen, from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct DownloadPolicy {
    #[pyo3(get)]
    pub mode: String,
    #[pyo3(get)]
    pub prefixes: Vec<Vec<u8>>,
}

impl DownloadPolicy {
    fn to_sdk(&self) -> PyResult<aster::DownloadPolicy> {
        policy_from_parts(&self.mode, self.prefixes.clone())
    }
}

fn policy_from_parts(mode: &str, prefixes: Vec<Vec<u8>>) -> PyResult<aster::DownloadPolicy> {
    match mode {
        "everything" => Ok(aster::DownloadPolicy::Everything),
        "nothing_except" => Ok(aster::DownloadPolicy::NothingExcept(prefixes)),
        "everything_except" => Ok(aster::DownloadPolicy::EverythingExcept(prefixes)),
        _ => Err(PyValueError::new_err(format!(
            "mode must be 'everything', 'nothing_except', or 'everything_except', got '{mode}'"
        ))),
    }
}

impl From<aster::DownloadPolicy> for DownloadPolicy {
    fn from(p: aster::DownloadPolicy) -> Self {
        let (mode, prefixes) = match p {
            aster::DownloadPolicy::Everything => ("everything", vec![]),
            aster::DownloadPolicy::NothingExcept(p) => ("nothing_except", p),
            aster::DownloadPolicy::EverythingExcept(p) => ("everything_except", p),
        };
        Self {
            mode: mode.to_string(),
            prefixes,
        }
    }
}

#[pymethods]
impl DownloadPolicy {
    #[new]
    #[pyo3(signature = (mode="everything", prefixes=Vec::new()))]
    fn new(mode: &str, prefixes: Vec<Vec<u8>>) -> PyResult<Self> {
        Ok(policy_from_parts(mode, prefixes)?.into())
    }

    /// Download everything (the default).
    #[staticmethod]
    fn everything() -> Self {
        aster::DownloadPolicy::Everything.into()
    }

    /// Download only entries whose keys start with one of `prefixes`.
    #[staticmethod]
    fn nothing_except(prefixes: Vec<Vec<u8>>) -> Self {
        aster::DownloadPolicy::NothingExcept(prefixes).into()
    }

    /// Download everything except entries whose keys start with one of `prefixes`.
    #[staticmethod]
    fn everything_except(prefixes: Vec<Vec<u8>>) -> Self {
        aster::DownloadPolicy::EverythingExcept(prefixes).into()
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .cast::<DownloadPolicy>()
            .map(|o| o.get().mode == self.mode && o.get().prefixes == self.prefixes)
            .unwrap_or(false)
    }

    fn __repr__(&self) -> String {
        format!(
            "DownloadPolicy(mode='{}', prefixes={})",
            self.mode,
            self.prefixes.len()
        )
    }
}

/// A stable, opaque correlation value over `(namespace, stored access)`:
/// BLAKE3 over a versioned domain, the namespace id and one access byte.
/// Correlation, not evidence -- see the Rust `AuthorityFingerprint` docs.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct AuthorityFingerprint {
    inner: aster::AuthorityFingerprint,
}

#[pymethods]
impl AuthorityFingerprint {
    /// Fingerprint a namespace (32 bytes or hex) and access pair, e.g. to
    /// compare a grant record against an `AuthorityReport`.
    #[staticmethod]
    fn of(namespace: NamespaceArg, access: LocalAccess) -> Self {
        Self {
            inner: aster::AuthorityFingerprint::of(namespace.0, access.into()),
        }
    }

    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.as_bytes())
    }

    fn to_hex(&self) -> String {
        self.inner.to_hex()
    }

    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        let Ok(o) = other.cast::<AuthorityFingerprint>() else {
            return Ok(py.NotImplemented());
        };
        let eq = o.get().inner == self.inner;
        match op {
            CompareOp::Eq => Ok(eq.into_pyobject(py)?.to_owned().into_any().unbind()),
            CompareOp::Ne => Ok((!eq).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }

    fn __hash__(&self) -> u64 {
        u64::from_le_bytes(self.inner.as_bytes()[..8].try_into().unwrap_or([0; 8]))
    }

    fn __repr__(&self) -> String {
        format!("AuthorityFingerprint('{}')", self.inner.to_hex())
    }
}

/// What this node actually holds over a namespace, read back from the store.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
pub struct AuthorityReport {
    inner: aster::AuthorityReport,
}

#[pymethods]
impl AuthorityReport {
    /// The namespace id (32 bytes).
    #[getter]
    fn namespace_id<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.namespace().to_bytes())
    }

    /// The namespace id as hex (same value as `DocHandle.doc_id()`).
    #[getter]
    fn namespace_hex(&self) -> String {
        self.inner.namespace().to_hex()
    }

    /// The access **stored** -- never the access requested.
    #[getter]
    fn access(&self) -> LocalAccess {
        self.inner.access().into()
    }

    #[getter]
    fn fingerprint(&self) -> AuthorityFingerprint {
        AuthorityFingerprint {
            inner: self.inner.fingerprint(),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "AuthorityReport(namespace='{}', access={:?})",
            self.inner.namespace().to_hex(),
            LocalAccess::from(self.inner.access())
        )
    }
}

/// The result of `retire_local_write_authority`: either `retired` (with the
/// access now stored) or refused because `handles` references are still open.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
pub struct RetireOutcome {
    inner: aster::RetireOutcome,
}

#[pymethods]
impl RetireOutcome {
    /// Whether write authority was retired.
    #[getter]
    fn retired(&self) -> bool {
        matches!(self.inner, aster::RetireOutcome::Retired(_))
    }

    /// The access now stored (when retired), else `None`.
    #[getter]
    fn access(&self) -> Option<LocalAccess> {
        match self.inner {
            aster::RetireOutcome::Retired(a) => Some(a.into()),
            aster::RetireOutcome::Busy { .. } => None,
        }
    }

    /// Open references that blocked the retire (when busy), else `None`.
    #[getter]
    fn handles(&self) -> Option<u64> {
        match self.inner {
            aster::RetireOutcome::Busy { handles } => Some(handles),
            aster::RetireOutcome::Retired(_) => None,
        }
    }

    fn __bool__(&self) -> bool {
        self.retired()
    }

    fn __repr__(&self) -> String {
        match self.inner {
            aster::RetireOutcome::Retired(a) => {
                format!("RetireOutcome.retired({:?})", LocalAccess::from(a))
            }
            aster::RetireOutcome::Busy { handles } => {
                format!("RetireOutcome.busy(handles={handles})")
            }
        }
    }
}

// ============================================================================
// DocsClient
// ============================================================================

#[pyclass]
pub struct DocsClient {
    pub(crate) inner: aster::Docs,
}

pub(crate) fn handle(doc: aster::Doc) -> DocHandle {
    DocHandle { inner: doc }
}

#[pymethods]
impl DocsClient {
    /// Create a new document, returns a DocHandle.
    fn create<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(handle(docs.create().await.map_err(aster_err_to_py)?))
        })
    }

    /// Create a new author, returns the author ID as hex string.
    fn create_author<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(docs
                .create_author()
                .await
                .map_err(aster_err_to_py)?
                .into_string())
        })
    }

    /// The node-wide default author (hex). Equals the node id.
    fn default_author<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(docs
                .default_author()
                .await
                .map_err(aster_err_to_py)?
                .into_string())
        })
    }

    /// Join a document from a ticket string, returns a DocHandle.
    fn join<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(handle(
                docs.join(ticket_str).await.map_err(aster_err_to_py)?,
            ))
        })
    }

    /// Reopen a namespace already in the local replica (32 bytes or hex).
    /// Returns None if it is not present.
    fn open<'py>(&self, py: Python<'py>, namespace: NamespaceArg) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(docs
                .open(namespace.0)
                .await
                .map_err(aster_err_to_py)?
                .map(handle))
        })
    }

    /// Alias of `open` (namespace id bytes).
    fn open_namespace<'py>(
        &self,
        py: Python<'py>,
        namespace_id: NamespaceArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.open(py, namespace_id)
    }

    /// Alias of `open` (namespace id hex).
    fn open_namespace_hex<'py>(
        &self,
        py: Python<'py>,
        namespace_id_hex: NamespaceArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.open(py, namespace_id_hex)
    }

    /// Open-or-import a read-only namespace by its public id (32 bytes or
    /// hex). Never downgrades an existing write capability.
    fn open_or_import_read_namespace<'py>(
        &self,
        py: Python<'py>,
        namespace_id: NamespaceArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(handle(
                docs.open_or_import_read_namespace(namespace_id.0)
                    .await
                    .map_err(aster_err_to_py)?,
            ))
        })
    }

    /// Open-or-import a writable namespace from its 32-byte secret.
    fn open_or_import_write_namespace<'py>(
        &self,
        py: Python<'py>,
        namespace_secret: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let raw: [u8; 32] = namespace_secret.as_slice().try_into().map_err(|_| {
            PyValueError::new_err(format!(
                "namespace secret must be 32 bytes, got {}",
                namespace_secret.len()
            ))
        })?;
        let docs = self.inner.clone();
        future_into_py(py, async move {
            Ok(handle(
                docs.open_or_import_write_namespace(aster::NamespaceSecret::from_bytes(raw))
                    .await
                    .map_err(aster_err_to_py)?,
            ))
        })
    }

    /// Report the authority this node holds over the namespace named by the
    /// argument (raises if it is not in the local store). Prefer
    /// `DocHandle.authority_report()` when gating on an open document.
    fn authority_report<'py>(
        &self,
        py: Python<'py>,
        namespace: NamespaceArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            let inner = docs
                .authority_report(namespace.0)
                .await
                .map_err(aster_err_to_py)?;
            Ok(AuthorityReport { inner })
        })
    }

    /// Retire this node's local write authority over a namespace, keeping
    /// every entry. Release every handle first (`shutdown_for_role_swap`);
    /// otherwise the outcome is busy. Local only: revokes nothing remotely.
    fn retire_local_write_authority<'py>(
        &self,
        py: Python<'py>,
        namespace: NamespaceArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            let inner = docs
                .retire_local_write_authority(namespace.0)
                .await
                .map_err(aster_err_to_py)?;
            Ok(RetireOutcome { inner })
        })
    }

    /// Join a document and subscribe to live events atomically.
    /// Returns a (DocHandle, DocEventReceiver) tuple.
    fn join_and_subscribe<'py>(
        &self,
        py: Python<'py>,
        ticket_str: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        future_into_py(py, async move {
            let (doc, events) = docs
                .join_and_subscribe(ticket_str)
                .await
                .map_err(aster_err_to_py)?;
            Ok((handle(doc), DocEventReceiver { inner: events }))
        })
    }

    /// Join a doc by namespace id (hex or bytes), syncing from `peer`, and
    /// subscribe to its events. Returns (DocHandle, DocEventReceiver).
    fn join_and_subscribe_namespace<'py>(
        &self,
        py: Python<'py>,
        namespace_id_hex: NamespaceArg,
        peer_node_id_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let docs = self.inner.clone();
        let peer = aster::NodeId::from_hex(peer_node_id_hex.0);
        future_into_py(py, async move {
            let (doc, events) = docs
                .join_and_subscribe_namespace(namespace_id_hex.0, &peer)
                .await
                .map_err(aster_err_to_py)?;
            Ok((handle(doc), DocEventReceiver { inner: events }))
        })
    }
}

// ============================================================================
// DocHandle
// ============================================================================

#[pyclass]
pub struct DocHandle {
    pub(crate) inner: aster::Doc,
}

impl DocHandle {
    fn delete_impl<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        prefix: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.del(&author(author_hex), prefix)
                .await
                .map_err(aster_err_to_py)
        })
    }
}

#[pymethods]
impl DocHandle {
    /// Get the document's namespace ID as hex string.
    fn doc_id(&self) -> PyResult<String> {
        Ok(self.inner.id().map_err(aster_err_to_py)?.to_hex())
    }

    /// Set a key to a byte value; returns the content hash (hex).
    fn set_bytes<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            Ok(doc
                .set_bytes(&author(author_hex), key, value)
                .await
                .map_err(aster_err_to_py)?
                .into_string())
        })
    }

    /// As `set_bytes`, but commits the value's blob metadata to disk first:
    /// the entry never outlives its value across a hard process kill. Costs
    /// a blob-store commit per call.
    fn set_bytes_durable<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            Ok(doc
                .set_bytes_durable(&author(author_hex), key, value)
                .await
                .map_err(aster_err_to_py)?
                .into_string())
        })
    }

    /// Delete all entries for `author_hex` whose key starts with `prefix`.
    /// Returns the number of removed entries.
    fn delete<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        prefix: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.delete_impl(py, author_hex, prefix)
    }

    /// Python-safe alias for the Rust/FFI `del` verb.
    #[pyo3(name = "del_")]
    fn del_alias<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        prefix: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.delete_impl(py, author_hex, prefix)
    }

    /// Get the value for an exact (author, key) pair. Returns bytes or None.
    fn get_exact<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        key: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let result = doc
                .get_exact(&author(author_hex), key)
                .await
                .map_err(aster_err_to_py)?;
            Ok(result.map(PyBytesResult))
        })
    }

    /// Whether an entry exists for an exact (author, key), *including* a
    /// deletion tombstone (which `get_exact` reports as absent). Fetches no
    /// content.
    fn entry_exists<'py>(
        &self,
        py: Python<'py>,
        author_hex: HexArg,
        key: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.entry_exists(&author(author_hex), key)
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// Query all entries for an exact key, across all authors. Entries are ordered by
    /// author id, not timestamp, so `limit` truncates by author and may drop the latest
    /// writer -- use it only to enumerate authors. To read a key's current value, use
    /// `query_latest_exact`. `limit` omitted (or None) reads every author's entry.
    #[pyo3(signature = (key, limit=None))]
    fn query_key_exact<'py>(
        &self,
        py: Python<'py>,
        key: Vec<u8>,
        limit: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entries = doc
                .query_key_exact(key, limit)
                .await
                .map_err(aster_err_to_py)?;
            Ok(entries.into_iter().map(DocEntry::from).collect::<Vec<_>>())
        })
    }

    /// Read the single latest entry for an exact key, across all authors. Returns the
    /// DocEntry with the highest timestamp, or None if the key was never written or its
    /// latest write was a deletion.
    fn query_latest_exact<'py>(
        &self,
        py: Python<'py>,
        key: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entry = doc.query_latest_exact(key).await.map_err(aster_err_to_py)?;
            Ok(entry.map(DocEntry::from))
        })
    }

    /// Query all entries matching a key prefix, across all authors.
    /// `limit` caps the result set; omit it (or pass None) for an unbounded listing.
    #[pyo3(signature = (prefix, limit=None))]
    fn query_key_prefix<'py>(
        &self,
        py: Python<'py>,
        prefix: Vec<u8>,
        limit: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entries = doc
                .query_key_prefix(prefix, limit)
                .await
                .map_err(aster_err_to_py)?;
            Ok(entries.into_iter().map(DocEntry::from).collect::<Vec<_>>())
        })
    }

    /// List the latest entry for each distinct key under a prefix, across all authors
    /// (keys whose latest write was a deletion are omitted). `limit` caps the number of
    /// distinct keys; omit it (or pass None) for an unbounded listing.
    #[pyo3(signature = (prefix, limit=None))]
    fn query_latest_prefix<'py>(
        &self,
        py: Python<'py>,
        prefix: Vec<u8>,
        limit: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entries = doc
                .query_latest_prefix(prefix, limit)
                .await
                .map_err(aster_err_to_py)?;
            Ok(entries.into_iter().map(DocEntry::from).collect::<Vec<_>>())
        })
    }

    /// Read the content bytes for an entry by its content hash.
    fn read_entry_content<'py>(
        &self,
        py: Python<'py>,
        content_hash_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let bytes = doc
                .read_entry_content(&hash(content_hash_hex))
                .await
                .map_err(aster_err_to_py)?;
            Ok(PyBytesResult(bytes))
        })
    }

    /// Read local values in input order, preserving duplicates: list[bytes | None].
    /// Missing/partial content is None, complete empty content is b"". No fetch
    /// or tags are created. Malformed hashes fail before any store read.
    /// This is not a snapshot: GC can race larger exports, and any storage,
    /// integrity or export error fails the entire call. Use custody for stable
    /// retention across reads and host verification.
    fn read_entry_contents_if_complete<'py>(
        &self,
        py: Python<'py>,
        content_hashes: Vec<HexArg>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        let hashes: Vec<aster::Hash> = content_hashes.into_iter().map(hash).collect();
        future_into_py(py, async move {
            let values = doc
                .read_entry_contents_if_complete(&hashes)
                .await
                .map_err(aster_err_to_py)?;
            Ok(values
                .into_iter()
                .map(|v| v.map(PyBytesResult))
                .collect::<Vec<_>>())
        })
    }

    /// Local storage status of an entry's value blob (`BlobStatusResult`).
    fn entry_content_status<'py>(
        &self,
        py: Python<'py>,
        content_hash: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let status = doc
                .entry_content_status(&hash(content_hash))
                .await
                .map_err(aster_err_to_py)?;
            Ok(BlobStatusResult::from(status))
        })
    }

    /// Report the authority this node holds over **this** document.
    fn authority_report<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let inner = doc.authority_report().await.map_err(aster_err_to_py)?;
            Ok(AuthorityReport { inner })
        })
    }

    /// Share this document, returning a ticket string. `mode` is a
    /// `ShareMode` or "read" / "write".
    fn share<'py>(&self, py: Python<'py>, mode: ShareModeArg) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.share(mode.0).await.map_err(aster_err_to_py)
        })
    }

    /// Share this document with full relay + address info. Returns a ticket string.
    fn share_with_addr<'py>(
        &self,
        py: Python<'py>,
        mode: ShareModeArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.share_with_addr(mode.0).await.map_err(aster_err_to_py)
        })
    }

    /// Subscribe to live document events. Returns a DocEventReceiver (an
    /// async iterator).
    fn subscribe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let events = doc.subscribe().await.map_err(aster_err_to_py)?;
            Ok(DocEventReceiver { inner: events })
        })
    }

    /// Subscribe now and return the events as an async iterator:
    /// `async for ev in doc.events(): ...`. Events written after this call
    /// returns are delivered.
    fn events(&self, py: Python<'_>) -> PyResult<DocEventReceiver> {
        let doc = self.inner.clone();
        let events = py.detach(|| {
            pyo3_async_runtimes::tokio::get_runtime().block_on(async move { doc.subscribe().await })
        });
        Ok(DocEventReceiver {
            inner: events.map_err(aster_err_to_py)?,
        })
    }

    /// Start syncing this document with the given peers (`NodeId` or hex).
    fn start_sync<'py>(&self, py: Python<'py>, peers: Vec<HexArg>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        let peers: Vec<aster::NodeId> = peers
            .into_iter()
            .map(|p| aster::NodeId::from_hex(p.0))
            .collect();
        future_into_py(py, async move {
            doc.start_sync(&peers).await.map_err(aster_err_to_py)
        })
    }

    /// Stop syncing this document.
    fn leave<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(
            py,
            async move { doc.leave().await.map_err(aster_err_to_py) },
        )
    }

    /// Release both of this document's references (live sync + handle) so
    /// its namespace can change role, within one `budget` in seconds. Every
    /// handle to this document (including clones) is unusable afterwards.
    fn shutdown_for_role_swap<'py>(
        &self,
        py: Python<'py>,
        budget: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !budget.is_finite() || budget < 0.0 {
            return Err(PyValueError::new_err(
                "budget must be a non-negative number of seconds",
            ));
        }
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.shutdown_for_role_swap(Duration::from_secs_f64(budget))
                .await
                .map_err(aster_err_to_py)?;
            Ok(None::<u8>)
        })
    }

    /// Set the download policy: a `DownloadPolicy`, or the legacy
    /// `(mode, prefixes)` strings form.
    #[pyo3(signature = (policy, prefixes=None))]
    fn set_download_policy<'py>(
        &self,
        py: Python<'py>,
        policy: &Bound<'py, PyAny>,
        prefixes: Option<Vec<Vec<u8>>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let policy = if let Ok(p) = policy.cast::<DownloadPolicy>() {
            if prefixes.is_some() {
                return Err(PyValueError::new_err(
                    "pass prefixes inside the DownloadPolicy, not separately",
                ));
            }
            p.get().to_sdk()?
        } else {
            let mode: String = policy.extract().map_err(|_| {
                PyValueError::new_err("policy must be a DownloadPolicy or a mode string")
            })?;
            policy_from_parts(&mode, prefixes.unwrap_or_default())?
        };
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.set_download_policy(policy)
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// Get the current download policy (`DownloadPolicy`).
    fn get_download_policy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let policy = doc.get_download_policy().await.map_err(aster_err_to_py)?;
            Ok(DownloadPolicy::from(policy))
        })
    }
}

// ============================================================================
// Factory function
// ============================================================================

/// Extract a DocsClient from a Node.
#[pyfunction]
pub fn docs_client(node: &Node) -> DocsClient {
    DocsClient {
        inner: node.inner.docs(),
    }
}

/// Register the docs types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<DocEntry>()?;
    m.add_class::<DocEvent>()?;
    m.add_class::<DocEventReceiver>()?;
    m.add_class::<DownloadPolicy>()?;
    m.add_class::<ShareMode>()?;
    m.add_class::<LocalAccess>()?;
    m.add_class::<AuthorityFingerprint>()?;
    m.add_class::<AuthorityReport>()?;
    m.add_class::<RetireOutcome>()?;
    m.add_class::<DocsClient>()?;
    m.add_class::<DocHandle>()?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    Ok(())
}
