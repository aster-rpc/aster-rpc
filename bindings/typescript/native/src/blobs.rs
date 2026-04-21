//! Blobs module — wraps CoreBlobsClient.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::CoreBlobsClient;

use crate::error::to_napi_err;

#[napi]
pub struct BlobsClient {
    pub(crate) inner: CoreBlobsClient,
}

impl From<CoreBlobsClient> for BlobsClient {
    fn from(inner: CoreBlobsClient) -> Self {
        Self { inner }
    }
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
    pub async fn download_collection(
        &self,
        ticket: String,
    ) -> Result<Vec<CollectionFile>> {
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

    /// Get blob status (complete/partial/missing).
    #[napi]
    pub async fn blob_status(&self, hash_hex: String) -> Result<String> {
        let status = self
            .inner
            .clone()
            .blob_status(hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(format!("{:?}", status))
    }

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
