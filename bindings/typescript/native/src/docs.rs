//! Docs — wraps the `aster` crate's [`aster::Docs`] / [`aster::Doc`] (plan
//! D1, T6).
//!
//! Keys and values are bytes: keys accept a `string` (UTF-8) or a
//! `Uint8Array`. Entries, events, policies and authority reports are typed
//! objects; event subscriptions are `AsyncIterable`s.

use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::{AuthorId, Hash, NamespaceId, NamespaceSecret, NodeId};

use crate::blobs::{blob_status_out, BlobStatusInfo};
use crate::config::aster_err;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

pub(crate) fn bytes_in(v: Either<String, Uint8Array>) -> Vec<u8> {
    match v {
        Either::A(s) => s.into_bytes(),
        Either::B(b) => b.to_vec(),
    }
}

fn safe(n: u64, what: &str) -> Result<f64> {
    if n > MAX_SAFE_INTEGER {
        return Err(Error::from_reason(format!(
            "{what} exceeds JavaScript safe integer range"
        )));
    }
    Ok(n as f64)
}

fn limit_in(limit: Option<i64>) -> Result<Option<u64>> {
    match limit {
        None => Ok(None),
        Some(n) if n < 0 => Err(invalid("limit must be non-negative")),
        Some(n) => Ok(Some(n as u64)),
    }
}

/// A 32-byte value given as 64 hex characters or raw bytes.
fn bytes32(what: &str, v: Either<String, Uint8Array>) -> Result<[u8; 32]> {
    let raw = match v {
        Either::A(s) => hex::decode(s.trim())
            .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))?,
        Either::B(b) => b.to_vec(),
    };
    raw.try_into()
        .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))
}

fn namespace_in(v: Either<String, Uint8Array>) -> Result<NamespaceId> {
    Ok(NamespaceId::from_bytes(bytes32("namespace id", v)?))
}

fn author(s: String) -> AuthorId {
    AuthorId::from_hex(s)
}

fn hash(s: String) -> Hash {
    Hash::from_hex(s)
}

// ── Value types ─────────────────────────────────────────────────────────────

/// One document entry (metadata; read the value with `readEntryContent`).
#[napi(object)]
pub struct DocEntry {
    /// Who wrote the entry (author id hex).
    pub author: String,
    pub key: Buffer,
    /// Content hash of the value (hex).
    pub content_hash: String,
    /// Value length in bytes.
    pub content_len: f64,
    /// Write timestamp, microseconds since the Unix epoch.
    pub timestamp: f64,
}

impl From<aster::DocEntry> for DocEntry {
    fn from(e: aster::DocEntry) -> Self {
        Self {
            author: e.author.into_string(),
            key: Buffer::from(e.key),
            content_hash: e.content_hash.into_string(),
            content_len: e.content_len as f64,
            timestamp: e.timestamp as f64,
        }
    }
}

/// Which remote entries are downloaded automatically.
#[napi(object)]
pub struct DownloadPolicy {
    /// `'everything'` (default), `'nothing_except'` (only `prefixes`) or
    /// `'everything_except'` (all but `prefixes`).
    #[napi(ts_type = "'everything' | 'nothing_except' | 'everything_except'")]
    pub mode: String,
    /// Key prefixes for the `*_except` modes.
    #[napi(ts_type = "Array<string | Uint8Array>")]
    pub prefixes: Option<Vec<Either<String, Uint8Array>>>,
}

impl DownloadPolicy {
    fn into_rust(self) -> Result<aster::DownloadPolicy> {
        let prefixes = || -> Vec<Vec<u8>> {
            self.prefixes
                .unwrap_or_default()
                .into_iter()
                .map(bytes_in)
                .collect()
        };
        match self.mode.as_str() {
            "everything" => Ok(aster::DownloadPolicy::Everything),
            "nothing_except" => Ok(aster::DownloadPolicy::NothingExcept(prefixes())),
            "everything_except" => Ok(aster::DownloadPolicy::EverythingExcept(prefixes())),
            other => Err(invalid(format!(
                "unknown download policy mode {other:?} (expected 'everything', \
                 'nothing_except' or 'everything_except')"
            ))),
        }
    }

