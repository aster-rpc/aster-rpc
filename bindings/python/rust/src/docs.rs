//! Docs module - wraps CoreDocsClient, CoreDoc from aster_transport_core.
//!
//! Phase 2: Now wraps aster_transport_core types instead of iroh_docs types directly.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::{
    CoreDoc, CoreDocEntry, CoreDocEvent, CoreDocEventReceiver, CoreDocsClient, CoreDownloadPolicy,
};

use crate::error::err_to_py;
use crate::node::IrohNode;
use crate::PyBytesResult;

// ============================================================================
// DocEntry
// ============================================================================

/// A document entry returned from queries, with metadata about who wrote it.
#[pyclass]
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

impl From<CoreDocEntry> for DocEntry {
    fn from(e: CoreDocEntry) -> Self {
        Self {
            author_id: e.author_id,
            key: e.key,
            content_hash: e.content_hash,
            content_len: e.content_len,
            timestamp: e.timestamp,
        }
    }
}

// ============================================================================
// DocEvent
// ============================================================================

/// A live document event. Check the `kind` field to determine which fields are populated.
///
/// kind values:
///   "insert_local"     — entry written by this node  (entry populated)
///   "insert_remote"    — entry received from peer    (entry + from populated)
///   "content_ready"    — blob now available locally  (hash populated)
///   "pending_content_ready" — all queued downloads done
///   "neighbor_up"      — peer joined the swarm       (peer populated)
///   "neighbor_down"    — peer left the swarm         (peer populated)
///   "sync_finished"    — sync with peer complete     (peer populated)
#[pyclass]
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
}

// ============================================================================
// DocDownloadPolicy
// ============================================================================

/// Download policy for a document.
///
/// mode values:
///   "everything"        — download all entries (default)
///   "nothing_except"    — download only entries whose keys start with one of the prefixes
///   "everything_except" — download all entries except those whose keys start with one of the prefixes
#[pyclass]
pub struct DocDownloadPolicy {
    #[pyo3(get)]
    pub mode: String,
    #[pyo3(get)]
    pub prefixes: Vec<Vec<u8>>,
}

impl From<CoreDownloadPolicy> for DocDownloadPolicy {
    fn from(p: CoreDownloadPolicy) -> Self {
        match p {
            CoreDownloadPolicy::Everything => Self {
                mode: "everything".to_string(),
                prefixes: vec![],
            },
            CoreDownloadPolicy::NothingExcept { prefixes } => Self {
                mode: "nothing_except".to_string(),
                prefixes,
            },
            CoreDownloadPolicy::EverythingExcept { prefixes } => Self {
                mode: "everything_except".to_string(),
                prefixes,
            },
        }
    }
}

fn py_policy_to_core(mode: &str, prefixes: Vec<Vec<u8>>) -> pyo3::PyResult<CoreDownloadPolicy> {
    match mode {
        "everything" => Ok(CoreDownloadPolicy::Everything),
        "nothing_except" => Ok(CoreDownloadPolicy::NothingExcept { prefixes }),
        "everything_except" => Ok(CoreDownloadPolicy::EverythingExcept { prefixes }),
        _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "mode must be 'everything', 'nothing_except', or 'everything_except', got '{mode}'"
        ))),
    }
}

