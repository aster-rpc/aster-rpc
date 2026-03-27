use pyo3::prelude::*;
use pyo3::types::PyBytes;

use bytes::Bytes;
use iroh_blobs::api::Store as BlobStore;
use iroh_docs::protocol::Docs;
use iroh_docs::api::Doc;
use iroh_docs::api::protocol::{ShareMode, AddrInfoOptions};
use iroh_docs::AuthorId;
use iroh_tickets::Ticket;
use iroh::address_lookup::memory::MemoryLookup;

use crate::error::err_to_py;

#[pyclass]
pub struct DocsClient {
    pub(crate) inner: Docs,
    pub(crate) store: BlobStore,
    pub(crate) endpoint: iroh::endpoint::Endpoint,
}

#[pyclass]
pub struct DocHandle {
    pub(crate) doc: Doc,
    pub(crate) store: BlobStore,
}

#[pymethods]
impl DocsClient {
    /// Create a new document, returns a DocHandle.
    fn create<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let docs = self.inner.clone();
        let store = self.store.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let doc = docs.api().create().await.map_err(err_to_py)?;
            Ok(DocHandle { doc, store })
        })
    }

    /// Create a new author, returns the author ID as hex string.
    fn create_author<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let docs = self.inner.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let author_id = docs.api().author_create().await.map_err(err_to_py)?;
            Ok(author_id.to_string())
        })
    }

    /// Join a document from a ticket string, returns a DocHandle.
    fn join<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<&'py PyAny> {
        let docs = self.inner.clone();
        let store = self.store.clone();
        let endpoint = self.endpoint.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let ticket = iroh_docs::DocTicket::deserialize(&ticket_str).map_err(err_to_py)?;

            // Add node addresses from the ticket for peer discovery
            if let Ok(lookup) = endpoint.address_lookup() {
                for node_addr in &ticket.nodes {
                    let mem = MemoryLookup::new();
                    mem.add_endpoint_info(node_addr.clone());
                    lookup.add(mem);
                }
            }

            let doc = docs.api().import_namespace(ticket.capability).await.map_err(err_to_py)?;
            Ok(DocHandle { doc, store })
        })
    }
}

#[pymethods]
impl DocHandle {
    /// Get the document's namespace ID as hex string.
    fn doc_id(&self) -> String {
        self.doc.id().to_string()
    }

    /// Set a key to a byte value.
    fn set_bytes<'py>(
        &self,
        py: Python<'py>,
        author_hex: String,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> PyResult<&'py PyAny> {
        let doc = self.doc.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let author_id: AuthorId = author_hex.parse().map_err(err_to_py)?;
            let hash = doc
                .set_bytes(author_id, Bytes::from(key), Bytes::from(value))
                .await
                .map_err(err_to_py)?;
            Ok(hash.to_hex().to_string())
        })
    }

    /// Get the value for an exact (author, key) pair. Returns bytes or None.
    fn get_exact<'py>(
        &self,
        py: Python<'py>,
        author_hex: String,
        key: Vec<u8>,
    ) -> PyResult<&'py PyAny> {
        let doc = self.doc.clone();
        let store = self.store.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let author_id: AuthorId = author_hex.parse().map_err(err_to_py)?;
            let entry = doc
                .get_exact(author_id, key, false)
                .await
                .map_err(err_to_py)?;
            match entry {
                Some(entry) => {
                    let hash = entry.content_hash();
                    let data = store.get_bytes(hash).await.map_err(err_to_py)?;
                    Python::with_gil(|py| Ok(PyBytes::new(py, &data).into()))
                }
                None => Ok(Python::with_gil(|py| py.None())),
            }
        })
    }

    /// Share this document, returning a ticket string.
    /// mode: "read" or "write"
    fn share<'py>(&self, py: Python<'py>, mode: String, endpoint: &crate::node::IrohNode) -> PyResult<&'py PyAny> {
        let doc = self.doc.clone();
        let ep = endpoint.endpoint.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let share_mode = match mode.as_str() {
                "read" | "Read" => ShareMode::Read,
                "write" | "Write" => ShareMode::Write,
                _ => return Err(crate::error::IrohError::new_err("mode must be 'read' or 'write'")),
            };
            let ticket = doc.share(share_mode, AddrInfoOptions::Id).await.map_err(err_to_py)?;
            Ok(ticket.serialize())
        })
    }
}

#[pyfunction]
pub fn docs_client(node: &crate::node::IrohNode) -> DocsClient {
    DocsClient {
        inner: node.docs.clone(),
        store: node.store.clone(),
        endpoint: node.endpoint.clone(),
    }
}