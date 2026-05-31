//! Blob storage client.

use crate::error::Result;
use crate::id::{Hash, NodeId};
use aster_transport_core::{CoreBlobStatus, CoreBlobsClient};

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
}
