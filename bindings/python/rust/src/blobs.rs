//! Blobs module - wraps CoreBlobsClient from aster_transport_core.
//!
//! Phase 2: Now wraps aster_transport_core::CoreBlobsClient instead of iroh_blobs types directly.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::{
    CoreBlobLocalInfo, CoreBlobObserveResult, CoreBlobStatus, CoreBlobsClient, CoreTagInfo,
};

use crate::error::err_to_py;
use crate::node::IrohNode;
use crate::PyBytesResult;

// ============================================================================
// BlobStatusResult
// ============================================================================

/// Result of BlobsClient.blob_status(). Holds status string and byte size.
#[pyclass]
pub struct BlobStatusResult {
    #[pyo3(get)]
    pub status: String, // "not_found", "partial", or "complete"
    #[pyo3(get)]
    pub size: u64,
}

// ============================================================================
// BlobObserveResult
// ============================================================================

/// Snapshot of a blob's local bitfield: is it complete, and what is its total size?
#[pyclass]
pub struct BlobObserveResult {
    #[pyo3(get)]
    pub is_complete: bool,
    /// Total blob size in bytes. 0 if not yet known (header not fetched).
    #[pyo3(get)]
    pub size: u64,
}

impl From<CoreBlobObserveResult> for BlobObserveResult {
    fn from(r: CoreBlobObserveResult) -> Self {
        Self {
            is_complete: r.is_complete,
            size: r.size,
        }
    }
}

// ============================================================================
// BlobLocalInfo
// ============================================================================

/// Local availability info for a blob: how many bytes we have and whether it is complete.
#[pyclass]
pub struct BlobLocalInfo {
    #[pyo3(get)]
    pub is_complete: bool,
    #[pyo3(get)]
    pub local_bytes: u64,
}

impl From<CoreBlobLocalInfo> for BlobLocalInfo {
    fn from(r: CoreBlobLocalInfo) -> Self {
        Self {
            is_complete: r.is_complete,
            local_bytes: r.local_bytes,
        }
    }
}

// ============================================================================
// TagInfo
// ============================================================================

/// Information about a named tag in the blob store.
#[pyclass]
pub struct TagInfo {
    #[pyo3(get)]
    pub name: String,
    #[pyo3(get)]
    pub hash: String,
    #[pyo3(get)]
    pub format: String,
}

impl From<CoreTagInfo> for TagInfo {
    fn from(t: CoreTagInfo) -> Self {
        Self {
            name: t.name,
            hash: t.hash,
            format: t.format,
        }
    }
}

