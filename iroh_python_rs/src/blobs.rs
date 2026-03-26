use pyo3::prelude::*;
use pyo3::types::PyBytes;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::BlobFormat;
use iroh_blobs::api::downloader::Downloader;
use iroh::endpoint::Endpoint;
use iroh::address_lookup::memory::MemoryLookup;
use iroh_tickets::Ticket;
use crate::error::err_to_py;
use crate::node::IrohNode;

/// Python wrapper for the Iroh Blobs client.
#[pyclass]
pub struct BlobsClient {
    pub(crate) store: MemStore,
    pub(crate) endpoint: Endpoint,
}

#[pymethods]
impl BlobsClient {
    /// Store bytes and return the BLAKE3 hash as a hex string.
    fn add_bytes<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let store = self.store.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let tag_info = store
                .add_slice(&data)
                .await
                .map_err(err_to_py)?;
            Ok(tag_info.hash.to_string())
        })
    }

    /// Read a blob by its BLAKE3 hash hex string. Returns bytes or raises IrohError.
    fn read_to_bytes<'py>(&self, py: Python<'py>, hash_hex: String) -> PyResult<&'py PyAny> {
        let store = self.store.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let hash: iroh_blobs::Hash = hash_hex.parse().map_err(err_to_py)?;
            let data: bytes::Bytes = store
                .get_bytes(hash)
                .await
                .map_err(err_to_py)?;
            let result: PyObject = Python::with_gil(|py| PyBytes::new(py, &data).into_py(py));
            Ok(result)
        })
    }

    /// Create a blob ticket string for sharing a blob with a remote peer.
    /// The ticket contains the blob hash, this node's address, and format info.
    fn create_ticket(&self, hash_hex: String) -> PyResult<String> {
        let hash: iroh_blobs::Hash = hash_hex.parse().map_err(err_to_py)?;
        let addr = self.endpoint.addr();
        let ticket = BlobTicket::new(addr, hash, BlobFormat::Raw);
        Ok(ticket.serialize())
    }

    /// Download a blob from a remote peer using a blob ticket string.
    /// Returns the blob content as bytes.
    fn download_blob<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<&'py PyAny> {
        let store = self.store.clone();
        let endpoint = self.endpoint.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let ticket = BlobTicket::deserialize(&ticket_str).map_err(err_to_py)?;
            let hash = ticket.hash();
            let (addr, _, _) = ticket.into_parts();

            // Add peer address for discovery
            if let Ok(lookup) = endpoint.address_lookup() {
                let mem = MemoryLookup::new();
                mem.add_endpoint_info(addr.clone());
                lookup.add(mem);
            }

            // Download from remote peer
            let downloader = Downloader::new(&store, &endpoint);
            let node_id = addr.id;
            downloader
                .download(hash, vec![node_id])
                .await
                .map_err(err_to_py)?;

            // Read downloaded blob from local store
            let data: bytes::Bytes = store
                .get_bytes(hash)
                .await
                .map_err(err_to_py)?;
            let result: PyObject = Python::with_gil(|py| PyBytes::new(py, &data).into_py(py));
            Ok(result)
        })
    }
}

/// Create a BlobsClient from an IrohNode.
#[pyfunction]
pub fn blobs_client(node: &IrohNode) -> BlobsClient {
    BlobsClient {
        store: node.store.clone(),
        endpoint: node.endpoint.clone(),
    }
}