    fn from_rust(p: aster::DownloadPolicy) -> Self {
        let out = |mode: &str, prefixes: Vec<Vec<u8>>| DownloadPolicy {
            mode: mode.into(),
            prefixes: Some(
                prefixes
                    .into_iter()
                    .map(|p| Either::B(Uint8Array::from(p)))
                    .collect(),
            ),
        };
        match p {
            aster::DownloadPolicy::Everything => out("everything", Vec::new()),
            aster::DownloadPolicy::NothingExcept(p) => out("nothing_except", p),
            aster::DownloadPolicy::EverythingExcept(p) => out("everything_except", p),
        }
    }
}

fn access_out(a: aster::LocalAccess) -> String {
    match a {
        aster::LocalAccess::Write => "write".into(),
        aster::LocalAccess::Read => "read".into(),
    }
}

fn access_in(a: &str) -> Result<aster::LocalAccess> {
    match a {
        "write" => Ok(aster::LocalAccess::Write),
        "read" => Ok(aster::LocalAccess::Read),
        other => Err(invalid(format!(
            "unknown local access {other:?} (expected 'read' or 'write')"
        ))),
    }
}

fn share_mode_in(mode: &str) -> Result<aster::ShareMode> {
    match mode {
        "read" => Ok(aster::ShareMode::Read),
        "write" => Ok(aster::ShareMode::Write),
        other => Err(invalid(format!(
            "unknown share mode {other:?} (expected 'read' or 'write')"
        ))),
    }
}

/// What this node actually holds over a namespace, read back from the store.
#[napi(object)]
pub struct AuthorityReport {
    /// Namespace id (hex).
    pub namespace: String,
    /// The access **stored** — never the access requested.
    #[napi(ts_type = "'read' | 'write'")]
    pub access: String,
    /// Correlation fingerprint over `(namespace, access)` (64 hex). See
    /// `authorityFingerprint`; it is correlation, not evidence.
    pub fingerprint: String,
}

impl From<aster::AuthorityReport> for AuthorityReport {
    fn from(r: aster::AuthorityReport) -> Self {
        Self {
            namespace: r.namespace().to_hex(),
            access: access_out(r.access()),
            fingerprint: r.fingerprint().to_hex(),
        }
    }
}

/// The result of `retireLocalWriteAuthority`.
#[napi(object)]
pub struct RetireOutcome {
    /// `'retired'`, or `'busy'` when references to the document are still
    /// open (fail closed: release them, e.g. `shutdownForRoleSwap`, first).
    #[napi(ts_type = "'retired' | 'busy'")]
    pub outcome: String,
    /// For `'retired'`: the access now stored.
    #[napi(ts_type = "'read' | 'write'")]
    pub access: Option<String>,
    /// For `'busy'`: how many references remain.
    pub handles: Option<f64>,
}

impl From<aster::RetireOutcome> for RetireOutcome {
    fn from(o: aster::RetireOutcome) -> Self {
        match o {
            aster::RetireOutcome::Retired(a) => Self {
                outcome: "retired".into(),
                access: Some(access_out(a)),
                handles: None,
            },
            aster::RetireOutcome::Busy { handles } => Self {
                outcome: "busy".into(),
                access: None,
                handles: Some(handles as f64),
            },
        }
    }
}

/// The correlation fingerprint of a `(namespace, access)` pair — the value an
/// `AuthorityReport` carries. BLAKE3 over a versioned domain separator, the
/// namespace id and one access byte; correlation, not evidence.
#[napi]
pub fn authority_fingerprint(
    namespace: Either<String, Uint8Array>,
    #[napi(ts_arg_type = "'read' | 'write'")] access: String,
) -> Result<String> {
    Ok(aster::AuthorityFingerprint::of(namespace_in(namespace)?, access_in(&access)?).to_hex())
}

/// The docs default-author id for a node secret key — equal to the node id.
#[napi]
pub fn default_author_id(secret_key: Either<String, Uint8Array>) -> Result<String> {
    let key = aster::SecretKey::from_bytes(bytes32("secret key", secret_key)?);
    Ok(aster::default_author_id(&key).into_string())
}

