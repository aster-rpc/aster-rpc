//! Blob storage client.

use crate::error::{Error, Result};
use crate::id::{Hash, NodeId};
use aster_transport_core::{
    CoreBlobStatus, CoreBlobsClient, CoreFetchReport, CoreFetchStrategy, CoreProviderOutcome,
};

/// The on-wire format of a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlobFormat {
    /// A single raw blob.
    #[default]
    Raw,
    /// A HashSeq (collection of blobs).
    HashSeq,
}

impl BlobFormat {
    fn as_core(self) -> String {
        match self {
            BlobFormat::Raw => "raw".into(),
            BlobFormat::HashSeq => "hash_seq".into(),
        }
    }
}

/// Local storage status of a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobStatus {
    /// Not present locally.
    NotFound,
    /// Partially present (`size` bytes known/available).
    Partial { size: u64 },
    /// Fully present locally.
    Complete { size: u64 },
}

impl From<CoreBlobStatus> for BlobStatus {
    fn from(s: CoreBlobStatus) -> Self {
        match s {
            CoreBlobStatus::NotFound => BlobStatus::NotFound,
            CoreBlobStatus::Partial { size } => BlobStatus::Partial { size },
            CoreBlobStatus::Complete { size } => BlobStatus::Complete { size },
        }
    }
}

/// How a multi-provider fetch decomposes a HashSeq.
///
/// This says nothing about how many providers are used: an ordered provider
/// list is walked with failover in **both** variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FetchStrategy {
    /// Fetch the HashSeq as one request. One transfer is in flight at a time,
    /// and the provider list is walked strictly in order.
    #[default]
    Whole,
    /// Fetch the root, then each child as its own request, up to 32
    /// concurrently.
    ///
    /// **Order is honoured per child, not fleet-wide.** Every child starts at
    /// the top of the provider list, so "direct before relay" is a property of
    /// each individual request rather than a schedule across the fleet. Only
    /// valid for [`BlobFormat::HashSeq`].
    SplitChildren,
}

impl FetchStrategy {
    fn as_core(self) -> CoreFetchStrategy {
        match self {
            FetchStrategy::Whole => CoreFetchStrategy::Whole,
            FetchStrategy::SplitChildren => CoreFetchStrategy::SplitChildren,
        }
    }
}

/// What one provider did during a fetch.
#[derive(Clone, Debug)]
pub struct ProviderOutcome {
    node_id: NodeId,
    attempts: u32,
    failures: u32,
    completed_requests: u32,
}

impl ProviderOutcome {
    /// The provider this outcome describes.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }
    /// How many requests were attempted against this provider. An attempt means
    /// a real dial or transfer: a request that was already fully resident
    /// locally is not counted.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
    /// How many of those attempts failed to connect or to transfer.
    pub fn failures(&self) -> u32 {
        self.failures
    }
    /// How many requests this provider carried to completion.
    pub fn completed_requests(&self) -> u32 {
        self.completed_requests
    }
}

impl From<CoreProviderOutcome> for ProviderOutcome {
    fn from(o: CoreProviderOutcome) -> Self {
        Self {
            node_id: NodeId::from_hex(o.node_id),
            attempts: o.attempts,
            failures: o.failures,
            completed_requests: o.completed_requests,
        }
    }
}

/// What a multi-provider fetch did, for callers that rank providers and want
/// to feed the result back into that ranking.
#[derive(Clone, Debug)]
pub struct FetchReport {
    providers: Vec<ProviderOutcome>,
    bytes_transferred: u64,
}

impl FetchReport {
    /// Per-provider outcomes, in the order each provider was first attempted.
    /// Repeated use of one provider (across split children, or across retries)
    /// is aggregated into a single entry.
    pub fn providers(&self) -> &[ProviderOutcome] {
        &self.providers
    }

    /// Providers that were actually dialled, in first-attempt order.
    pub fn attempted(&self) -> impl Iterator<Item = &NodeId> {
        self.providers.iter().map(|p| &p.node_id)
    }

    /// Providers where at least one request failed to connect or transfer.
    /// A provider can be both failed and served — e.g. it completed one split
    /// child and could not serve another.
    pub fn failed(&self) -> impl Iterator<Item = &NodeId> {
        self.providers
            .iter()
            .filter(|p| p.failures > 0)
            .map(|p| &p.node_id)
    }

    /// Providers that carried at least one request to completion.
    pub fn served(&self) -> impl Iterator<Item = &NodeId> {
        self.providers
            .iter()
            .filter(|p| p.completed_requests > 0)
            .map(|p| &p.node_id)
    }

