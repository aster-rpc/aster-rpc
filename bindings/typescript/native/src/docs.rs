//! Docs module — wraps CoreDocsClient and CoreDoc.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::{CoreDoc, CoreDocsClient};

use crate::error::to_napi_err;

#[napi]
pub struct DocsClient {
    pub(crate) inner: CoreDocsClient,
}

impl From<CoreDocsClient> for DocsClient {
    fn from(inner: CoreDocsClient) -> Self {
        Self { inner }
    }
}

#[napi]
impl DocsClient {
    /// Create a new document.
    #[napi]
    pub async fn create(&self) -> Result<DocHandle> {
        let doc = self.inner.clone().create().await.map_err(to_napi_err)?;
        Ok(DocHandle { inner: doc })
    }

    /// Join a document by ticket string.
    #[napi]
    pub async fn join(&self, ticket: String) -> Result<DocHandle> {
        let doc = self.inner.clone().join(ticket).await.map_err(to_napi_err)?;
        Ok(DocHandle { inner: doc })
    }
}

#[napi]
pub struct DocHandle {
    inner: CoreDoc,
}

#[napi]
impl DocHandle {
    /// Set a key-value pair. Returns the content hash.
    #[napi]
    pub async fn set_bytes(&self, author_hex: String, key: String, value: Buffer) -> Result<String> {
        self.inner
            .clone()
            .set_bytes(author_hex, key.into_bytes(), value.to_vec())
            .await
            .map_err(to_napi_err)
    }

    /// Get a value by author + key (returns None if not found).
    #[napi]
    pub async fn get_exact(&self, author_hex: String, key: String) -> Result<Option<Buffer>> {
        match self
            .inner
            .clone()
            .get_exact(author_hex, key.into_bytes())
            .await
        {
            Ok(Some(data)) => Ok(Some(Buffer::from(data))),
            Ok(None) => Ok(None),
            Err(e) => Err(to_napi_err(e)),
        }
    }

    /// Share the document, returning a ticket string.
    #[napi]
    pub async fn share(&self, mode: String) -> Result<String> {
        self.inner.clone().share(mode).await.map_err(to_napi_err)
    }

    /// Get the document ID.
    #[napi]
    pub fn doc_id(&self) -> String {
        self.inner.doc_id()
    }

    /// Query entries by exact key.
    #[napi]
    pub async fn query_key_exact(&self, key: String) -> Result<Vec<String>> {
        let entries = self.inner.clone().query_key_exact(key.into_bytes()).await.map_err(to_napi_err)?;
        Ok(entries.into_iter().map(|e| format!("{}:{}", e.author_id, e.content_hash)).collect())
    }

    /// Query entries by key prefix.
    #[napi]
    pub async fn query_key_prefix(&self, prefix: String) -> Result<Vec<String>> {
        let entries = self.inner.clone().query_key_prefix(prefix.into_bytes()).await.map_err(to_napi_err)?;
        Ok(entries.into_iter().map(|e| format!("{}:{}", e.author_id, e.content_hash)).collect())
    }

    /// Read entry content by content hash.
    #[napi]
    pub async fn read_entry_content(&self, content_hash_hex: String) -> Result<Buffer> {
        let data = self.inner.clone().read_entry_content(content_hash_hex).await.map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Start sync with peers.
    #[napi]
    pub async fn start_sync(&self, peers: Vec<String>) -> Result<()> {
        self.inner.clone().start_sync(peers).await.map_err(to_napi_err)
    }

    /// Leave the document (stop syncing).
    #[napi]
    pub async fn leave(&self) -> Result<()> {
        self.inner.clone().leave().await.map_err(to_napi_err)
    }

    /// Share with full relay+address info.
    #[napi]
    pub async fn share_with_addr(&self, mode: String) -> Result<String> {
        self.inner.clone().share_with_addr(mode).await.map_err(to_napi_err)
    }
}