fn core_event_to_py(py: Python<'_>, ev: CoreDocEvent) -> PyResult<DocEvent> {
    match ev {
        CoreDocEvent::InsertLocal { entry } => Ok(DocEvent {
            kind: "insert_local".to_string(),
            entry: Some(Py::new(py, DocEntry::from(entry))?),
            from_peer: None,
            hash: None,
            peer: None,
        }),
        CoreDocEvent::InsertRemote { from, entry } => Ok(DocEvent {
            kind: "insert_remote".to_string(),
            entry: Some(Py::new(py, DocEntry::from(entry))?),
            from_peer: Some(from),
            hash: None,
            peer: None,
        }),
        CoreDocEvent::ContentReady { hash } => Ok(DocEvent {
            kind: "content_ready".to_string(),
            entry: None,
            from_peer: None,
            hash: Some(hash),
            peer: None,
        }),
        CoreDocEvent::PendingContentReady => Ok(DocEvent {
            kind: "pending_content_ready".to_string(),
            entry: None,
            from_peer: None,
            hash: None,
            peer: None,
        }),
        CoreDocEvent::NeighborUp { peer } => Ok(DocEvent {
            kind: "neighbor_up".to_string(),
            entry: None,
            from_peer: None,
            hash: None,
            peer: Some(peer),
        }),
        CoreDocEvent::NeighborDown { peer } => Ok(DocEvent {
            kind: "neighbor_down".to_string(),
            entry: None,
            from_peer: None,
            hash: None,
            peer: Some(peer),
        }),
        CoreDocEvent::SyncFinished { peer } => Ok(DocEvent {
            kind: "sync_finished".to_string(),
            entry: None,
            from_peer: None,
            hash: None,
            peer: Some(peer),
        }),
    }
}

// ============================================================================
// DocEventReceiver
// ============================================================================

/// Receiver for live document events. Obtained from DocHandle.subscribe().
#[pyclass]
pub struct DocEventReceiver {
    inner: CoreDocEventReceiver,
}

impl From<CoreDocEventReceiver> for DocEventReceiver {
    fn from(inner: CoreDocEventReceiver) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl DocEventReceiver {
    /// Receive the next live document event. Returns None when the subscription ends.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let receiver = self.inner.clone();
        future_into_py(py, async move {
            match receiver.recv().await.map_err(err_to_py)? {
                None => Ok(None::<DocEvent>),
                Some(ev) => {
                    // We need the GIL to create Py<DocEntry> — acquire it here.
                    let event = pyo3::Python::attach(|py| core_event_to_py(py, ev))?;
                    Ok(Some(event))
                }
            }
        })
    }
}

// ============================================================================
// DocsClient
// ============================================================================

#[pyclass]
pub struct DocsClient {
    inner: CoreDocsClient,
}

impl From<CoreDocsClient> for DocsClient {
    fn from(inner: CoreDocsClient) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl DocsClient {
    /// Create a new document, returns a DocHandle.
    fn create<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let doc = client.create().await.map_err(err_to_py)?;
            Ok(DocHandle::from(doc))
        })
    }

    /// Create a new author, returns the author ID as hex string.
    fn create_author<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.create_author().await.map_err(err_to_py)
        })
    }

    /// Join a document from a ticket string, returns a DocHandle.
    fn join<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let doc = client.join(ticket_str).await.map_err(err_to_py)?;
            Ok(DocHandle::from(doc))
        })
    }

    /// Join a document and subscribe to live events atomically.
    /// Returns a (DocHandle, DocEventReceiver) tuple.
    fn join_and_subscribe<'py>(
        &self,
        py: Python<'py>,
        ticket_str: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let (doc, receiver) = client
                .join_and_subscribe(ticket_str)
                .await
                .map_err(err_to_py)?;
            Ok((DocHandle::from(doc), DocEventReceiver::from(receiver)))
        })
    }

    /// Join a doc by namespace ID (hex) and subscribe to events.
    ///
    /// Use this instead of ``join_and_subscribe`` when you already know the
    /// peer address (e.g., after consumer admission) and only have the
    /// namespace ID, not a full ``DocTicket`` string.
    fn join_and_subscribe_namespace<'py>(
        &self,
        py: Python<'py>,
        namespace_id_hex: String,
        peer_node_id_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let (doc, receiver) = client
                .join_and_subscribe_namespace(namespace_id_hex, peer_node_id_hex)
                .await
                .map_err(err_to_py)?;
            Ok((DocHandle::from(doc), DocEventReceiver::from(receiver)))
        })
    }
}

// ============================================================================
// DocHandle
// ============================================================================

#[pyclass]
pub struct DocHandle {
    inner: CoreDoc,
}

