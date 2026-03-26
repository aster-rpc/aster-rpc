use pyo3::prelude::*;
use pyo3::types::PyBytes;
use iroh_blobs::store::mem::MemStore;
use crate::error::err_to_py;
use crate::node::IrohNode;

/// Python wrapper for the Iroh Blobs client.
#[pyclass]
pub struct BlobsClient {
    pub(crate) store: MemStore,
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
}

/// Create a BlobsClient from an IrohNode.
#[pyfunction]
pub fn blobs_client(node: &IrohNode) -> BlobsClient {
    BlobsClient {
        store: node.store.clone(),
    }
}