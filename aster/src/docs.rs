//! Document sync client.

use crate::blobs::BlobStatus;
use crate::error::Result;
use crate::id::{AuthorId, Hash, NamespaceId, NamespaceSecret, NodeId, SecretKey};
use aster_transport_core::{
    CoreDoc, CoreDocEntry, CoreDocEvent, CoreDocEventReceiver, CoreDocsClient, CoreDownloadPolicy,
};

/// The default-docs [`AuthorId`] for a node with this secret — the same value
/// [`Docs::default_author`] returns after the node starts. Aster uses the node
/// identity key as the docs author, so this **equals the node id**; provided as a
/// typed convenience for computing it offline.
pub fn default_author_id(secret: &SecretKey) -> AuthorId {
    AuthorId::from_hex(aster_transport_core::default_docs_author_id(
        &secret.to_bytes(),
    ))
}

/// Read or write share capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareMode {
    /// Recipients can read.
    Read,
    /// Recipients can read and write.
    Write,
}

impl ShareMode {
    fn as_core(self) -> String {
        match self {
            ShareMode::Read => "read".into(),
            ShareMode::Write => "write".into(),
        }
    }
}

/// Which remote entries are automatically downloaded.
#[derive(Clone, Debug)]
pub enum DownloadPolicy {
    /// Download everything (the default).
    Everything,
    /// Download nothing except entries whose keys match one of these prefixes.
    NothingExcept(Vec<Vec<u8>>),
    /// Download everything except entries whose keys match one of these prefixes.
    EverythingExcept(Vec<Vec<u8>>),
}

impl From<DownloadPolicy> for CoreDownloadPolicy {
    fn from(p: DownloadPolicy) -> Self {
        match p {
            DownloadPolicy::Everything => CoreDownloadPolicy::Everything,
            DownloadPolicy::NothingExcept(prefixes) => {
                CoreDownloadPolicy::NothingExcept { prefixes }
            }
            DownloadPolicy::EverythingExcept(prefixes) => {
                CoreDownloadPolicy::EverythingExcept { prefixes }
            }
        }
    }
}

impl From<CoreDownloadPolicy> for DownloadPolicy {
    fn from(p: CoreDownloadPolicy) -> Self {
        match p {
            CoreDownloadPolicy::Everything => DownloadPolicy::Everything,
            CoreDownloadPolicy::NothingExcept { prefixes } => {
                DownloadPolicy::NothingExcept(prefixes)
            }
            CoreDownloadPolicy::EverythingExcept { prefixes } => {
                DownloadPolicy::EverythingExcept(prefixes)
            }
        }
    }
}

/// Metadata for a single document entry.
#[derive(Clone, Debug)]
pub struct DocEntry {
    /// Who wrote the entry.
    pub author: AuthorId,
    /// The entry key.
    pub key: Vec<u8>,
    /// The content hash of the value.
    pub content_hash: Hash,
    /// The value length in bytes.
    pub content_len: u64,
    /// Write timestamp (microseconds since epoch).
    pub timestamp: u64,
}

impl From<CoreDocEntry> for DocEntry {
    fn from(e: CoreDocEntry) -> Self {
        DocEntry {
            author: AuthorId::from_hex(e.author_id),
            key: e.key,
            content_hash: Hash::from_hex(e.content_hash),
            content_len: e.content_len,
            timestamp: e.timestamp,
        }
    }
}

/// A live document event.
#[derive(Clone, Debug)]
pub enum DocEvent {
    /// This node wrote an entry.
    InsertLocal { entry: DocEntry },
    /// A peer sent us an entry.
    InsertRemote { from: NodeId, entry: DocEntry },
    /// Content for an entry is now available locally.
    ContentReady { hash: Hash },
    /// All pending content downloads from the last sync finished.
    PendingContentReady,
    /// A peer joined this document's swarm.
    NeighborUp { peer: NodeId },
    /// A peer left this document's swarm.
    NeighborDown { peer: NodeId },
    /// A sync with a peer finished.
    SyncFinished { peer: NodeId },
}

impl From<CoreDocEvent> for DocEvent {
    fn from(e: CoreDocEvent) -> Self {
        match e {
            CoreDocEvent::InsertLocal { entry } => DocEvent::InsertLocal {
                entry: entry.into(),
            },
            CoreDocEvent::InsertRemote { from, entry } => DocEvent::InsertRemote {
                from: NodeId::from_hex(from),
                entry: entry.into(),
            },
            CoreDocEvent::ContentReady { hash } => DocEvent::ContentReady {
                hash: Hash::from_hex(hash),
            },
            CoreDocEvent::PendingContentReady => DocEvent::PendingContentReady,
            CoreDocEvent::NeighborUp { peer } => DocEvent::NeighborUp {
                peer: NodeId::from_hex(peer),
            },
            CoreDocEvent::NeighborDown { peer } => DocEvent::NeighborDown {
                peer: NodeId::from_hex(peer),
            },
            CoreDocEvent::SyncFinished { peer } => DocEvent::SyncFinished {
                peer: NodeId::from_hex(peer),
            },
        }
    }
}