// ── DocsClient ──────────────────────────────────────────────────────────────

#[napi]
pub struct DocsClient {
    pub(crate) inner: aster::Docs,
}

impl From<aster::Docs> for DocsClient {
    fn from(inner: aster::Docs) -> Self {
        Self { inner }
    }
}

#[napi]
impl DocsClient {
    /// Create a new document with a fresh random namespace.
    #[napi]
    pub async fn create(&self) -> Result<DocHandle> {
        Ok(self.inner.create().await.map_err(aster_err)?.into())
    }

    /// Create a new random author; returns its id (hex).
    #[napi]
    pub async fn create_author(&self) -> Result<String> {
        Ok(self
            .inner
            .create_author()
            .await
            .map_err(aster_err)?
            .into_string())
    }

    /// The node-wide default author — equal to the node id.
    #[napi]
    pub async fn default_author(&self) -> Result<String> {
        Ok(self
            .inner
            .default_author()
            .await
            .map_err(aster_err)?
            .into_string())
    }

    /// Join a document from a ticket string.
    #[napi]
    pub async fn join(&self, ticket: String) -> Result<DocHandle> {
        Ok(self.inner.join(ticket).await.map_err(aster_err)?.into())
    }

    /// Reopen a namespace already in the local replica (`null` if absent).
    #[napi]
    pub async fn open(
        &self,
        namespace_id: Either<String, Uint8Array>,
    ) -> Result<Option<DocHandle>> {
        let id = namespace_in(namespace_id)?;
        Ok(self
            .inner
            .open(id)
            .await
            .map_err(aster_err)?
            .map(DocHandle::from))
    }

    /// Open-or-import a **read-only** namespace from its public id
    /// (hex or 32 bytes). Idempotent; never downgrades a write capability.
    #[napi]
    pub async fn open_or_import_read_namespace(
        &self,
        namespace_id: Either<String, Uint8Array>,
    ) -> Result<DocHandle> {
        let id = namespace_in(namespace_id)?;
        Ok(self
            .inner
            .open_or_import_read_namespace(id)
            .await
            .map_err(aster_err)?
            .into())
    }

    /// Open-or-import a **writable** namespace from its 32-byte secret (hex
    /// or bytes). Idempotent; upgrades a prior read-only import.
    #[napi]
    pub async fn open_or_import_write_namespace(
        &self,
        namespace_secret: Either<String, Uint8Array>,
    ) -> Result<DocHandle> {
        let secret = NamespaceSecret::from_bytes(bytes32("namespace secret", namespace_secret)?);
        Ok(self
            .inner
            .open_or_import_write_namespace(secret)
            .await
            .map_err(aster_err)?
            .into())
    }

    /// Retire this node's local write authority over a namespace, keeping
    /// every entry. Local only. Release every reference first
    /// (`shutdownForRoleSwap`), or the outcome is `'busy'`.
    #[napi]
    pub async fn retire_local_write_authority(
        &self,
        namespace_id: Either<String, Uint8Array>,
    ) -> Result<RetireOutcome> {
        let id = namespace_in(namespace_id)?;
        Ok(self
            .inner
            .retire_local_write_authority(id)
            .await
            .map_err(aster_err)?
            .into())
    }

    /// The authority this node holds over the namespace `namespaceId` names.
    /// Errors if the namespace is not in the local store.
    #[napi]
    pub async fn authority_report(
        &self,
        namespace_id: Either<String, Uint8Array>,
    ) -> Result<AuthorityReport> {
        let id = namespace_in(namespace_id)?;
        Ok(self
            .inner
            .authority_report(id)
            .await
            .map_err(aster_err)?
            .into())
    }

    /// Join a document from a ticket and subscribe to its events atomically.
    #[napi]
    pub async fn join_and_subscribe(&self, ticket: String) -> Result<DocWithEvents> {
        let (doc, events) = self
            .inner
            .join_and_subscribe(ticket)
            .await
            .map_err(aster_err)?;
        Ok(DocWithEvents::new(doc, events))
    }

