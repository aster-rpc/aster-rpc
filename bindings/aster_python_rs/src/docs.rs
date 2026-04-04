//! Docs module - wraps CoreDocsClient, CoreDoc from aster_transport_core.
//!
//! Phase 2: Now wraps aster_transport_core types instead of iroh_docs types directly.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::{CoreDoc, CoreDocEntry, CoreDocsClient};

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
    m.add_class::<DocsClient>()?;
    m.add_class::<DocHandle>()?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    Ok(())
}
