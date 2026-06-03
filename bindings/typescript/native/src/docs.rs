//! Docs module — wraps CoreDocsClient and CoreDoc.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::{CoreDoc, CoreDocEvent, CoreDocEventReceiver, CoreDocsClient};

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

    /// Reopen an existing local namespace by public namespace id bytes.
    /// Returns null if the namespace is not present locally.
    #[napi]
    pub async fn open_namespace(&self, namespace_id: Buffer) -> Result<Option<DocHandle>> {
        if namespace_id.len() != 32 {
            return Err(Error::from_reason("namespace id must be 32 bytes"));
        }
        let namespace_id_hex = hex::encode(namespace_id);
        let doc = self
            .inner
            .clone()
            .open(namespace_id_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(doc.map(|inner| DocHandle { inner }))
    }

    /// Reopen an existing local namespace by public namespace id hex.
    /// Returns null if the namespace is not present locally.
    #[napi]
    pub async fn open_namespace_hex(&self, namespace_id_hex: String) -> Result<Option<DocHandle>> {
        let doc = self
            .inner
            .clone()
            .open(namespace_id_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(doc.map(|inner| DocHandle { inner }))
    }

    /// Open-or-import a read-only namespace by public namespace id bytes.
    #[napi]
    pub async fn open_or_import_read_namespace(&self, namespace_id: Buffer) -> Result<DocHandle> {
        let doc = self
            .inner
            .clone()
            .import_read_namespace(namespace_id.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(DocHandle { inner: doc })
    }

    /// Open-or-import a writable namespace by namespace secret bytes.
    #[napi]
    pub async fn open_or_import_write_namespace(
        &self,
        namespace_secret: Buffer,
    ) -> Result<DocHandle> {
        let doc = self
            .inner
            .clone()
            .import_write_namespace(namespace_secret.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(DocHandle { inner: doc })
    }

    /// Create a new author, returning the author ID (hex).
    #[napi]
    pub async fn create_author(&self) -> Result<String> {
        self.inner
            .clone()
            .create_author()
            .await
            .map_err(to_napi_err)
    }

    /// Join a document and subscribe to events.
    /// Returns [docHandle, eventReceiver].
    #[napi]
    pub async fn join_and_subscribe(&self, ticket: String) -> Result<DocWithEvents> {
        let (doc, receiver) = self
            .inner
            .clone()
            .join_and_subscribe(ticket)
            .await
            .map_err(to_napi_err)?;
        Ok(DocWithEvents {
            doc: Some(DocHandle { inner: doc }),
            events: Some(DocEventReceiver { inner: receiver }),
        })
    }

    /// Join a document by namespace id (hex) and peer endpoint id (hex),
    /// subscribing to events. Mirrors Python's
    /// ``docs_client.join_and_subscribe_namespace`` so dynamic proxy
    /// / consumer code can pull the producer's registry doc from just
    /// the bits returned in the admission response.
    #[napi]
    pub async fn join_and_subscribe_namespace(
        &self,
        namespace_id_hex: String,
        peer_node_id_hex: String,
    ) -> Result<DocWithEvents> {
        let (doc, receiver) = self
            .inner
            .clone()
            .join_and_subscribe_namespace(namespace_id_hex, peer_node_id_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(DocWithEvents {
            doc: Some(DocHandle { inner: doc }),
            events: Some(DocEventReceiver { inner: receiver }),
        })
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
    pub async fn set_bytes(
        &self,
        author_hex: String,
        key: String,
        value: Buffer,
    ) -> Result<String> {
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

    /// Query entries by exact key. Entries are ordered by author id, not timestamp, so
    /// `limit` truncates by author and may drop the latest writer — use it only to
    /// enumerate authors. To read a key's current value, use `queryLatestExact`. Omit
    /// `limit` to read every author's entry.
    #[napi]
    pub async fn query_key_exact(&self, key: String, limit: Option<i64>) -> Result<Vec<String>> {
        let entries = self
            .inner
            .clone()
            .query_key_exact(key.into_bytes(), limit.map(|n| n as u64))
            .await
            .map_err(to_napi_err)?;
        Ok(entries
            .into_iter()
            .map(|e| format!("{}:{}", e.author_id, e.content_hash))
            .collect())
    }

    /// Read the single latest entry for an exact key, across all authors. Returns
    /// `"<author>:<content_hash>"`, or null if the key was never written or its latest
    /// write was a deletion. Unaffected by how many authors have written the key.
    #[napi]
    pub async fn query_latest_exact(&self, key: String) -> Result<Option<String>> {
        let entry = self
            .inner
            .clone()
            .query_latest_exact(key.into_bytes())
            .await
            .map_err(to_napi_err)?;
        Ok(entry.map(|e| format!("{}:{}", e.author_id, e.content_hash)))
    }

    /// Query entries by key prefix. `limit` caps the result set; omit it for an
    /// unbounded listing.
    #[napi]
    pub async fn query_key_prefix(
        &self,
        prefix: String,
        limit: Option<i64>,
    ) -> Result<Vec<String>> {
        let entries = self
            .inner
            .clone()
            .query_key_prefix(prefix.into_bytes(), limit.map(|n| n as u64))
            .await
            .map_err(to_napi_err)?;
        Ok(entries
            .into_iter()
            .map(|e| format!("{}:{}", e.author_id, e.content_hash))
            .collect())
    }

    /// List the latest entry for each distinct key under a prefix, across all authors.
    /// Collapses each key to its highest-timestamp entry (omitting keys whose latest
    /// write was a deletion), so a key written by multiple authors appears once — use
    /// this for directory-style listings. `limit` caps the number of distinct keys; omit
    /// it for an unbounded listing.
    #[napi]
    pub async fn query_latest_prefix(
        &self,
        prefix: String,
        limit: Option<i64>,
    ) -> Result<Vec<String>> {
        let entries = self
            .inner
            .clone()
            .query_latest_prefix(prefix.into_bytes(), limit.map(|n| n as u64))
            .await
            .map_err(to_napi_err)?;
        Ok(entries
            .into_iter()
            .map(|e| format!("{}:{}", e.author_id, e.content_hash))
            .collect())
    }

    /// Read entry content by content hash.
    #[napi]
    pub async fn read_entry_content(&self, content_hash_hex: String) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .read_entry_content(content_hash_hex)
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Start sync with peers.
    #[napi]
    pub async fn start_sync(&self, peers: Vec<String>) -> Result<()> {
        self.inner
            .clone()
            .start_sync(peers)
            .await
            .map_err(to_napi_err)
    }

    /// Leave the document (stop syncing).
    #[napi]
    pub async fn leave(&self) -> Result<()> {
        self.inner.clone().leave().await.map_err(to_napi_err)
    }

    /// Share with full relay+address info.
    #[napi]
    pub async fn share_with_addr(&self, mode: String) -> Result<String> {
        self.inner
            .clone()
            .share_with_addr(mode)
            .await
            .map_err(to_napi_err)
    }

    /// Subscribe to document events.
    #[napi]
    pub async fn subscribe(&self) -> Result<DocEventReceiver> {
        let receiver = self.inner.clone().subscribe().await.map_err(to_napi_err)?;
        Ok(DocEventReceiver { inner: receiver })
    }

    /// Set the download policy for this document.
    /// Policy: "everything", "nothing_except:<prefix1>,<prefix2>", "everything_except:<prefix1>,<prefix2>"
    #[napi]
    pub async fn set_download_policy(&self, policy: String) -> Result<()> {
        let core_policy = parse_download_policy(&policy)?;
        self.inner
            .clone()
            .set_download_policy(core_policy)
            .await
            .map_err(to_napi_err)
    }

    /// Get the download policy for this document.
    #[napi]
    pub async fn get_download_policy(&self) -> Result<String> {
        let policy = self
            .inner
            .clone()
            .get_download_policy()
            .await
            .map_err(to_napi_err)?;
        Ok(format_download_policy(&policy))
    }
}

fn parse_download_policy(s: &str) -> Result<aster_transport_core::CoreDownloadPolicy> {
    use aster_transport_core::CoreDownloadPolicy;
    if s == "everything" {
        Ok(CoreDownloadPolicy::Everything)
    } else if let Some(rest) = s.strip_prefix("nothing_except:") {
        let prefixes = rest
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| p.as_bytes().to_vec())
            .collect();
        Ok(CoreDownloadPolicy::NothingExcept { prefixes })
    } else if let Some(rest) = s.strip_prefix("everything_except:") {
        let prefixes = rest
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| p.as_bytes().to_vec())
            .collect();
        Ok(CoreDownloadPolicy::EverythingExcept { prefixes })
    } else {
        Err(napi::Error::from_reason(format!(
            "invalid download policy: {s}"
        )))
    }
}

fn format_download_policy(policy: &aster_transport_core::CoreDownloadPolicy) -> String {
    use aster_transport_core::CoreDownloadPolicy;
    match policy {
        CoreDownloadPolicy::Everything => "everything".to_string(),
        CoreDownloadPolicy::NothingExcept { prefixes } => {
            let ps: Vec<String> = prefixes
                .iter()
                .map(|p| String::from_utf8_lossy(p).to_string())
                .collect();
            format!("nothing_except:{}", ps.join(","))
        }
        CoreDownloadPolicy::EverythingExcept { prefixes } => {
            let ps: Vec<String> = prefixes
                .iter()
                .map(|p| String::from_utf8_lossy(p).to_string())
                .collect();
            format!("everything_except:{}", ps.join(","))
        }
    }
}

// ============================================================================
// DocEventReceiver
// ============================================================================

#[napi]
pub struct DocEventReceiver {
    inner: CoreDocEventReceiver,
}

/// A doc event returned from subscribe().
#[napi(object)]
pub struct DocEvent {
    /// Event kind: "insert_local", "insert_remote", "content_ready",
    /// "pending_content_ready", "neighbor_up", "neighbor_down", "sync_finished"
    pub kind: String,
    /// Author ID (for insert events).
    pub author: Option<String>,
    /// Content hash (for insert/content_ready events).
    pub content_hash: Option<String>,
    /// Key (for insert events).
    pub key: Option<Vec<u8>>,
    /// Peer ID (for neighbor/sync events, or insert_remote sender).
    pub peer: Option<String>,
}

fn core_event_to_js(event: CoreDocEvent) -> DocEvent {
    match event {
        CoreDocEvent::InsertLocal { entry } => DocEvent {
            kind: "insert_local".to_string(),
            author: Some(entry.author_id),
            content_hash: Some(entry.content_hash),
            key: Some(entry.key),
            peer: None,
        },
        CoreDocEvent::InsertRemote { from, entry } => DocEvent {
            kind: "insert_remote".to_string(),
            author: Some(entry.author_id),
            content_hash: Some(entry.content_hash),
            key: Some(entry.key),
            peer: Some(from),
        },
        CoreDocEvent::ContentReady { hash } => DocEvent {
            kind: "content_ready".to_string(),
            author: None,
            content_hash: Some(hash),
            key: None,
            peer: None,
        },
        CoreDocEvent::PendingContentReady => DocEvent {
            kind: "pending_content_ready".to_string(),
            author: None,
            content_hash: None,
            key: None,
            peer: None,
        },
        CoreDocEvent::NeighborUp { peer } => DocEvent {
            kind: "neighbor_up".to_string(),
            author: None,
            content_hash: None,
            key: None,
            peer: Some(peer),
        },
        CoreDocEvent::NeighborDown { peer } => DocEvent {
            kind: "neighbor_down".to_string(),
            author: None,
            content_hash: None,
            key: None,
            peer: Some(peer),
        },
        CoreDocEvent::SyncFinished { peer } => DocEvent {
            kind: "sync_finished".to_string(),
            author: None,
            content_hash: None,
            key: None,
            peer: Some(peer),
        },
    }
}

#[napi]
impl DocEventReceiver {
    /// Receive the next doc event. Returns null when subscription ends.
    #[napi]
    pub async fn recv(&self) -> Result<Option<DocEvent>> {
        match self.inner.clone().recv().await.map_err(to_napi_err)? {
            Some(event) => Ok(Some(core_event_to_js(event))),
            None => Ok(None),
        }
    }
}

// ============================================================================
// DocWithEvents — returned by join_and_subscribe
// ============================================================================

#[napi]
pub struct DocWithEvents {
    doc: Option<DocHandle>,
    events: Option<DocEventReceiver>,
}

#[napi]
impl DocWithEvents {
    /// Take the doc handle (can only be called once).
    #[napi]
    pub fn take_doc(&mut self) -> Result<DocHandle> {
        self.doc
            .take()
            .ok_or_else(|| napi::Error::from_reason("doc already taken".to_string()))
    }

    /// Take the event receiver (can only be called once).
    #[napi]
    pub fn take_events(&mut self) -> Result<DocEventReceiver> {
        self.events
            .take()
            .ok_or_else(|| napi::Error::from_reason("events already taken".to_string()))
    }
}
