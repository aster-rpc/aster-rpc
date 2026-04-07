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

    /// Add bytes as a named collection entry, returns collection hash.
    #[napi]
    pub async fn add_bytes_as_collection(&self, name: String, data: Buffer) -> Result<String> {
        self.inner
            .clone()
            .add_bytes_as_collection(name, data.to_vec())
            .await
            .map_err(to_napi_err)
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
        self.inner.clone().tag_set(name, hash_hex, format).await.map_err(to_napi_err)
    }

    /// Get a tag. Returns null if not found.
    #[napi]
    pub async fn tag_get(&self, name: String) -> Result<Option<String>> {
        let info = self.inner.clone().tag_get(name).await.map_err(to_napi_err)?;
        Ok(info.map(|t| t.hash))
    }

    /// Delete a tag. Returns number of tags deleted.
    #[napi]
    pub async fn tag_delete(&self, name: String) -> Result<u32> {
        self.inner.clone().tag_delete(name).await.map(|n| n as u32).map_err(to_napi_err)
    }

    /// List tags with a given prefix.
    #[napi]
    pub async fn tag_list_prefix(&self, prefix: String) -> Result<Vec<String>> {
        let tags = self.inner.clone().tag_list_prefix(prefix).await.map_err(to_napi_err)?;
        Ok(tags.into_iter().map(|t| t.name).collect())
    }

    // -- Observability --------------------------------------------------------

    /// Get blob status (complete/partial/missing).
    #[napi]
    pub async fn blob_status(&self, hash_hex: String) -> Result<String> {
        let status = self.inner.clone().blob_status(hash_hex).await.map_err(to_napi_err)?;
        Ok(format!("{:?}", status))
    }

    /// Wait for a blob download to complete.
    #[napi]
    pub async fn blob_observe_complete(&self, hash_hex: String) -> Result<()> {
        self.inner.clone().blob_observe_complete(hash_hex).await.map_err(to_napi_err)
    }
}
