//! Blobs: `BlobsClient` over the `aster` SDK's `Blobs` (downloads, ranged
//! reads, multi-provider fetch with `FetchReport`, tags, GC) plus the
//! transport-core client for tickets, collections and observe calls. Every
//! hash / node-id argument takes a typed `Hash` / `NodeId` or a hex string.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::{
    CoreBlobLocalInfo, CoreBlobObserveResult, CoreBlobStatus, CoreBlobsClient, CoreTagInfo,
};

use crate::error::{aster_err_to_py, err_to_py};
use crate::ids::{HexArg, NodeId};
use crate::node::Node;
use crate::PyBytesResult;

// ============================================================================
// BlobStatusResult
// ============================================================================

/// A blob's local storage status (`BlobsClient.blob_status`,
/// `DocHandle.entry_content_status`): `status` is `"not_found"`,
/// `"partial"` or `"complete"`, `size` the bytes known/available.
#[pyclass(frozen)]
pub struct BlobStatusResult {
    #[pyo3(get)]
    pub status: String,
    #[pyo3(get)]
    pub size: u64,
}

impl From<aster::BlobStatus> for BlobStatusResult {
    fn from(s: aster::BlobStatus) -> Self {
        let (status, size) = match s {
            aster::BlobStatus::NotFound => ("not_found", 0),
            aster::BlobStatus::Partial { size } => ("partial", size),
            aster::BlobStatus::Complete { size } => ("complete", size),
        };
        Self {
            status: status.to_string(),
            size,
        }
    }
}

#[pymethods]
impl BlobStatusResult {
    /// Whether the blob is fully present locally.
    #[getter]
    fn is_complete(&self) -> bool {
        self.status == "complete"
    }

    fn __repr__(&self) -> String {
        format!(
            "BlobStatusResult(status='{}', size={})",
            self.status, self.size
        )
    }
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
// BlobFormat / FetchStrategy / FetchReport
// ============================================================================

/// On-wire blob format.
#[pyclass(eq, eq_int, frozen, from_py_object, module = "aster._aster")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlobFormat {
    /// A single raw blob.
    #[pyo3(name = "RAW")]
    Raw,
    /// A HashSeq (collection).
    #[pyo3(name = "HASH_SEQ")]
    HashSeq,
}

/// A format argument: `BlobFormat` or `"raw"` / `"hash_seq"`.
pub(crate) struct BlobFormatArg(pub aster::BlobFormat);

impl<'a, 'py> FromPyObject<'a, 'py> for BlobFormatArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(f) = obj.cast::<BlobFormat>() {
            return Ok(Self(match *f.get() {
                BlobFormat::Raw => aster::BlobFormat::Raw,
                BlobFormat::HashSeq => aster::BlobFormat::HashSeq,
            }));
        }
        let s: String = obj.extract().map_err(|_| {
            PyValueError::new_err("format must be a BlobFormat or 'raw' / 'hash_seq'")
        })?;
        match s.as_str() {
            "raw" => Ok(Self(aster::BlobFormat::Raw)),
            "hash_seq" | "hashseq" => Ok(Self(aster::BlobFormat::HashSeq)),
            other => Err(PyValueError::new_err(format!(
                "format must be 'raw' or 'hash_seq', got {other:?}"
            ))),
        }
    }
}

/// How a multi-provider fetch decomposes a HashSeq. Providers are walked
/// in the given order, with failover, in both.
#[pyclass(eq, eq_int, frozen, from_py_object, module = "aster._aster")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchStrategy {
    /// One request for the whole blob / HashSeq.
    #[pyo3(name = "WHOLE")]
    Whole,
    /// The root, then each child as its own request (up to 32 in flight).
    /// Order is honoured per child. Needs `BlobFormat.HASH_SEQ`.
    #[pyo3(name = "SPLIT_CHILDREN")]
    SplitChildren,
}

