//! Blobs — the `aster` crate's [`aster::Blobs`] (ranged reads, multi-provider
//! fetch with a report, store-only downloads, tag prefixes, GC) plus the
//! transport-core ticket / collection / observe helpers.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::{BlobFormat, BlobStatus, FetchStrategy, Hash, NodeId};
use aster_transport_core::CoreBlobsClient;

use crate::config::aster_err;
use crate::error::to_napi_err;

#[napi]
pub struct BlobsClient {
    pub(crate) inner: CoreBlobsClient,
    pub(crate) blobs: aster::Blobs,
}

impl BlobsClient {
    pub(crate) fn new(node: &aster::Node) -> Self {
        Self {
            inner: node.core_node().blobs_client(),
            blobs: node.blobs(),
        }
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

fn format_in(format: Option<&str>) -> Result<BlobFormat> {
    match format.unwrap_or("raw") {
        "raw" => Ok(BlobFormat::Raw),
        "hash_seq" => Ok(BlobFormat::HashSeq),
        other => Err(invalid(format!(
            "unknown blob format {other:?} (expected 'raw' or 'hash_seq')"
        ))),
    }
}

fn strategy_in(strategy: Option<&str>) -> Result<FetchStrategy> {
    match strategy.unwrap_or("whole") {
        "whole" => Ok(FetchStrategy::Whole),
        "split_children" => Ok(FetchStrategy::SplitChildren),
        other => Err(invalid(format!(
            "unknown fetch strategy {other:?} (expected 'whole' or 'split_children')"
        ))),
    }
}

fn u64_in(what: &str, v: f64) -> Result<u64> {
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 || v > 9_007_199_254_740_991.0 {
        return Err(invalid(format!(
            "{what} must be a non-negative safe integer"
        )));
    }
    Ok(v as u64)
}

fn providers_in(from: Vec<String>) -> Vec<NodeId> {
    from.into_iter().map(NodeId::from_hex).collect()
}

/// Local storage status of a blob.
#[napi(object)]
pub struct BlobStatusInfo {
    #[napi(ts_type = "'not_found' | 'partial' | 'complete'")]
    pub state: String,
    /// Bytes stored (`partial`: known so far, `0` if unknown) or the blob
    /// size (`complete`). `null` for `not_found`.
    pub size: Option<f64>,
}

pub(crate) fn blob_status_out(s: BlobStatus) -> BlobStatusInfo {
    match s {
        BlobStatus::NotFound => BlobStatusInfo {
            state: "not_found".into(),
            size: None,
        },
        BlobStatus::Partial { size } => BlobStatusInfo {
            state: "partial".into(),
            size: Some(size as f64),
        },
        BlobStatus::Complete { size } => BlobStatusInfo {
            state: "complete".into(),
            size: Some(size as f64),
        },
    }
}

/// What one provider did during a multi-provider fetch.
#[napi(object)]
pub struct ProviderOutcome {
    pub node_id: String,
    /// Requests attempted against this provider (real dials / transfers).
    pub attempts: u32,
    /// Attempts that failed to connect or transfer.
    pub failures: u32,
    /// Requests this provider carried to completion.
    pub completed_requests: u32,
}

/// What a multi-provider fetch did — feed it back into provider ranking.
#[napi(object)]
pub struct FetchReport {
    /// Per-provider outcomes, in first-attempt order (repeats aggregated).
    pub providers: Vec<ProviderOutcome>,
    /// Providers actually dialled, in first-attempt order.
    pub attempted: Vec<String>,
    /// Providers where at least one request failed.
    pub failed: Vec<String>,
    /// Providers that completed at least one request.
    pub served: Vec<String>,
    /// Payload bytes pulled off the wire (a transfer metric, not a size).
    pub bytes_transferred: f64,
}

impl From<aster::FetchReport> for FetchReport {
    fn from(r: aster::FetchReport) -> Self {
        let ids = |it: &mut dyn Iterator<Item = &NodeId>| -> Vec<String> {
            it.map(|id| id.as_str().to_string()).collect()
        };
        Self {
            attempted: ids(&mut r.attempted()),
            failed: ids(&mut r.failed()),
            served: ids(&mut r.served()),
            bytes_transferred: r.bytes_transferred() as f64,
            providers: r
                .providers()
                .iter()
                .map(|p| ProviderOutcome {
                    node_id: p.node_id().as_str().to_string(),
                    attempts: p.attempts(),
                    failures: p.failures(),
                    completed_requests: p.completed_requests(),
                })
                .collect(),
        }
    }
}

/// Options for the multi-provider downloads.
#[napi(object)]
#[derive(Default)]
pub struct FetchOptions {
    /// `'raw'` (default) or `'hash_seq'`.
    #[napi(ts_type = "'raw' | 'hash_seq'")]
    pub format: Option<String>,
    /// `'whole'` (default) or `'split_children'` (HashSeq only; provider
    /// order applies per child).
    #[napi(ts_type = "'whole' | 'split_children'")]
    pub strategy: Option<String>,
}

/// Bytes plus the fetch report (`downloadHashFrom`).
#[napi(object)]
pub struct FetchResult {
    pub data: Buffer,
    pub report: FetchReport,
}

/// Collection entries plus the fetch report (`downloadCollectionFrom`).
#[napi(object)]
pub struct CollectionFetchResult {
    pub files: Vec<CollectionFile>,
    pub report: FetchReport,
}

#[napi]
impl BlobsClient {
    /// Add bytes to the blob store, returning the hash hex string.
    #[napi]
    pub async fn add_bytes(&self, data: Buffer) -> Result<String> {
        self.inner
            .clone()
            .add_bytes(data.to_vec())
            .await
            .map_err(to_napi_err)
    }

    /// Import a file by path (Copy mode, never TryReference) and return the
    /// hash hex string. Like `addBytes`, an auto-tag keeps the blob alive; use
    /// `addPathWithNamedTag` for a deterministic GC-keyed tag.
    #[napi]
    pub async fn add_path(&self, path: String) -> Result<String> {
        self.inner.clone().add_path(path).await.map_err(to_napi_err)
    }

    /// Import a file by path and set a persistent named tag in one step,
    /// returning the hash hex string.
    #[napi]
    pub async fn add_path_with_named_tag(&self, path: String, tag_name: String) -> Result<String> {
        self.inner
            .clone()
            .add_path_with_named_tag(path, tag_name)
            .await
            .map_err(to_napi_err)
    }

    /// Read a blob by hash hex string.
    #[napi]
    pub async fn read(&self, hash_hex: String) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .read_to_bytes(hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Create a download ticket for a blob (sync).
    #[napi]
    pub fn create_ticket(&self, hash_hex: String) -> Result<String> {
        self.inner.create_ticket(hash_hex).map_err(to_napi_err)
    }

    /// Download a blob using a ticket string. Returns the blob data.
    #[napi]
    pub async fn download_blob(&self, ticket: String) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .download_blob(ticket)
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Download a contract collection (HashSeq) by ticket. Pulls the
    /// HashSeq and all child blobs into the local store. Returns the
    /// (name, data) entries for callers that want to consume them
    /// immediately; local reads via `list_collection` / `read` also
    /// work afterward because the collection is now persisted.
    #[napi]
    pub async fn download_collection(&self, ticket: String) -> Result<Vec<CollectionFile>> {
        let files = self
            .inner
            .clone()
            .download_collection(ticket)
            .await
            .map_err(to_napi_err)?;
        Ok(files
            .into_iter()
            .map(|(name, data)| CollectionFile {
                name,
                data: Buffer::from(data),
            })
            .collect())
    }

    /// Download a HashSeq collection by raw hash + node id. The ticket-
    /// less variant used when the caller already knows the collection
    /// hash (e.g. from the registry doc's ArtifactRef) and the remote
    /// peer's endpoint id (captured at connect time). Unlike
    /// `download_collection` this sets `BlobFormat::HashSeq` explicitly,
    /// which is required for HashSeq collections to pull child blobs.
    #[napi]
    pub async fn download_collection_hash(
        &self,
        hash_hex: String,
        node_id_hex: String,
    ) -> Result<Vec<CollectionFile>> {
        let files = self
            .inner
            .clone()
            .download_collection_hash(hash_hex, node_id_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(files
            .into_iter()
            .map(|(name, data)| CollectionFile {
                name,
                data: Buffer::from(data),
            })
            .collect())
    }

    /// Add bytes as a named collection entry, returns collection hash.
    #[napi]
    pub async fn add_bytes_as_collection(&self, name: String, data: Buffer) -> Result<String> {
        self.inner
            .clone()
            .add_bytes_as_collection(name, data.to_vec())
            .await
            .map_err(to_napi_err)
    }

    /// Store a multi-file collection (HashSeq). Takes an array of [name, data] pairs.
    /// Returns the collection hash hex. The collection is auto-tagged for GC protection.
    #[napi]
    pub async fn add_collection(&self, entries: Vec<(String, Buffer)>) -> Result<String> {
        let core_entries: Vec<(String, Vec<u8>)> = entries
            .into_iter()
            .map(|(name, data)| (name, data.to_vec()))
            .collect();
        self.inner
            .clone()
            .add_collection(core_entries)
            .await
            .map_err(to_napi_err)
    }

    /// List entries from a stored collection by its hash.
    /// Returns an array of { name, hash, size } objects.
    #[napi]
    pub async fn list_collection(&self, hash_hex: String) -> Result<Vec<CollectionEntry>> {
        let entries = self
            .inner
            .clone()
            .list_collection(hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(entries
            .into_iter()
            .map(|(name, hash, size)| CollectionEntry {
                name,
                hash,
                size: size as f64,
            })
            .collect())
    }

    /// Create a collection ticket from hash (sync).
    #[napi]
    pub fn create_collection_ticket(&self, hash_hex: String) -> Result<String> {
        self.inner
            .create_collection_ticket(hash_hex)
            .map_err(to_napi_err)
    }

    /// Check if a blob exists locally.
    #[napi]
    pub async fn has(&self, hash_hex: String) -> Result<bool> {
        self.inner
            .clone()
            .blob_has(hash_hex)
            .await
            .map_err(to_napi_err)
    }

    /// Read `len` bytes at `offset` from a blob in the local store.
    #[napi]
    pub async fn read_range(&self, hash_hex: String, offset: f64, len: f64) -> Result<Buffer> {
        let (offset, len) = (u64_in("offset", offset)?, u64_in("len", len)?);
        Ok(Buffer::from(
            self.blobs
                .read_range(&Hash::from_hex(hash_hex), offset, len)
                .await
                .map_err(aster_err)?,
        ))
    }

    /// The blob's local storage status.
    #[napi]
    pub async fn status(&self, hash_hex: String) -> Result<BlobStatusInfo> {
        Ok(blob_status_out(
            self.blobs
                .status(&Hash::from_hex(hash_hex))
                .await
                .map_err(aster_err)?,
        ))
    }

    /// Download a blob by hash from one peer; returns the bytes.
    #[napi]
    pub async fn download_hash(
        &self,
        hash_hex: String,
        from: String,
        #[napi(ts_arg_type = "'raw' | 'hash_seq'")] format: Option<String>,
    ) -> Result<Buffer> {
        let format = format_in(format.as_deref())?;
        Ok(Buffer::from(
            self.blobs
                .download_hash(&Hash::from_hex(hash_hex), &NodeId::from_hex(from), format)
                .await
                .map_err(aster_err)?,
        ))
    }

    /// Download a blob by hash from one peer into the local store without
    /// returning the bytes (no whole-blob copy; read it later with
    /// `readRange`).
    #[napi]
    pub async fn download_hash_to_store(
        &self,
        hash_hex: String,
        from: String,
        #[napi(ts_arg_type = "'raw' | 'hash_seq'")] format: Option<String>,
    ) -> Result<()> {
        let format = format_in(format.as_deref())?;
        self.blobs
            .download_hash_to_store(&Hash::from_hex(hash_hex), &NodeId::from_hex(from), format)
            .await
            .map_err(aster_err)
    }

    /// Download from an **ordered** provider list (failover in order; each
    /// attempt fetches only missing ranges). Returns bytes and a report.
    #[napi]
    pub async fn download_hash_from(
        &self,
        hash_hex: String,
        providers: Vec<String>,
        options: Option<FetchOptions>,
    ) -> Result<FetchResult> {
        let o = options.unwrap_or_default();
        let (format, strategy) = (
            format_in(o.format.as_deref())?,
            strategy_in(o.strategy.as_deref())?,
        );
        let (data, report) = self
            .blobs
            .download_hash_from(
                &Hash::from_hex(hash_hex),
                &providers_in(providers),
                format,
                strategy,
            )
            .await
            .map_err(aster_err)?;
        Ok(FetchResult {
            data: Buffer::from(data),
            report: report.into(),
        })
    }

    /// As `downloadHashFrom`, leaving the blob in the store; returns the
    /// report only.
    #[napi]
    pub async fn download_hash_to_store_from(
        &self,
        hash_hex: String,
        providers: Vec<String>,
        options: Option<FetchOptions>,
    ) -> Result<FetchReport> {
        let o = options.unwrap_or_default();
        let (format, strategy) = (
            format_in(o.format.as_deref())?,
            strategy_in(o.strategy.as_deref())?,
        );
        Ok(self
            .blobs
            .download_hash_to_store_from(
                &Hash::from_hex(hash_hex),
                &providers_in(providers),
                format,
                strategy,
            )
            .await
            .map_err(aster_err)?
            .into())
    }

    /// As `downloadHashFrom` for a collection: its `(name, data)` entries
    /// plus the report.
    #[napi]
    pub async fn download_collection_from(
        &self,
        hash_hex: String,
        providers: Vec<String>,
        #[napi(ts_arg_type = "'whole' | 'split_children'")] strategy: Option<String>,
    ) -> Result<CollectionFetchResult> {
        let strategy = strategy_in(strategy.as_deref())?;
        let (files, report) = self
            .blobs
            .download_collection_from(
                &Hash::from_hex(hash_hex),
                &providers_in(providers),
                strategy,
            )
            .await
            .map_err(aster_err)?;
        Ok(CollectionFetchResult {
            files: files
                .into_iter()
                .map(|(name, data)| CollectionFile {
                    name,
                    data: Buffer::from(data),
                })
                .collect(),
            report: report.into(),
        })
    }

    /// Delete every tag whose name starts with `prefix`; returns how many.
    #[napi]
    pub async fn tag_delete_prefix(&self, prefix: String) -> Result<f64> {
        Ok(self
            .blobs
            .tag_delete_prefix(prefix)
            .await
            .map_err(aster_err)? as f64)
    }

    /// Run one blob GC mark-and-sweep pass now. Node-wide: every blob not
    /// protected by a tag (or a live temp tag) is reclaimed.
    #[napi]
    pub async fn gc_run_once(&self) -> Result<()> {
        self.blobs.gc_run_once().await.map_err(aster_err)
    }

    // -- Tags -----------------------------------------------------------------

    /// Set a tag.
    #[napi]
    pub async fn tag_set(&self, name: String, hash_hex: String, format: String) -> Result<()> {
        self.inner
            .clone()
            .tag_set(name, hash_hex, format)
            .await
            .map_err(to_napi_err)
    }

    /// Get a tag. Returns null if not found.
    #[napi]
    pub async fn tag_get(&self, name: String) -> Result<Option<String>> {
        let info = self
            .inner
            .clone()
            .tag_get(name)
            .await
            .map_err(to_napi_err)?;
        Ok(info.map(|t| t.hash))
    }

    /// Delete a tag. Returns number of tags deleted.
    #[napi]
    pub async fn tag_delete(&self, name: String) -> Result<u32> {
        self.inner
            .clone()
            .tag_delete(name)
            .await
            .map(|n| n as u32)
            .map_err(to_napi_err)
    }

    /// List tags with a given prefix.
    #[napi]
    pub async fn tag_list_prefix(&self, prefix: String) -> Result<Vec<String>> {
        let tags = self
            .inner
            .clone()
            .tag_list_prefix(prefix)
            .await
            .map_err(to_napi_err)?;
        Ok(tags.into_iter().map(|t| t.name).collect())
    }

    // -- Observability --------------------------------------------------------

    /// Wait for a blob download to complete.
    #[napi]
    pub async fn blob_observe_complete(&self, hash_hex: String) -> Result<()> {
        self.inner
            .clone()
            .blob_observe_complete(hash_hex)
            .await
            .map_err(to_napi_err)
    }

    /// Get a snapshot of blob download progress.
    /// Returns { isComplete: boolean, size: number }.
    #[napi]
    pub async fn blob_observe_snapshot(&self, hash_hex: String) -> Result<BlobObserveResult> {
        let result = self
            .inner
            .clone()
            .blob_observe_snapshot(hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(BlobObserveResult {
            is_complete: result.is_complete,
            size: result.size as f64,
        })
    }

    /// Get local info for a blob.
    /// Returns { isComplete: boolean, localBytes: number }.
    #[napi]
    pub async fn blob_local_info(&self, hash_hex: String) -> Result<BlobLocalInfo> {
        let result = self
            .inner
            .clone()
            .blob_local_info(hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(BlobLocalInfo {
            is_complete: result.is_complete,
            local_bytes: result.local_bytes as f64,
        })
    }
}

/// Blob observe result.
#[napi(object)]
pub struct BlobObserveResult {
    pub is_complete: bool,
    pub size: f64,
}

/// Blob local info.
#[napi(object)]
pub struct BlobLocalInfo {
    pub is_complete: bool,
    pub local_bytes: f64,
}

/// A single entry in a collection.
#[napi(object)]
pub struct CollectionEntry {
    pub name: String,
    pub hash: String,
    pub size: f64,
}

/// A (name, data) pair returned by `download_collection`.
#[napi(object)]
pub struct CollectionFile {
    pub name: String,
    pub data: Buffer,
}