/// A live event subscription. Call [`recv`](DocEvents::recv) in a loop.
#[derive(Clone)]
pub struct DocEvents {
    inner: CoreDocEventReceiver,
}

impl DocEvents {
    fn new(inner: CoreDocEventReceiver) -> Self {
        Self { inner }
    }

    /// Receive the next event, or `None` when the subscription ends.
    pub async fn recv(&self) -> Result<Option<DocEvent>> {
        Ok(self.inner.recv().await?.map(DocEvent::from))
    }
}

/// A handle to a node's document store. Cheap to clone.
#[derive(Clone)]
pub struct Docs {
    inner: CoreDocsClient,
}

impl Docs {
    pub(crate) fn new(inner: CoreDocsClient) -> Self {
        Self { inner }
    }

    /// Create a new document with a fresh random namespace.
    pub async fn create(&self) -> Result<Doc> {
        Ok(Doc::new(self.inner.create().await?))
    }

    /// Create a new random author; returns its id.
    pub async fn create_author(&self) -> Result<AuthorId> {
        Ok(AuthorId::from_hex(self.inner.create_author().await?))
    }

    /// The node-wide default author. Aster sets it to the node's own identity
    /// key, so it **equals the node id** (`AuthorId == NodeId`): a doc entry's
    /// author is exactly the node that wrote it, directly verifiable against the
    /// attestation chain and QUIC handshake — no separate author↔node binding to
    /// publish. Restart-stable when the node keeps the same secret.
    pub async fn default_author(&self) -> Result<AuthorId> {
        Ok(AuthorId::from_hex(self.inner.default_author().await?))
    }

    /// Reopen a namespace already present in the local replica. Returns `None`
    /// if it isn't present (use an `open_or_import_*` method to bootstrap one).
    pub async fn open(&self, id: NamespaceId) -> Result<Option<Doc>> {
        Ok(self.inner.open(id.to_hex()).await?.map(Doc::new))
    }

    /// Open-or-import a **writable** namespace from a deterministic 32-byte
    /// secret. Idempotent; upgrades a prior read-only import to write. The
    /// resulting `doc.id()` equals `secret.id()`.
    pub async fn open_or_import_write_namespace(&self, secret: NamespaceSecret) -> Result<Doc> {
        Ok(Doc::new(
            self.inner
                .import_write_namespace(secret.to_bytes().to_vec())
                .await?,
        ))
    }

    /// Open-or-import a **read-only** namespace from its public id. Idempotent;
    /// never downgrades an existing write capability.
    pub async fn open_or_import_read_namespace(&self, id: NamespaceId) -> Result<Doc> {
        Ok(Doc::new(
            self.inner
                .import_read_namespace(id.to_bytes().to_vec())
                .await?,
        ))
    }

    /// Join a document from a ticket string.
    pub async fn join(&self, ticket: impl Into<String>) -> Result<Doc> {
        Ok(Doc::new(self.inner.join(ticket.into()).await?))
    }

    /// Join a document from a ticket and subscribe to live events atomically.
    pub async fn join_and_subscribe(&self, ticket: impl Into<String>) -> Result<(Doc, DocEvents)> {
        let (doc, events) = self.inner.join_and_subscribe(ticket.into()).await?;
        Ok((Doc::new(doc), DocEvents::new(events)))
    }

    /// Join a document by namespace id, syncing from `peer`, and subscribe.
    pub async fn join_and_subscribe_namespace(
        &self,
        id: NamespaceId,
        peer: &NodeId,
    ) -> Result<(Doc, DocEvents)> {
        let (doc, events) = self
            .inner
            .join_and_subscribe_namespace(id.to_hex(), peer.as_str().to_string())
            .await?;
        Ok((Doc::new(doc), DocEvents::new(events)))
    }
}

/// A single document.
#[derive(Clone)]
pub struct Doc {
    inner: CoreDoc,
}

impl Doc {
    fn new(inner: CoreDoc) -> Self {
        Self { inner }
    }

    /// This document's namespace id.
    pub fn id(&self) -> Result<NamespaceId> {
        NamespaceId::from_hex(&self.inner.doc_id())
    }