impl From<FetchStrategy> for aster::FetchStrategy {
    fn from(s: FetchStrategy) -> Self {
        match s {
            FetchStrategy::Whole => aster::FetchStrategy::Whole,
            FetchStrategy::SplitChildren => aster::FetchStrategy::SplitChildren,
        }
    }
}

/// What one provider did during a fetch.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct ProviderOutcome {
    /// The provider.
    #[pyo3(get)]
    node_id: NodeId,
    /// Requests attempted against it (a dial or transfer).
    #[pyo3(get)]
    attempts: u32,
    /// Attempts that failed to connect or transfer.
    #[pyo3(get)]
    failures: u32,
    /// Requests it carried to completion.
    #[pyo3(get)]
    completed_requests: u32,
}

#[pymethods]
impl ProviderOutcome {
    fn __repr__(&self) -> String {
        format!(
            "ProviderOutcome(node_id='{}', attempts={}, failures={}, completed_requests={})",
            self.node_id.hex, self.attempts, self.failures, self.completed_requests
        )
    }
}

/// What a multi-provider fetch did, for callers ranking providers.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
pub struct FetchReport {
    providers: Vec<ProviderOutcome>,
    /// Payload bytes pulled off the wire (excludes data already local).
    #[pyo3(get)]
    bytes_transferred: u64,
}

impl From<aster::FetchReport> for FetchReport {
    fn from(r: aster::FetchReport) -> Self {
        Self {
            providers: r
                .providers()
                .iter()
                .map(|p| ProviderOutcome {
                    node_id: NodeId::from_trusted_hex(p.node_id().as_str().to_string()),
                    attempts: p.attempts(),
                    failures: p.failures(),
                    completed_requests: p.completed_requests(),
                })
                .collect(),
            bytes_transferred: r.bytes_transferred(),
        }
    }
}

#[pymethods]
impl FetchReport {
    /// Per-provider outcomes, in first-attempt order.
    #[getter]
    fn providers(&self) -> Vec<ProviderOutcome> {
        self.providers.clone()
    }

    /// Providers actually dialled, in first-attempt order.
    fn attempted(&self) -> Vec<NodeId> {
        self.providers.iter().map(|p| p.node_id.clone()).collect()
    }

    /// Providers where at least one request failed.
    fn failed(&self) -> Vec<NodeId> {
        self.providers
            .iter()
            .filter(|p| p.failures > 0)
            .map(|p| p.node_id.clone())
            .collect()
    }

    /// Providers that completed at least one request.
    fn served(&self) -> Vec<NodeId> {
        self.providers
            .iter()
            .filter(|p| p.completed_requests > 0)
            .map(|p| p.node_id.clone())
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "FetchReport(providers={}, bytes_transferred={})",
            self.providers.len(),
            self.bytes_transferred
        )
    }
}

fn node_ids(ids: Vec<HexArg>) -> Vec<aster::NodeId> {
    ids.into_iter()
        .map(|a| aster::NodeId::from_hex(a.0))
        .collect()
}

// ============================================================================
// BlobsClient
// ============================================================================

/// Python wrapper for the Iroh Blobs client.
#[pyclass]
pub struct BlobsClient {
    /// Transport-core client: tickets, collections, observe/local-info.
    inner: CoreBlobsClient,
    /// The `aster` SDK facade: downloads, ranged reads, tags, GC.
    sdk: aster::Blobs,
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

