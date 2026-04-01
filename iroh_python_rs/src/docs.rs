//! Docs module - wraps CoreDocsClient, CoreDoc from iroh_transport_core.
//!
//! Phase 2: Now wraps iroh_transport_core types instead of iroh_docs types directly.

use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_asyncio::tokio::future_into_py;

use iroh_transport_core::{CoreDocsClient, CoreDoc};

use crate::error::err_to_py;
use crate::node::IrohNode;

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
    fn create<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let doc = client.create().await.map_err(err_to_py)?;
            Ok(DocHandle::from(doc))
        })
    }

    /// Create a new author, returns the author ID as hex string.
    fn create_author<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client
                .create_author()
                .await
                .map_err(err_to_py)
        })
    }

    /// Join a document from a ticket string, returns a DocHandle.
    fn join<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<&'py PyAny> {
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
    ) -> PyResult<&'py PyAny> {
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
    ) -> PyResult<&'py PyAny> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            let result = doc.get_exact(author_hex, key).await.map_err(err_to_py)?;
            Ok(Python::with_gil(|py| match result {
                Some(data) => PyBytes::new(py, &data).into_py(py),
                None => py.None(),
            }))
        })
    }

    /// Share this document, returning a ticket string.
    /// mode: "read" or "write"
    fn share<'py>(&self, py: Python<'py>, mode: String) -> PyResult<&'py PyAny> {
        let doc = self.inner.clone();
        future_into_py(py, async move {
            doc.share(mode).await.map_err(err_to_py)
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
pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<DocsClient>()?;
    m.add_class::<DocHandle>()?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    Ok(())
}