impl From<CoreDoc> for DocHandle {
    fn from(inner: CoreDoc) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl DocHandle {
    /// Get the document's namespace ID as hex string.
    fn doc_id(&self) -> String {
        self.inner.doc_id()
    }

    /// Set a key to a byte value.
    fn set_bytes<'py>(
        &self,
        py: Python<'py>,
        author_hex: String,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.set_bytes(author_hex, key, value)
                .await
                .map_err(err_to_py)
        })
    }

    /// Get the value for an exact (author, key) pair. Returns bytes or None.
    fn get_exact<'py>(
        &self,
        py: Python<'py>,
        author_hex: String,
        key: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let result = doc.get_exact(author_hex, key).await.map_err(err_to_py)?;
            Ok(result.map(PyBytesResult))
        })
    }

    /// Query all entries for an exact key, across all authors.
    /// Returns a list of DocEntry with metadata.
    fn query_key_exact<'py>(&self, py: Python<'py>, key: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entries = doc.query_key_exact(key).await.map_err(err_to_py)?;
            Ok(entries.into_iter().map(DocEntry::from).collect::<Vec<_>>())
        })
    }

    /// Query all entries matching a key prefix, across all authors.
    /// Returns a list of DocEntry with metadata.
    fn query_key_prefix<'py>(
        &self,
        py: Python<'py>,
        prefix: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let entries = doc.query_key_prefix(prefix).await.map_err(err_to_py)?;
            Ok(entries.into_iter().map(DocEntry::from).collect::<Vec<_>>())
        })
    }

    /// Read the content bytes for an entry by its content hash hex string.
    fn read_entry_content<'py>(
        &self,
        py: Python<'py>,
        content_hash_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let bytes = doc
                .read_entry_content(content_hash_hex)
                .await
                .map_err(err_to_py)?;
            Ok(PyBytesResult(bytes))
        })
    }

    /// Share this document, returning a ticket string.
    /// mode: "read" or "write"
    fn share<'py>(&self, py: Python<'py>, mode: String) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move { doc.share(mode).await.map_err(err_to_py) })
    }

    /// Subscribe to live document events. Returns a DocEventReceiver.
    fn subscribe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let receiver = doc.subscribe().await.map_err(err_to_py)?;
            Ok(DocEventReceiver::from(receiver))
        })
    }

    /// Start syncing this document with the given peers (endpoint ID hex strings).
    fn start_sync<'py>(&self, py: Python<'py>, peers: Vec<String>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(
            py,
            async move { doc.start_sync(peers).await.map_err(err_to_py) },
        )
    }

    /// Stop syncing this document.
    fn leave<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move { doc.leave().await.map_err(err_to_py) })
    }

    /// Set the download policy for this document.
    /// mode: "everything" | "nothing_except" | "everything_except"
    /// prefixes: list of byte prefixes that the policy applies to (ignored for "everything")
    fn set_download_policy<'py>(
        &self,
        py: Python<'py>,
        mode: String,
        prefixes: Vec<Vec<u8>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        let policy = py_policy_to_core(&mode, prefixes)?;
        future_into_py(py, async move {
            doc.set_download_policy(policy).await.map_err(err_to_py)
        })
    }

    /// Get the current download policy for this document. Returns a DocDownloadPolicy.
    fn get_download_policy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let policy = doc.get_download_policy().await.map_err(err_to_py)?;
            Ok(DocDownloadPolicy::from(policy))
        })
    }

    /// Share this document with full relay+address info. Returns a ticket string.
    /// mode: "read" or "write"
    fn share_with_addr<'py>(&self, py: Python<'py>, mode: String) -> PyResult<Bound<'py, PyAny>> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.share_with_addr(mode).await.map_err(err_to_py)
        })
    }
}

// ============================================================================
// Factory function
// ============================================================================

/// Extract a DocsClient from an IrohNode.
#[pyfunction]
pub fn docs_client(node: &IrohNode) -> DocsClient {
    DocsClient::from(node.inner().docs_client())
}

/// Register the docs types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<DocEntry>()?;
    m.add_class::<DocEvent>()?;
    m.add_class::<DocEventReceiver>()?;
    m.add_class::<DocDownloadPolicy>()?;
    m.add_class::<DocsClient>()?;
    m.add_class::<DocHandle>()?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    Ok(())
}