    /// Payload bytes successfully decoded from providers during this fetch.
    ///
    /// Excludes protocol overhead and anything that was already resident
    /// locally. A provider that transferred part of a blob before failing is
    /// counted up to its last valid chunk, so each byte is counted once even
    /// when the fetch failed over between providers.
    pub fn bytes_transferred(&self) -> u64 {
        self.bytes_transferred
    }
}

impl From<CoreFetchReport> for FetchReport {
    fn from(r: CoreFetchReport) -> Self {
        Self {
            providers: r.providers.into_iter().map(Into::into).collect(),
            bytes_transferred: r.bytes_transferred,
        }
    }
}

/// A handle to a node's blob store. Cheap to clone.
#[derive(Clone)]
pub struct Blobs {
    inner: CoreBlobsClient,
}

impl Blobs {
    pub(crate) fn new(inner: CoreBlobsClient) -> Self {
        Self { inner }
    }

    /// Store raw bytes; returns the blob hash.
    pub async fn add_bytes(&self, data: impl Into<Vec<u8>>) -> Result<Hash> {
        Ok(Hash::from_hex(self.inner.add_bytes(data.into()).await?))
    }

    /// Import a file into the store (copy import); returns the blob hash.
    pub async fn add_path(&self, path: impl Into<String>) -> Result<Hash> {
        Ok(Hash::from_hex(self.inner.add_path(path.into()).await?))
    }

    /// Import a file and protect it with a caller-chosen persistent tag, in one
    /// step; returns the blob hash. The tag controls GC lifetime.
    pub async fn add_path_with_named_tag(
        &self,
        path: impl Into<String>,
        tag_name: impl Into<String>,
    ) -> Result<Hash> {
        Ok(Hash::from_hex(
            self.inner
                .add_path_with_named_tag(path.into(), tag_name.into())
                .await?,
        ))
    }

    /// Read a blob's bytes from the local store.
    pub async fn read_to_bytes(&self, hash: &Hash) -> Result<Vec<u8>> {
        Ok(self.inner.read_to_bytes(hash.as_str().to_string()).await?)
    }

    /// Read a byte range from a blob in the local store.
    pub async fn read_range(&self, hash: &Hash, offset: u64, len: u64) -> Result<Vec<u8>> {
        Ok(self
            .inner
            .read_range(hash.as_str().to_string(), offset, len)
            .await?)
    }