    /// Write a key/value as `author`; returns the content hash.
    pub async fn set_bytes(
        &self,
        author: &AuthorId,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> Result<Hash> {
        Ok(Hash::from_hex(
            self.inner
                .set_bytes(author.as_str().to_string(), key.into(), value.into())
                .await?,
        ))
    }

    /// Delete all entries for `author` whose key starts with `prefix`.
    ///
    /// This writes iroh-docs' deletion marker at `prefix` and returns the
    /// number of removed entries.
    pub async fn del(&self, author: &AuthorId, prefix: impl Into<Vec<u8>>) -> Result<u64> {
        Ok(self
            .inner
            .del(author.as_str().to_string(), prefix.into())
            .await?)
    }

    /// Read the value for an exact `(author, key)`, if present.
    pub async fn get_exact(
        &self,
        author: &AuthorId,
        key: impl Into<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .inner
            .get_exact(author.as_str().to_string(), key.into())
            .await?)
    }

    /// Whether an entry exists for an exact `(author, key)`, INCLUDING a deletion tombstone left
    /// by [`del`](Self::del). [`get_exact`](Self::get_exact) reports a tombstone as absent; this
    /// reports it as present (and fetches no content). For insert-if-absent callers that must
    /// treat a revoked key as permanently taken.
    pub async fn entry_exists(&self, author: &AuthorId, key: impl Into<Vec<u8>>) -> Result<bool> {
        Ok(self
            .inner
            .entry_exists(author.as_str().to_string(), key.into())
            .await?)
    }

    /// All entries for an exact key, across authors (`limit` optional).
    pub async fn query_key_exact(
        &self,
        key: impl Into<Vec<u8>>,
        limit: Option<u64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_key_exact(key.into(), limit)
            .await?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// The single latest entry for an exact key (most-recent writer wins).
    pub async fn query_latest_exact(&self, key: impl Into<Vec<u8>>) -> Result<Option<DocEntry>> {
        Ok(self
            .inner
            .query_latest_exact(key.into())
            .await?
            .map(DocEntry::from))
    }

    /// All entries under a key prefix, across authors (`limit` optional).
    pub async fn query_key_prefix(
        &self,
        prefix: impl Into<Vec<u8>>,
        limit: Option<u64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_key_prefix(prefix.into(), limit)
            .await?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// The latest entry for each distinct key under a prefix (`limit` optional).
    pub async fn query_latest_prefix(
        &self,
        prefix: impl Into<Vec<u8>>,
        limit: Option<u64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_latest_prefix(prefix.into(), limit)
            .await?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// Read content bytes for a content hash (e.g. from a queried entry).
    pub async fn read_entry_content(&self, content_hash: &Hash) -> Result<Vec<u8>> {
        Ok(self
            .inner
            .read_entry_content(content_hash.as_str().to_string())
            .await?)
    }

    /// Read multiple content blobs that are already complete locally.
    ///
    /// Missing or partial values return `None` at their original index.
    pub async fn read_entry_contents_if_complete(
        &self,
        content_hashes: &[Hash],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        Ok(self
            .inner
            .read_entry_contents_if_complete(
                content_hashes
                    .iter()
                    .map(|hash| hash.as_str().to_string())
                    .collect(),
            )
            .await?)
    }

    /// Return the local storage status for a document entry value blob.
    pub async fn entry_content_status(&self, content_hash: &Hash) -> Result<BlobStatus> {
        Ok(self
            .inner
            .entry_content_status(content_hash.as_str().to_string())
            .await?
            .into())
    }

    /// Share this document as a ticket (id-only addressing).
    pub async fn share(&self, mode: ShareMode) -> Result<String> {
        Ok(self.inner.share(mode.as_core()).await?)
    }

    /// Share this document as a ticket including relay + direct addresses.
    pub async fn share_with_addr(&self, mode: ShareMode) -> Result<String> {
        Ok(self.inner.share_with_addr(mode.as_core()).await?)
    }

    /// Start syncing with the given peers.
    pub async fn start_sync(&self, peers: &[NodeId]) -> Result<()> {
        let peers = peers.iter().map(|p| p.as_str().to_string()).collect();
        Ok(self.inner.start_sync(peers).await?)
    }

    /// Stop syncing this document.
    pub async fn leave(&self) -> Result<()> {
        Ok(self.inner.leave().await?)
    }

    /// Subscribe to live events for this document.
    pub async fn subscribe(&self) -> Result<DocEvents> {
        Ok(DocEvents::new(self.inner.subscribe().await?))
    }

    /// Set the download policy.
    pub async fn set_download_policy(&self, policy: DownloadPolicy) -> Result<()> {
        Ok(self.inner.set_download_policy(policy.into()).await?)
    }

    /// Get the current download policy.
    pub async fn get_download_policy(&self) -> Result<DownloadPolicy> {
        Ok(self.inner.get_download_policy().await?.into())
    }
}