    /// Join a document by namespace id, syncing from `peer`, and subscribe.
    #[napi]
    pub async fn join_and_subscribe_namespace(
        &self,
        namespace_id: Either<String, Uint8Array>,
        peer: String,
    ) -> Result<DocWithEvents> {
        let id = namespace_in(namespace_id)?;
        let (doc, events) = self
            .inner
            .join_and_subscribe_namespace(id, &NodeId::from_hex(peer))
            .await
            .map_err(aster_err)?;
        Ok(DocWithEvents::new(doc, events))
    }
}

// ── DocHandle ───────────────────────────────────────────────────────────────

#[napi]
pub struct DocHandle {
    pub(crate) inner: aster::Doc,
}

impl From<aster::Doc> for DocHandle {
    fn from(inner: aster::Doc) -> Self {
        Self { inner }
    }
}

#[napi]
impl DocHandle {
    /// This document's namespace id (hex).
    #[napi]
    pub fn doc_id(&self) -> Result<String> {
        Ok(self.inner.id().map_err(aster_err)?.to_hex())
    }

    /// The authority this node holds over **this** document.
    #[napi]
    pub async fn authority_report(&self) -> Result<AuthorityReport> {
        Ok(self
            .inner
            .authority_report()
            .await
            .map_err(aster_err)?
            .into())
    }

    /// Write a key/value as `author`; returns the content hash.
    #[napi]
    pub async fn set_bytes(
        &self,
        author_id: String,
        key: Either<String, Uint8Array>,
        value: Uint8Array,
    ) -> Result<String> {
        Ok(self
            .inner
            .set_bytes(&author(author_id), bytes_in(key), value.to_vec())
            .await
            .map_err(aster_err)?
            .into_string())
    }

    /// Write a key/value after committing its blob metadata to disk — the
    /// crash-durable publication boundary (survives a hard process kill).
    #[napi]
    pub async fn set_bytes_durable(
        &self,
        author_id: String,
        key: Either<String, Uint8Array>,
        value: Uint8Array,
    ) -> Result<String> {
        Ok(self
            .inner
            .set_bytes_durable(&author(author_id), bytes_in(key), value.to_vec())
            .await
            .map_err(aster_err)?
            .into_string())
    }

    /// Delete all of `author`'s entries under `prefix`; returns how many.
    #[napi]
    pub async fn del(&self, author_id: String, prefix: Either<String, Uint8Array>) -> Result<f64> {
        let n = self
            .inner
            .del(&author(author_id), bytes_in(prefix))
            .await
            .map_err(aster_err)?;
        safe(n, "delete count")
    }

    /// The value for an exact `(author, key)`, or `null`.
    #[napi]
    pub async fn get_exact(
        &self,
        author_id: String,
        key: Either<String, Uint8Array>,
    ) -> Result<Option<Buffer>> {
        Ok(self
            .inner
            .get_exact(&author(author_id), bytes_in(key))
            .await
            .map_err(aster_err)?
            .map(Buffer::from))
    }

    /// Whether an entry exists for `(author, key)`, **including** a deletion
    /// tombstone (which `getExact` reports as absent).
    #[napi]
    pub async fn entry_exists(
        &self,
        author_id: String,
        key: Either<String, Uint8Array>,
    ) -> Result<bool> {
        self.inner
            .entry_exists(&author(author_id), bytes_in(key))
            .await
            .map_err(aster_err)
    }

