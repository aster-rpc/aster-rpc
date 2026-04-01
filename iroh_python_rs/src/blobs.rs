//! Blobs module - wraps CoreBlobsClient from iroh_transport_core.
//!
//! Phase 2: Now wraps iroh_transport_core::CoreBlobsClient instead of iroh_blobs types directly.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use iroh_transport_core::CoreBlobsClient;

use crate::error::err_to_py;
use crate::node::IrohNode;
use crate::PyBytesResult;

/// Python wrapper for the Iroh Blobs client.
#[pyclass]
pub struct BlobsClient {
    inner: CoreBlobsClient,
}

impl From<CoreBlobsClient> for BlobsClient {
    fn from(inner: CoreBlobsClient) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl BlobsClient {
    /// Store bytes and return the BLAKE3 hash as a hex string.
    fn add_bytes<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.add_bytes(data).await.map_err(err_to_py)
        })
    }

    /// Read a blob by its BLAKE3 hash hex string. Returns bytes or raises IrohError.
    fn read_to_bytes<'py>(&self, py: Python<'py>, hash_hex: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let data = client.read_to_bytes(hash_hex).await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// Create a blob ticket string for sharing a blob with a remote peer.
    /// The ticket contains the blob hash, this node's address, and format info.
    fn create_ticket(&self, hash_hex: String) -> PyResult<String> {
        self.inner.create_ticket(hash_hex).map_err(err_to_py)
    }

    /// Download a blob from a remote peer using a blob ticket string.
    /// Returns the blob content as bytes.
    fn download_blob<'py>(
        &self,
        py: Python<'py>,
        ticket_str: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let data = client.download_blob(ticket_str).await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }
}

/// Create a BlobsClient from an IrohNode.
#[pyfunction]
pub fn blobs_client(node: &IrohNode) -> BlobsClient {
    BlobsClient::from(node.inner().blobs_client())
}

/// Register the blobs types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<BlobsClient>()?;
    m.add_function(wrap_pyfunction!(blobs_client, m)?)?;
    Ok(())
}
