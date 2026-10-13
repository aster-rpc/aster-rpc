//! Document sync client.

use crate::blobs::BlobStatus;
use crate::error::Result;
use crate::id::{AuthorId, Hash, NamespaceId, NamespaceSecret, NodeId, SecretKey};
use std::time::Duration;

use aster_transport_core::{
    CoreAuthorityFingerprint, CoreAuthorityReport, CoreDoc, CoreDocEntry, CoreDocEvent,
    CoreDocEventReceiver, CoreDocsClient, CoreDownloadPolicy, CoreLocalAccess, CoreRetireOutcome,
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

/// What access a namespace grants this node locally.
///
/// Aster's own type on purpose: the document layer has an equivalent, and
/// re-exporting it would put a dependency's enum in Aster's public API, so
/// callers would have to track that crate's versioning to match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAccess {
    /// This node can write to the namespace.
    Write,
    /// This node can read it and nothing more.
    Read,
}

/// The result of retiring local write authority over a namespace.
///
/// `#[must_use]` because `Busy` is an `Ok` value: `retire(..).await?;` would
/// otherwise discard a fail-closed outcome and read as success.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// Retired; this is the access now **stored**, not the access requested.
    Retired(LocalAccess),
    /// Refused: references to the document are still open.
    ///
    /// A live-sync reference and each caller handle both count. Unexpected
    /// handles usually mean the caller does not own what it thought it owned,
    /// so this is normally fail-closed rather than something to retry through.
    Busy {
        /// How many references remain.
        handles: u64,
    },
}

/// A stable, opaque correlation value over `(namespace, stored access)`.
///
/// **Correlation, not evidence.** A namespace id is the namespace public key,
/// and a write capability is the one secret behind it — so `(namespace,
/// access)` already identifies every capability this node can hold for that
/// namespace, and this hash tells a caller nothing that comparing those two
/// fields would not. It exists so a caller holding a grant record can compare
/// one value instead of two, and so a future capability type carrying material
/// that pair does not determine has a versioned slot to grow into.
///
/// The stored-role lookup reads only `(namespace, kind)` and deliberately needs
/// nothing more: that pair already determines the observable authority
/// identity, so no secret material is read, exposed, or required to answer the
/// question. What follows from that is a limit worth stating plainly — the
/// fingerprint cannot detect same-role secret replacement within a namespace,
/// because nothing in the observable pair changes when that happens.
///
/// It does **not** cover secret material — the report never reads the secret
/// back — and it cannot detect authority material changing at the same role,
/// because the current capability model cannot represent that.
///
/// The algorithm is BLAKE3 over a domain separator, the 32-byte namespace id,
/// and one access byte (`Write` = 1, `Read` = 2). The domain string carries the
/// version, so a later definition can never collide with this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthorityFingerprint(CoreAuthorityFingerprint);