    /// Import a file by path (Copy mode, never TryReference) and return the
    /// BLAKE3 hash as a hex string. Like `add_bytes`, an auto-tag keeps the blob
    /// alive; use `add_path_with_named_tag` for a deterministic GC-keyed tag.
    fn add_path<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(
            py,
            async move { client.add_path(path).await.map_err(err_to_py) },
        )
    }

    /// Import a file by path and set a persistent named tag in one step,
    /// returning the BLAKE3 hash as a hex string.
    fn add_path_with_named_tag<'py>(
        &self,
        py: Python<'py>,
        path: String,
        tag_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client
                .add_path_with_named_tag(path, tag_name)
                .await
                .map_err(err_to_py)
        })
    }

    /// Read a blob by its BLAKE3 hash hex string. Returns bytes or raises IrohError.
    fn read_to_bytes<'py>(&self, py: Python<'py>, hash_hex: HexArg) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
        let client = self.inner.clone();
        future_into_py(py, async move {
            let data = client.read_to_bytes(hash_hex).await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// Create a blob ticket string for sharing a blob with a remote peer.
    fn create_ticket(&self, hash_hex: HexArg) -> PyResult<String> {
        let hash_hex = hash_hex.0;
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
        hash_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.list_collection(hash_hex).await.map_err(err_to_py)
        })
    }

    /// Create a ticket for a Collection (HashSeq format), compatible with sendme.
    fn create_collection_ticket(&self, hash_hex: HexArg) -> PyResult<String> {
        let hash_hex = hash_hex.0;
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

    /// Download a blob by hash (`Hash` or hex) from one provider (`NodeId`
    /// or hex) and return its bytes. `format` is a `BlobFormat` or
    /// `"raw"` / `"hash_seq"` (default raw).
    #[pyo3(signature = (hash_hex, node_id_hex, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn download_hash<'py>(
        &self,
        py: Python<'py>,
        hash_hex: HexArg,
        node_id_hex: HexArg,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        future_into_py(py, async move {
            let data = blobs
                .download_hash(
                    &aster::Hash::from_hex(hash_hex.0),
                    &aster::NodeId::from_hex(node_id_hex.0),
                    format.0,
                )
                .await
                .map_err(aster_err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// As `download_hash`, but leaves the blob in the local store without
    /// returning its bytes (no whole-blob copy for large blobs).
    #[pyo3(signature = (hash, provider, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn download_hash_to_store<'py>(
        &self,
        py: Python<'py>,
        hash: HexArg,
        provider: HexArg,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        future_into_py(py, async move {
            blobs
                .download_hash_to_store(
                    &aster::Hash::from_hex(hash.0),
                    &aster::NodeId::from_hex(provider.0),
                    format.0,
                )
                .await
                .map_err(aster_err_to_py)?;
            Ok(None::<u8>)
        })
    }

    /// Download from an **ordered** list of providers, failing over in
    /// order; returns `(bytes, FetchReport)`. `strategy=SPLIT_CHILDREN`
    /// needs `format=HASH_SEQ`.
    #[pyo3(signature = (hash, providers, format=BlobFormatArg(aster::BlobFormat::Raw), strategy=FetchStrategy::Whole))]
    fn download_hash_from<'py>(
        &self,
        py: Python<'py>,
        hash: HexArg,
        providers: Vec<HexArg>,
        format: BlobFormatArg,
        strategy: FetchStrategy,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        let providers = node_ids(providers);
        future_into_py(py, async move {
            let (data, report) = blobs
                .download_hash_from(
                    &aster::Hash::from_hex(hash.0),
                    &providers,
                    format.0,
                    strategy.into(),
                )
                .await
                .map_err(aster_err_to_py)?;
            Ok((PyBytesResult(data), FetchReport::from(report)))
        })
    }

    /// As `download_hash_from`, but leaves the blob in the local store;
    /// returns the `FetchReport`.
    #[pyo3(signature = (hash, providers, format=BlobFormatArg(aster::BlobFormat::Raw), strategy=FetchStrategy::Whole))]
    fn download_hash_to_store_from<'py>(
        &self,
        py: Python<'py>,
        hash: HexArg,
        providers: Vec<HexArg>,
        format: BlobFormatArg,
        strategy: FetchStrategy,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        let providers = node_ids(providers);
        future_into_py(py, async move {
            let report = blobs
                .download_hash_to_store_from(
                    &aster::Hash::from_hex(hash.0),
                    &providers,
                    format.0,
                    strategy.into(),
                )
                .await
                .map_err(aster_err_to_py)?;
            Ok(FetchReport::from(report))
        })
    }

    /// Download a collection from an ordered provider list; returns
    /// `([(name, bytes), ...], FetchReport)`.
    #[pyo3(signature = (hash, providers, strategy=FetchStrategy::Whole))]
    fn download_collection_from<'py>(
        &self,
        py: Python<'py>,
        hash: HexArg,
        providers: Vec<HexArg>,
        strategy: FetchStrategy,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        let providers = node_ids(providers);
        future_into_py(py, async move {
            let (files, report) = blobs
                .download_collection_from(
                    &aster::Hash::from_hex(hash.0),
                    &providers,
                    strategy.into(),
                )
                .await
                .map_err(aster_err_to_py)?;
            let files: Vec<(String, PyBytesResult)> = files
                .into_iter()
                .map(|(name, data)| (name, PyBytesResult(data)))
                .collect();
            Ok((files, FetchReport::from(report)))
        })
    }

    /// Read `length` bytes at `offset` from a blob in the local store.
    fn read_range<'py>(
        &self,
        py: Python<'py>,
        hash: HexArg,
        offset: u64,
        length: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        future_into_py(py, async move {
            let data = blobs
                .read_range(&aster::Hash::from_hex(hash.0), offset, length)
                .await
                .map_err(aster_err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// Run one blob GC mark+sweep pass now. Reclaims every blob not protected
    /// by a persistent tag, a live temp tag or the store's protect callback
    /// (node-wide).
    fn gc_run_once<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        future_into_py(py, async move {
            blobs.gc_run_once().await.map_err(aster_err_to_py)?;
            Ok(None::<u8>)
        })
    }

    /// Download a collection by hash from a specific node, returning (name, data) pairs.
    fn download_collection_hash<'py>(
        &self,
        py: Python<'py>,
        hash_hex: HexArg,
        node_id_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
        let node_id_hex = node_id_hex.0;
        let client = self.inner.clone();
        future_into_py(py, async move {
            let files = client
                .download_collection_hash(hash_hex, node_id_hex)
                .await
                .map_err(err_to_py)?;
            let result: Vec<(String, PyBytesResult)> = files
                .into_iter()
                .map(|(name, data)| (name, PyBytesResult(data)))
                .collect();
            Ok(result)
        })
    }

    // ── Tag methods ──────────────────────────────────────────────────────────

    /// Set a named tag. `format` is a `BlobFormat` or `"raw"` / `"hash_seq"`
    /// (default raw).
    #[pyo3(signature = (name, hash_hex, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn tag_set<'py>(
        &self,
        py: Python<'py>,
        name: String,
        hash_hex: HexArg,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let blobs = self.sdk.clone();
        future_into_py(py, async move {
            blobs
                .tag_set(name, &aster::Hash::from_hex(hash_hex.0), format.0)
                .await
                .map_err(aster_err_to_py)?;
            Ok(None::<u8>)
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
    fn blob_status<'py>(&self, py: Python<'py>, hash_hex: HexArg) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
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
    fn blob_has<'py>(&self, py: Python<'py>, hash_hex: HexArg) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
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
        hash_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
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
        hash_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
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
        hash_hex: HexArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash_hex = hash_hex.0;
        let client = self.inner.clone();
        future_into_py(py, async move {
            let r = client.blob_local_info(hash_hex).await.map_err(err_to_py)?;
            Ok(BlobLocalInfo::from(r))
        })
    }
}

/// Create a BlobsClient from a Node.
#[pyfunction]
pub fn blobs_client(node: &Node) -> BlobsClient {
    BlobsClient {
        inner: node.inner().blobs_client(),
        sdk: node.inner.blobs(),
    }
}

/// Register the blobs types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<BlobStatusResult>()?;
    m.add_class::<BlobObserveResult>()?;
    m.add_class::<BlobLocalInfo>()?;
    m.add_class::<TagInfo>()?;
    m.add_class::<BlobsClient>()?;
    m.add_class::<BlobFormat>()?;
    m.add_class::<FetchStrategy>()?;
    m.add_class::<FetchReport>()?;
    m.add_class::<ProviderOutcome>()?;
    m.add_function(wrap_pyfunction!(blobs_client, m)?)?;
    Ok(())
}