// ============================================================================
// BlobsClient
// ============================================================================

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
    fn create_ticket(&self, hash_hex: String) -> PyResult<String> {
        self.inner.create_ticket(hash_hex).map_err(err_to_py)
    }

    /// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
    /// Returns the collection hash (hex). Sets tag "aster-python/{name}" for GC protection.
    fn add_bytes_as_collection<'py>(
        &self,
        py: Python<'py>,
        name: String,
        data: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client
                .add_bytes_as_collection(name, data)
                .await
                .map_err(err_to_py)
        })
    }

    /// Store a multi-file collection (HashSeq). Takes a list of (name, data) tuples.
    /// Returns the collection hash (hex). The collection is auto-tagged for GC protection.
    fn add_collection<'py>(
        &self,
        py: Python<'py>,
        entries: Vec<(String, Vec<u8>)>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.add_collection(entries).await.map_err(err_to_py)
        })
    }

    /// List entries from a stored collection by its hash.
    /// Returns a list of (name, hash_hex, size) tuples.
    fn list_collection<'py>(
        &self,
        py: Python<'py>,
        hash_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.list_collection(hash_hex).await.map_err(err_to_py)
        })
    }

    /// Create a ticket for a Collection (HashSeq format), compatible with sendme.
    fn create_collection_ticket(&self, hash_hex: String) -> PyResult<String> {
        self.inner
            .create_collection_ticket(hash_hex)
            .map_err(err_to_py)
    }

    /// Download a blob from a remote peer using a blob ticket string.
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

    // ── Tag methods ──────────────────────────────────────────────────────────

    /// Set a named tag. `format` must be "raw" or "hash_seq".
    fn tag_set<'py>(
        &self,
        py: Python<'py>,
        name: String,
        hash_hex: String,
        format: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client
                .tag_set(name, hash_hex, format)
                .await
                .map_err(err_to_py)
        })
    }

    /// Get a tag by name. Returns TagInfo or None.
    fn tag_get<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let result = client.tag_get(name).await.map_err(err_to_py)?;
            Ok(result.map(TagInfo::from))
        })
    }

    /// Delete a tag by name. Returns the number of tags removed (0 or 1).
    fn tag_delete<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.tag_delete(name).await.map_err(err_to_py)
        })
    }

    /// Delete all tags matching a prefix. Returns count removed.
    fn tag_delete_prefix<'py>(
        &self,
        py: Python<'py>,
        prefix: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.tag_delete_prefix(prefix).await.map_err(err_to_py)
        })
    }

    /// List all tags.
    fn tag_list<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let tags = client.tag_list().await.map_err(err_to_py)?;
            Ok(tags.into_iter().map(TagInfo::from).collect::<Vec<_>>())
        })
    }

    /// List tags matching a prefix.
    fn tag_list_prefix<'py>(&self, py: Python<'py>, prefix: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let tags = client.tag_list_prefix(prefix).await.map_err(err_to_py)?;
            Ok(tags.into_iter().map(TagInfo::from).collect::<Vec<_>>())
        })
    }

    /// List only HashSeq-format tags (collections).
    fn tag_list_hash_seq<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let tags = client.tag_list_hash_seq().await.map_err(err_to_py)?;
            Ok(tags.into_iter().map(TagInfo::from).collect::<Vec<_>>())
        })
    }

    /// Return the status of a blob as a dict: {"status": "not_found"|"partial"|"complete", "size": int}.
    fn blob_status<'py>(&self, py: Python<'py>, hash_hex: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let status = client.blob_status(hash_hex).await.map_err(err_to_py)?;
            let (status_str, size): (&str, u64) = match status {
                CoreBlobStatus::NotFound => ("not_found", 0),
                CoreBlobStatus::Partial { size } => ("partial", size),
                CoreBlobStatus::Complete { size } => ("complete", size),
            };
            Ok(BlobStatusResult {
                status: status_str.to_string(),
                size,
            })
        })
    }

    /// Return true if the blob is fully stored locally.
    fn blob_has<'py>(&self, py: Python<'py>, hash_hex: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.blob_has(hash_hex).await.map_err(err_to_py)
        })
    }

    /// Snapshot of the current bitfield for a blob.
    /// Returns a BlobObserveResult with `is_complete` and `size` (total bytes, 0 if unknown).
    fn blob_observe_snapshot<'py>(
        &self,
        py: Python<'py>,
        hash_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let r = client
                .blob_observe_snapshot(hash_hex)
                .await
                .map_err(err_to_py)?;
            Ok(BlobObserveResult::from(r))
        })
    }

    /// Wait until the blob is fully downloaded locally.
    /// Resolves immediately if the blob is already complete; errors if the stream ends without
    /// completion (e.g. no active download).
    fn blob_observe_complete<'py>(
        &self,
        py: Python<'py>,
        hash_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client
                .blob_observe_complete(hash_hex)
                .await
                .map_err(err_to_py)
        })
    }

    /// Check local availability of a blob via the Remote API.
    /// Returns a BlobLocalInfo with `is_complete` and `local_bytes`.
    fn blob_local_info<'py>(
        &self,
        py: Python<'py>,
        hash_hex: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let r = client.blob_local_info(hash_hex).await.map_err(err_to_py)?;
            Ok(BlobLocalInfo::from(r))
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
    m.add_class::<BlobStatusResult>()?;
    m.add_class::<BlobObserveResult>()?;
    m.add_class::<BlobLocalInfo>()?;
    m.add_class::<TagInfo>()?;
    m.add_class::<BlobsClient>()?;
    m.add_function(wrap_pyfunction!(blobs_client, m)?)?;
    Ok(())
}