impl AuthorityFingerprint {
    /// Fingerprint a namespace and access pair.
    ///
    /// Public so a caller can derive the same value from its own grant record
    /// and compare it against a report without reimplementing the algorithm.
    pub fn of(namespace: NamespaceId, access: LocalAccess) -> Self {
        Self(CoreAuthorityFingerprint::of(
            namespace.to_bytes(),
            access.into(),
        ))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

/// What this node actually holds over a namespace, read back from the store.
///
/// Obtained from [`Docs::authority_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorityReport(CoreAuthorityReport);

impl AuthorityReport {
    /// The namespace this report is about.
    ///
    /// From [`Doc::authority_report`] this is the open document's own
    /// namespace. From [`Docs::authority_report`] it is the namespace the
    /// caller named — the same value it passed in, echoed for convenience, not
    /// independent evidence about any handle the caller holds.
    pub fn namespace(&self) -> NamespaceId {
        NamespaceId::from_bytes(self.0.namespace())
    }

    /// The access **stored** for it — never the access that was requested.
    pub fn access(&self) -> LocalAccess {
        self.0.access().into()
    }

    /// This report's correlation fingerprint. See [`AuthorityFingerprint`] for
    /// what it does and does not prove.
    pub fn fingerprint(&self) -> AuthorityFingerprint {
        AuthorityFingerprint(self.0.fingerprint())
    }
}

impl From<CoreAuthorityReport> for AuthorityReport {
    fn from(report: CoreAuthorityReport) -> Self {
        Self(report)
    }
}

impl From<CoreLocalAccess> for LocalAccess {
    fn from(access: CoreLocalAccess) -> Self {
        match access {
            CoreLocalAccess::Write => LocalAccess::Write,
            CoreLocalAccess::Read => LocalAccess::Read,
        }
    }
}

impl From<LocalAccess> for CoreLocalAccess {
    fn from(access: LocalAccess) -> Self {
        match access {
            LocalAccess::Write => CoreLocalAccess::Write,
            LocalAccess::Read => CoreLocalAccess::Read,
        }
    }
}

impl From<CoreRetireOutcome> for RetireOutcome {
    fn from(outcome: CoreRetireOutcome) -> Self {
        match outcome {
            CoreRetireOutcome::Retired(access) => RetireOutcome::Retired(access.into()),
            CoreRetireOutcome::Busy { handles } => RetireOutcome::Busy { handles },
        }
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

    /// Clone the live Iroh Docs protocol handle behind this facade.
    ///
    /// It shares the replica, blob store and router owned by the Aster node.
    /// Do not call its `shutdown`; [`crate::Node`] owns its lifecycle.
    pub fn native_docs(&self) -> crate::native::iroh_docs::protocol::Docs {
        self.inner.inner.clone()
    }

    /// Clone the Iroh Blobs store shared by Docs and [`crate::Blobs`]. Do not
    /// call the native store's `shutdown`; [`crate::Node`] owns its lifecycle.
    pub fn native_store(&self) -> crate::native::iroh_blobs::api::Store {
        self.inner.store.clone()
    }

    /// Clone the Iroh endpoint used by Docs synchronization.
    pub fn native_endpoint(&self) -> crate::native::iroh::Endpoint {
        self.inner.endpoint.clone()
    }

    /// Create a new document with a fresh random namespace.
    pub async fn create(&self) -> Result<Doc> {
        Ok(Doc::new(self.inner.create().await?, self.inner.clone()))
    }

    /// Create a new random author; returns its id.
    pub async fn create_author(&self) -> Result<AuthorId> {
        Ok(AuthorId::from_hex(self.inner.create_author().await?))
    }

    /// The node-wide default author. Aster sets it to the node's own identity
    /// key, so it **equals the node id** (`AuthorId == NodeId`): a doc entry's
    /// author is exactly the node that wrote it, directly verifiable against the
    /// enrollment credential and QUIC handshake — no separate author↔node binding to
    /// publish. Restart-stable when the node keeps the same secret.
    pub async fn default_author(&self) -> Result<AuthorId> {
        Ok(AuthorId::from_hex(self.inner.default_author().await?))
    }

    /// Reopen a namespace already present in the local replica. Returns `None`
    /// if it isn't present (use an `open_or_import_*` method to bootstrap one).
    pub async fn open(&self, id: NamespaceId) -> Result<Option<Doc>> {
        Ok(self
            .inner
            .open(id.to_hex())
            .await?
            .map(|d| Doc::new(d, self.inner.clone())))
    }

    /// Open-or-import a **writable** namespace from a deterministic 32-byte
    /// secret. Idempotent; upgrades a prior read-only import to write. The
    /// resulting `doc.id()` equals `secret.id()`.
    pub async fn open_or_import_write_namespace(&self, secret: NamespaceSecret) -> Result<Doc> {
        Ok(Doc::new(
            self.inner
                .import_write_namespace(secret.to_bytes().to_vec())
                .await?,
            self.inner.clone(),
        ))
    }

    /// Retire this node's **local write authority** over a namespace, keeping
    /// every entry.
    ///
    /// The only way to narrow access in place. Importing a read capability
    /// cannot do it — capability merge upgrades only — and dropping the
    /// document deletes the entries the node still has to serve.
    ///
    /// Local only: it revokes nothing cryptographically and reaches no other
    /// node. A peer holding the namespace secret still writes.
    ///
    /// **Release every reference first**, which
    /// [`Doc::shutdown_for_role_swap`] does. Until the count reaches zero this
    /// reports [`RetireOutcome::Busy`] rather than proceeding.
    ///
    /// Returns the access now **stored**, so a caller verifies the outcome
    /// rather than trusting its request was applied.
    pub async fn retire_local_write_authority(&self, id: NamespaceId) -> Result<RetireOutcome> {
        Ok(self
            .inner
            .retire_local_write_authority(id.to_bytes().to_vec())
            .await?
            .into())
    }

    /// Report the authority this node holds over the namespace `id` **names**.
    ///
    /// Read back from the local store *after* any import or merge, so the
    /// access is what this node may do rather than what it asked for. Because
    /// [`Self::open_or_import_read_namespace`] never narrows an existing write
    /// capability, a read request over a locally writable namespace correctly
    /// reports [`LocalAccess::Write`].
    ///
    /// **This is keyed by the argument, not by an open document.** The returned
    /// namespace is the one that was asked about, so this cannot on its own
    /// establish that some [`Doc`] a caller holds is that namespace. A caller
    /// gating on a specific open document wants [`Doc::authority_report`],
    /// which takes the namespace from the document itself; using this instead
    /// means independently comparing [`Doc::id`].
    ///
    /// Errors if the namespace is not in the local store. That is deliberate:
    /// a caller uses this to decide whether it may serve a namespace, and
    /// "cannot prove" must not arrive in the same shape as "proved read-only".
    pub async fn authority_report(&self, id: NamespaceId) -> Result<AuthorityReport> {
        Ok(self
            .inner
            .authority_report(id.to_bytes().to_vec())
            .await?
            .into())
    }

    /// Open-or-import a **read-only** namespace from its public id. Idempotent;
    /// never downgrades an existing write capability.
    pub async fn open_or_import_read_namespace(&self, id: NamespaceId) -> Result<Doc> {
        Ok(Doc::new(
            self.inner
                .import_read_namespace(id.to_bytes().to_vec())
                .await?,
            self.inner.clone(),
        ))
    }

    /// Join a document from a ticket string.
    pub async fn join(&self, ticket: impl Into<String>) -> Result<Doc> {
        Ok(Doc::new(
            self.inner.join(ticket.into()).await?,
            self.inner.clone(),
        ))
    }

    /// Join a document from a ticket and subscribe to live events atomically.
    pub async fn join_and_subscribe(&self, ticket: impl Into<String>) -> Result<(Doc, DocEvents)> {
        let (doc, events) = self.inner.join_and_subscribe(ticket.into()).await?;
        Ok((Doc::new(doc, self.inner.clone()), DocEvents::new(events)))
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
        Ok((Doc::new(doc, self.inner.clone()), DocEvents::new(events)))
    }
}

/// A single document.
#[derive(Clone)]
pub struct Doc {
    inner: CoreDoc,
    /// Kept so [`Doc::authority_report`] can answer for *this* document rather
    /// than for a namespace the caller names. Cheap to clone.
    docs: CoreDocsClient,
}

impl Doc {
    #[cfg(feature = "enrollment")]
    pub(crate) fn from_enrollment_core(inner: CoreDoc, docs: CoreDocsClient) -> Self {
        Self::new(inner, docs)
    }
    #[cfg(feature = "enrollment")]
    pub(crate) fn enrollment_core(&self) -> CoreDoc {
        self.inner.clone()
    }
    fn new(inner: CoreDoc, docs: CoreDocsClient) -> Self {
        Self { inner, docs }
    }

    /// Report the authority this node actually holds over **this** document.
    ///
    /// The namespace is taken from the open document, not from an argument, so
    /// the report cannot describe some other namespace: what it answers about
    /// and what the caller holds a handle to are the same thing by
    /// construction.
    ///
    /// The access is read back from the local store after any import or merge.
    /// Because [`Docs::open_or_import_read_namespace`] never narrows an existing
    /// write capability, a document opened read-only over a locally writable
    /// namespace correctly reports [`LocalAccess::Write`].
    pub async fn authority_report(&self) -> Result<AuthorityReport> {
        Ok(self
            .docs
            .authority_report(self.id()?.to_bytes().to_vec())
            .await?
            .into())
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

    /// Write a key/value after committing its blob metadata to disk.
    ///
    /// This is the crash-durable publication boundary for records that must
    /// never outlive their locally-authored value. It costs a blob-store
    /// commit per call; use [`Self::set_bytes`] for retryable high-rate state.
    /// The guarantee covers a hard process kill, not sudden machine power loss.
    pub async fn set_bytes_durable(
        &self,
        author: &AuthorId,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> Result<Hash> {
        Ok(Hash::from_hex(
            self.inner
                .set_bytes_durable(author.as_str().to_string(), key.into(), value.into())
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
    /// Preserves input length/order and duplicates. Missing/partial local blobs
    /// return `None`; complete empty content returns `Some(Vec::new())`. Empty
    /// input returns an empty list. All hashes are validated before store reads.
    /// No fetch, tags or retention mutation occurs.
    ///
    /// This is not a global snapshot: a later GC/availability change can make
    /// a large validated export fail. Storage/export/integrity errors fail the
    /// entire call and are never disguised as `None`. Use blob custody for
    /// stable retention during reads and host verification.
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

    /// Require successful metadata synchronization with every selected replica.
    /// Content reads must additionally reject pending or unavailable bytes.
    pub async fn sync_with_timeout(&self, peers: &[NodeId], budget: Duration) -> Result<()> {
        Ok(self
            .inner
            .sync_with_timeout(
                peers.iter().map(|peer| peer.as_str().to_string()).collect(),
                budget,
            )
            .await?)
    }

    /// Stop syncing this document.
    pub async fn leave(&self) -> Result<()> {
        Ok(self.inner.leave().await?)
    }

    /// Release **both** of this document's references, so its namespace can
    /// change role.
    ///
    /// Consuming, because it invalidates this handle and every clone of it —
    /// returning `self` would hand back something permanently unusable, and a
    /// caller still holding a clone will find that dead too. That is the point:
    /// a role swap must not leave a live reference behind.
    ///
    /// Releases the live-sync reference and this handle's own, under **one
    /// absolute deadline** covering both. A budget per step would silently
    /// double the transition budget a caller allowed for.
    ///
    /// Close is attempted even when leaving fails — a failed leave is exactly
    /// when the handle most needs releasing. If both fail, both are reported.
    pub async fn shutdown_for_role_swap(self, budget: Duration) -> Result<()> {
        Ok(self.inner.shutdown_for_role_swap(budget).await?)
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