    /// Entries for an exact key across authors. Ordered by author, so
    /// `limit` may drop the latest writer — use `queryLatestExact` for the
    /// current value.
    #[napi]
    pub async fn query_key_exact(
        &self,
        key: Either<String, Uint8Array>,
        limit: Option<i64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_key_exact(bytes_in(key), limit_in(limit)?)
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// The latest entry for an exact key (most recent writer), or `null` if
    /// never written or deleted.
    #[napi]
    pub async fn query_latest_exact(
        &self,
        key: Either<String, Uint8Array>,
    ) -> Result<Option<DocEntry>> {
        Ok(self
            .inner
            .query_latest_exact(bytes_in(key))
            .await
            .map_err(aster_err)?
            .map(DocEntry::from))
    }

    /// Entries under a key prefix across authors (`limit` optional).
    #[napi]
    pub async fn query_key_prefix(
        &self,
        prefix: Either<String, Uint8Array>,
        limit: Option<i64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_key_prefix(bytes_in(prefix), limit_in(limit)?)
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// The latest entry for each distinct key under a prefix (`limit` caps
    /// the number of keys).
    #[napi]
    pub async fn query_latest_prefix(
        &self,
        prefix: Either<String, Uint8Array>,
        limit: Option<i64>,
    ) -> Result<Vec<DocEntry>> {
        Ok(self
            .inner
            .query_latest_prefix(bytes_in(prefix), limit_in(limit)?)
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// Content bytes for a content hash.
    #[napi]
    pub async fn read_entry_content(&self, content_hash: String) -> Result<Buffer> {
        Ok(Buffer::from(
            self.inner
                .read_entry_content(&hash(content_hash))
                .await
                .map_err(aster_err)?,
        ))
    }

    /// Content for several hashes that are already complete locally;
    /// missing / partial values are `null` at their index.
    #[napi]
    pub async fn read_entry_contents_if_complete(
        &self,
        content_hashes: Vec<String>,
    ) -> Result<Vec<Option<Buffer>>> {
        let hashes: Vec<Hash> = content_hashes.into_iter().map(hash).collect();
        Ok(self
            .inner
            .read_entry_contents_if_complete(&hashes)
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(|v| v.map(Buffer::from))
            .collect())
    }

    /// Local storage status of an entry's value blob.
    #[napi]
    pub async fn entry_content_status(&self, content_hash: String) -> Result<BlobStatusInfo> {
        Ok(blob_status_out(
            self.inner
                .entry_content_status(&hash(content_hash))
                .await
                .map_err(aster_err)?,
        ))
    }

    /// Share as a ticket (id-only addressing).
    #[napi]
    pub async fn share(
        &self,
        #[napi(ts_arg_type = "'read' | 'write'")] mode: String,
    ) -> Result<String> {
        self.inner
            .share(share_mode_in(&mode)?)
            .await
            .map_err(aster_err)
    }

    /// Share as a ticket with relay + direct addresses.
    #[napi]
    pub async fn share_with_addr(
        &self,
        #[napi(ts_arg_type = "'read' | 'write'")] mode: String,
    ) -> Result<String> {
        self.inner
            .share_with_addr(share_mode_in(&mode)?)
            .await
            .map_err(aster_err)
    }

    /// Start syncing with peers (node ids).
    #[napi]
    pub async fn start_sync(&self, peers: Vec<String>) -> Result<()> {
        let peers: Vec<NodeId> = peers.into_iter().map(NodeId::from_hex).collect();
        self.inner.start_sync(&peers).await.map_err(aster_err)
    }

    /// Stop syncing this document.
    #[napi]
    pub async fn leave(&self) -> Result<()> {
        self.inner.leave().await.map_err(aster_err)
    }

    /// Release both of this document's references (live sync + handle) so
    /// its namespace can change role, within one `budgetMs` deadline. Every
    /// handle to the document is dead afterwards.
    #[napi]
    pub async fn shutdown_for_role_swap(&self, budget_ms: f64) -> Result<()> {
        if !budget_ms.is_finite() || budget_ms < 0.0 {
            return Err(invalid("budgetMs must be a non-negative number"));
        }
        self.inner
            .clone()
            .shutdown_for_role_swap(Duration::from_millis(budget_ms as u64))
            .await
            .map_err(aster_err)
    }

    /// Subscribe to live events (`for await (const ev of events)`).
    #[napi]
    pub async fn subscribe(&self) -> Result<DocEventReceiver> {
        Ok(DocEventReceiver {
            inner: self.inner.subscribe().await.map_err(aster_err)?,
        })
    }

    /// Set the download policy.
    #[napi]
    pub async fn set_download_policy(&self, policy: DownloadPolicy) -> Result<()> {
        let policy = policy.into_rust()?;
        self.inner
            .set_download_policy(policy)
            .await
            .map_err(aster_err)
    }

    /// The current download policy.
    #[napi]
    pub async fn get_download_policy(&self) -> Result<DownloadPolicy> {
        Ok(DownloadPolicy::from_rust(
            self.inner.get_download_policy().await.map_err(aster_err)?,
        ))
    }
}

// ── Events ──────────────────────────────────────────────────────────────────

/// A live document event.
#[napi(object)]
pub struct DocEvent {
    #[napi(
        ts_type = "'insert_local' | 'insert_remote' | 'content_ready' | 'pending_content_ready' | 'neighbor_up' | 'neighbor_down' | 'sync_finished'"
    )]
    pub kind: String,
    /// `insert_local` / `insert_remote`: the entry.
    pub entry: Option<DocEntry>,
    /// `insert_remote`: the sending peer; `neighbor_*` / `sync_finished`:
    /// the peer.
    pub peer: Option<String>,
    /// `content_ready`: the content hash now available locally.
    pub hash: Option<String>,
}

impl From<aster::DocEvent> for DocEvent {
    fn from(e: aster::DocEvent) -> Self {
        let ev = |kind: &str| DocEvent {
            kind: kind.into(),
            entry: None,
            peer: None,
            hash: None,
        };
        match e {
            aster::DocEvent::InsertLocal { entry } => DocEvent {
                entry: Some(entry.into()),
                ..ev("insert_local")
            },
            aster::DocEvent::InsertRemote { from, entry } => DocEvent {
                entry: Some(entry.into()),
                peer: Some(from.into_string()),
                ..ev("insert_remote")
            },
            aster::DocEvent::ContentReady { hash } => DocEvent {
                hash: Some(hash.into_string()),
                ..ev("content_ready")
            },
            aster::DocEvent::PendingContentReady => ev("pending_content_ready"),
            aster::DocEvent::NeighborUp { peer } => DocEvent {
                peer: Some(peer.into_string()),
                ..ev("neighbor_up")
            },
            aster::DocEvent::NeighborDown { peer } => DocEvent {
                peer: Some(peer.into_string()),
                ..ev("neighbor_down")
            },
            aster::DocEvent::SyncFinished { peer } => DocEvent {
                peer: Some(peer.into_string()),
                ..ev("sync_finished")
            },
        }
    }
}

/// A document's live events: `AsyncIterable<DocEvent>` (ends when the
/// subscription does), or call `recv()` directly.
#[napi(async_iterator)]
pub struct DocEventReceiver {
    inner: aster::DocEvents,
}

impl From<aster::DocEvents> for DocEventReceiver {
    fn from(inner: aster::DocEvents) -> Self {
        Self { inner }
    }
}

#[napi]
impl AsyncGenerator for DocEventReceiver {
    type Yield = DocEvent;
    type Next = ();
    type Return = ();

    fn next(
        &mut self,
        _value: Option<Self::Next>,
    ) -> impl std::future::Future<Output = Result<Option<Self::Yield>>> + Send + 'static {
        let inner = self.inner.clone();
        async move { Ok(inner.recv().await.map_err(aster_err)?.map(DocEvent::from)) }
    }
}

#[napi]
impl DocEventReceiver {
    /// The next event, or `null` when the subscription ends.
    #[napi]
    pub async fn recv(&self) -> Result<Option<DocEvent>> {
        Ok(self
            .inner
            .recv()
            .await
            .map_err(aster_err)?
            .map(DocEvent::from))
    }
}

/// A joined document plus its event subscription (`joinAndSubscribe*`).
#[napi]
pub struct DocWithEvents {
    doc: Option<DocHandle>,
    events: Option<DocEventReceiver>,
}

impl DocWithEvents {
    fn new(doc: aster::Doc, events: aster::DocEvents) -> Self {
        Self {
            doc: Some(doc.into()),
            events: Some(DocEventReceiver { inner: events }),
        }
    }
}

#[napi]
impl DocWithEvents {
    /// Take the doc handle (once).
    #[napi]
    pub fn take_doc(&mut self) -> Result<DocHandle> {
        self.doc
            .take()
            .ok_or_else(|| Error::from_reason("doc already taken"))
    }

    /// Take the event receiver (once).
    #[napi]
    pub fn take_events(&mut self) -> Result<DocEventReceiver> {
        self.events
            .take()
            .ok_or_else(|| Error::from_reason("events already taken"))
    }
}