    /// Whether the blob is fully present locally.
    pub async fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self.inner.blob_has(hash.as_str().to_string()).await?)
    }

    /// The blob's local storage status.
    pub async fn status(&self, hash: &Hash) -> Result<BlobStatus> {
        Ok(self
            .inner
            .blob_status(hash.as_str().to_string())
            .await?
            .into())
    }

    /// Download a blob by hash from a specific peer; returns the bytes.
    pub async fn download_hash(
        &self,
        hash: &Hash,
        from: &NodeId,
        format: BlobFormat,
    ) -> Result<Vec<u8>> {
        Ok(self
            .inner
            .download_hash(
                hash.as_str().to_string(),
                from.as_str().to_string(),
                format.as_core(),
            )
            .await?)
    }

    /// Download a blob by hash from a specific peer into the local store
    /// **without** returning its bytes — for callers that only need the blob
    /// resident (to serve it later via ranged reads). Avoids the whole-blob
    /// `get_bytes().to_vec()` copy that [`Self::download_hash`] pays to hand
    /// the data back (a ~2× transient RSS spike on large blobs).
    pub async fn download_hash_to_store(
        &self,
        hash: &Hash,
        from: &NodeId,
        format: BlobFormat,
    ) -> Result<()> {
        Ok(self
            .inner
            .download_hash_to_store(
                hash.as_str().to_string(),
                from.as_str().to_string(),
                format.as_core(),
            )
            .await?)
    }

    /// Download a blob by hash from an **ordered** list of candidate providers,
    /// returning the bytes and a report of what each provider did.
    ///
    /// Providers are tried strictly in the order given, moving to the next only
    /// when one fails to connect or to transfer. Each attempt asks only for the
    /// ranges still missing, so a partial transfer from an earlier candidate is
    /// never re-fetched. Ranking the candidates is the caller's job.
    ///
    /// See [`FetchStrategy`] for how `strategy` affects ordering — in
    /// [`FetchStrategy::SplitChildren`] the order applies per child, not
    /// fleet-wide.
    pub async fn download_hash_from(
        &self,
        hash: &Hash,
        from: &[NodeId],
        format: BlobFormat,
        strategy: FetchStrategy,
    ) -> Result<(Vec<u8>, FetchReport)> {
        let providers = Self::providers(hash, from, format, strategy)?;
        let (bytes, report) = self
            .inner
            .download_hash_multi(
                hash.as_str().to_string(),
                providers,
                format.as_core(),
                strategy.as_core(),
            )
            .await?;
        Ok((bytes, report.into()))
    }

    /// As [`Self::download_hash_from`], but leaves the blob in the local store
    /// **without** returning its bytes — see [`Self::download_hash_to_store`]
    /// for why that matters on large blobs.
    pub async fn download_hash_to_store_from(
        &self,
        hash: &Hash,
        from: &[NodeId],
        format: BlobFormat,
        strategy: FetchStrategy,
    ) -> Result<FetchReport> {
        let providers = Self::providers(hash, from, format, strategy)?;
        Ok(self
            .inner
            .download_hash_to_store_multi(
                hash.as_str().to_string(),
                providers,
                format.as_core(),
                strategy.as_core(),
            )
            .await?
            .into())
    }

    /// As [`Self::download_hash_from`], for a collection: returns its
    /// `(name, bytes)` entries plus the fetch report.
    pub async fn download_collection_from(
        &self,
        hash: &Hash,
        from: &[NodeId],
        strategy: FetchStrategy,
    ) -> Result<(Vec<(String, Vec<u8>)>, FetchReport)> {
        let providers = Self::providers(hash, from, BlobFormat::HashSeq, strategy)?;
        let (files, report) = self
            .inner
            .download_collection_hash_multi(
                hash.as_str().to_string(),
                providers,
                strategy.as_core(),
            )
            .await?;
        Ok((files, report.into()))
    }

    /// Validate the provider list and strategy, and hand the ids to the core in
    /// the caller's order — never sorted, deduplicated, or shuffled. Duplicate
    /// ids are legal and are aggregated by id in the report.
    fn providers(
        hash: &Hash,
        from: &[NodeId],
        format: BlobFormat,
        strategy: FetchStrategy,
    ) -> Result<Vec<String>> {
        if from.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "no providers supplied for download of {hash}"
            )));
        }
        if strategy == FetchStrategy::SplitChildren && format != BlobFormat::HashSeq {
            return Err(Error::InvalidArgument(format!(
                "FetchStrategy::SplitChildren needs BlobFormat::HashSeq, got {format:?} for {hash}"
            )));
        }
        Ok(from.iter().map(|id| id.as_str().to_string()).collect())
    }

    /// Store a collection (a HashSeq of named blobs) from `(name, bytes)`
    /// entries; returns the collection hash. Used to publish a contract
    /// collection (`contract.bin` + `types/*.bin` + `manifest.json`).
    pub async fn add_collection(&self, entries: Vec<(String, Vec<u8>)>) -> Result<Hash> {
        Ok(Hash::from_hex(self.inner.add_collection(entries).await?))
    }

    /// Download a collection by hash from a specific peer; returns its
    /// `(name, bytes)` entries.
    pub async fn download_collection(
        &self,
        hash: &Hash,
        from: &NodeId,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(self
            .inner
            .download_collection_hash(hash.as_str().to_string(), from.as_str().to_string())
            .await?)
    }

    // ── Tags ───────────────────────────────────────────────────────────────

    /// Set a persistent named tag pointing at a blob.
    pub async fn tag_set(
        &self,
        name: impl Into<String>,
        hash: &Hash,
        format: BlobFormat,
    ) -> Result<()> {
        Ok(self
            .inner
            .tag_set(name.into(), hash.as_str().to_string(), format.as_core())
            .await?)
    }

    /// Delete a tag by name; returns the number removed (0 or 1).
    pub async fn tag_delete(&self, name: impl Into<String>) -> Result<u64> {
        Ok(self.inner.tag_delete(name.into()).await?)
    }

    /// Delete all tags matching `prefix`; returns the number removed.
    pub async fn tag_delete_prefix(&self, prefix: impl Into<String>) -> Result<u64> {
        Ok(self.inner.tag_delete_prefix(prefix.into()).await?)
    }

    // ── Garbage collection ──────────────────────────────────────────────────

    /// Run exactly one blob GC mark+sweep pass now and return when it
    /// completes.
    ///
    /// Reclaims every blob not protected by a persistent tag, a live temp-tag,
    /// or the store's protect callback. Works whether or not periodic GC is
    /// enabled via [`AsterConfigBuilder::gc_interval`](crate::AsterConfigBuilder::gc_interval),
    /// giving tests a deterministic reclamation point without waiting on the
    /// timed interval.
    ///
    /// GC is **node-wide** over the one shared blob store: it collects any
    /// untagged blob regardless of which tree added it. Keep a live tag for
    /// every blob you want to retain.
    pub async fn gc_run_once(&self) -> Result<()> {
        Ok(self.inner.gc_run_once().await?)
    }
}
