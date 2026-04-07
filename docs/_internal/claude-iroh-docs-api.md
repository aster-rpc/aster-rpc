# iroh-docs API Reference

**Crate:** `iroh-docs` — Multi-dimensional key-value documents with efficient set-reconciliation sync

**Source version:** `iroh-docs-main` (as uploaded 2026-04-03)

-----

## 1. Overview

iroh-docs is a replicated key-value store where each document (called a “replica”) is identified by a cryptographic namespace keypair. Entries are keyed by `(NamespaceId, AuthorId, Key)` — meaning multiple authors can write to the same key, and each author’s version is retained independently.

**Core properties:**

- Every entry is signed by both the **namespace key** (proving write access) and the **author key** (proving authorship).
- Entry values are not stored in the replica directly — the entry holds the content’s BLAKE3 hash and size. The actual content bytes live in an `iroh-blobs` store.
- Synchronization between peers uses **range-based set reconciliation**, an efficient algorithm for computing the union of two sets by recursively partitioning and comparing fingerprints. Based on [this paper](https://arxiv.org/abs/2212.13567) by Aljoscha Meyer.
- The sync protocol runs over QUIC streams using a dedicated ALPN.
- Live sync is coordinated via `iroh-gossip` — peers discover each other per-document through gossip topics and trigger set-reconciliation syncs.

**ALPN:** `b"/iroh-sync/1"` (exported as `iroh_docs::ALPN`)

-----

## 2. Identity & Capability Model

### 2.1 `Author` / `AuthorId` / `AuthorPublicKey`

An author is an ed25519 signing key used to sign entries. The `AuthorId` is the 32-byte public key, used as the identifier.

```rust
use iroh_docs::Author;

// Create a new random author
let author = Author::new(&mut rand::rng());

// Identifiers
let author_id: AuthorId = author.id();
let public_key: AuthorPublicKey = author.public_key();

// Serialization
let bytes: [u8; 32] = author.to_bytes();
let restored = Author::from_bytes(&bytes);
let from_hex: Author = "abcdef...".parse()?;
```

**Important:** The `Author` struct contains the **secret signing key**. Treat it as sensitive material. `AuthorId` is safe to share freely.

### 2.2 `NamespaceSecret` / `NamespaceId` / `NamespacePublicKey`

A namespace key identifies and authorizes writes to a document. The `NamespaceId` is the 32-byte public key.

```rust
use iroh_docs::NamespaceSecret;

let namespace = NamespaceSecret::new(&mut rand::rng());
let namespace_id: NamespaceId = namespace.id();
let bytes: [u8; 32] = namespace.to_bytes();
```

**Key insight:** Possessing the `NamespaceSecret` grants **write access** to the document. Possessing only the `NamespaceId` gives **read access**. This is the capability model.

### 2.3 `Capability`

```rust
pub enum Capability {
    Write(NamespaceSecret),  // Can read and write
    Read(NamespaceId),       // Can only read (sync)
}

impl Capability {
    pub fn id(&self) -> NamespaceId;
    pub fn secret_key(&self) -> Result<&NamespaceSecret, ReadOnly>;
    pub fn kind(&self) -> CapabilityKind;  // Write or Read
    pub fn merge(&mut self, other: Capability) -> Result<bool, CapabilityError>;
}
```

`merge()` upgrades a Read capability to Write if the other capability provides the secret. Downgrade is not possible.

### 2.4 `CapabilityKind`

```rust
pub enum CapabilityKind {
    Write = 1,
    Read = 2,
}
```

-----

## 3. Data Model

### 3.1 `RecordIdentifier`

The composite key for an entry: `NamespaceId (32B) || AuthorId (32B) || Key (variable)`.

```rust
pub struct RecordIdentifier(Bytes);

impl RecordIdentifier {
    pub fn new(namespace: impl Into<NamespaceId>, author: impl Into<AuthorId>, key: impl AsRef<[u8]>) -> Self;
    pub fn namespace(&self) -> NamespaceId;
    pub fn author(&self) -> AuthorId;
    pub fn key(&self) -> &[u8];
}
```

**Critical implication for Aster’s registry:** Because the composite key includes `AuthorId`, two different authors writing to the same key produce **two distinct entries**, not a conflict. This is why `(AuthorId, Key)` forms the unique key — untrusted authors cannot shadow trusted authors’ entries.

### 3.2 `Record`

The value portion of an entry.

```rust
pub struct Record {
    len: u64,        // Content size in bytes
    hash: Hash,      // BLAKE3 hash of the content (iroh-blobs Hash)
    timestamp: u64,  // Microseconds since Unix epoch
}

impl Record {
    pub fn new(hash: Hash, len: u64, timestamp: u64) -> Self;
    pub fn empty(timestamp: u64) -> Self;      // Tombstone (deletion marker)
    pub fn empty_current() -> Self;            // Tombstone with current timestamp
    pub fn content_hash(&self) -> Hash;
    pub fn content_len(&self) -> u64;
    pub fn timestamp(&self) -> u64;
}
```

**Ordering:** Records are ordered by timestamp first, then by content hash. This means the “latest” entry wins when merging.

### 3.3 `Entry`

Combines identity and value.

```rust
pub struct Entry {
    id: RecordIdentifier,
    record: Record,
}

impl Entry {
    pub fn namespace(&self) -> NamespaceId;
    pub fn author(&self) -> AuthorId;
    pub fn key(&self) -> &[u8];
    pub fn content_hash(&self) -> Hash;
    pub fn content_len(&self) -> u64;
    pub fn timestamp(&self) -> u64;
    pub fn sign(self, namespace: &NamespaceSecret, author: &Author) -> SignedEntry;
}
```

`Entry` derefs to `Record`, so record methods are directly available.

### 3.4 `SignedEntry`

An entry with cryptographic signatures from both the namespace and author keys.

```rust
pub struct SignedEntry {
    entry: Entry,
    signature: EntrySignature,
}

impl SignedEntry {
    pub fn verify<S: PublicKeyStore>(&self, store: &S) -> Result<(), SignatureError>;
    pub fn entry(&self) -> &Entry;
    pub fn content_hash(&self) -> Hash;
    pub fn content_len(&self) -> u64;
    pub fn signature(&self) -> &EntrySignature;
}
```

`SignedEntry` derefs to `Entry`, which derefs to `Record` — so all accessor methods are available directly.

-----

## 4. Store Backend

### 4.1 `store::Store`

The storage backend, backed by `redb`. Supports both in-memory and persistent modes.

```rust
use iroh_docs::store::Store;

// In-memory (for tests, ephemeral use)
let store = Store::memory();

// Persistent (backed by a file on disk)
let store = Store::persistent("path/to/docs.redb")?;
```

**Key store operations:**

```rust
impl Store {
    // Document management
    pub fn new_replica(&mut self, namespace: NamespaceSecret) -> Result<Replica>;
    pub fn open_replica(&mut self, id: &NamespaceId) -> Result<Replica, OpenError>;
    pub fn close_replica(&mut self, id: NamespaceId);
    pub fn remove_replica(&mut self, namespace: &NamespaceId) -> Result<()>;
    pub fn list_namespaces(&mut self) -> Result<impl Iterator<Item = Result<(NamespaceId, CapabilityKind)>>>;
    pub fn import_namespace(&mut self, capability: Capability) -> Result<ImportNamespaceOutcome>;

    // Author management
    pub fn new_author<R: CryptoRng>(&mut self, rng: &mut R) -> Result<Author>;
    pub fn import_author(&mut self, author: Author) -> Result<()>;
    pub fn get_author(&mut self, author_id: &AuthorId) -> Result<Option<Author>>;
    pub fn delete_author(&mut self, author: AuthorId) -> Result<()>;
    pub fn list_authors(&mut self) -> Result<impl Iterator<Item = Result<Author>>>;

    // Querying
    pub fn get_many(&mut self, namespace: &NamespaceId, query: impl Into<Query>) -> Result<impl Iterator<Item = Result<SignedEntry>>>;
    pub fn get_exact(&mut self, namespace: &NamespaceId, author: &AuthorId, key: impl AsRef<[u8]>, include_empty: bool) -> Result<Option<SignedEntry>>;
    pub fn content_hashes(&mut self) -> Result<ContentHashesIterator>;

    // Download policies
    pub fn set_download_policy(&mut self, namespace: &NamespaceId, policy: DownloadPolicy) -> Result<()>;
    pub fn get_download_policy(&mut self, namespace: &NamespaceId) -> Result<DownloadPolicy>;

    // Peer tracking
    pub fn register_useful_peer(&mut self, namespace: NamespaceId, peer: PeerIdBytes, ...) -> Result<()>;
    pub fn get_sync_peers(&mut self, namespace: &NamespaceId) -> Result<Option<PeersIter>>;

    // Maintenance
    pub fn flush(&mut self) -> Result<()>;
}
```

**Note:** The store is `!Send` and `!Sync` — it runs behind an actor (`SyncHandle`) in production. The `DocsApi` provides the async, actor-safe interface.

-----

## 5. High-Level API (`DocsApi` / `Doc`)

This is the user-facing API. It communicates with the store through an actor and is fully async.

### 5.1 `DocsApi`

The top-level service API.

```rust
use iroh_docs::api::DocsApi;

// From an Engine (production)
let api = DocsApi::spawn(engine);

// Over RPC (requires "rpc" feature)
let api = DocsApi::connect(endpoint, addr)?;
```

#### Author management

```rust
// Create a new author
let author_id = api.author_create().await?;

// Get/set the default author
let default = api.author_default().await?;
api.author_set_default(author_id).await?;

// List all authors we have keys for
let mut stream = api.author_list().await?;
while let Some(Ok(id)) = stream.next().await { /* ... */ }

// Export/import author keys (sensitive!)
let author: Option<Author> = api.author_export(author_id).await?;
api.author_import(author).await?;

// Delete an author permanently
api.author_delete(author_id).await?;
```

#### Document lifecycle

```rust
// Create a new document
let doc: Doc = api.create().await?;

// Open an existing document
let doc: Option<Doc> = api.open(namespace_id).await?;

// List all documents
let mut stream = api.list().await?;
while let Some(Ok((id, kind))) = stream.next().await {
    // kind: CapabilityKind::Write or CapabilityKind::Read
}

// Import from a capability (no sync)
let doc = api.import_namespace(capability).await?;

// Import from a ticket (imports + starts sync with peers in ticket)
let doc = api.import(ticket).await?;

// Import and subscribe atomically (guaranteed to not miss sync events)
let (doc, events) = api.import_and_subscribe(ticket).await?;

// Permanently delete a document
api.drop_doc(namespace_id).await?;
```

### 5.2 `Doc`

Handle for a single document.

#### Writing

```rust
// Set a key to a byte value (hashes and stores content via blobs)
let hash = doc.set_bytes(author_id, "my-key", b"my-value").await?;

// Set a key to a pre-existing blob hash (content already in blobs store)
doc.set_hash(author_id, "my-key", hash, size).await?;

// Delete entries by prefix
let removed = doc.del(author_id, "prefix/").await?;
```

**How writes work:** `set_bytes` first imports the value bytes into the iroh-blobs store (getting back a hash), then creates a signed entry pointing to that hash. The entry contains only the hash and size, not the bytes themselves.

#### Reading

```rust
// Get a single entry by exact author + key
let entry: Option<Entry> = doc.get_exact(author_id, "my-key", false).await?;

// Get entries matching a query
let mut stream = doc.get_many(Query::key_exact("my-key").build()).await?;
while let Some(Ok(entry)) = stream.next().await {
    let key = std::str::from_utf8(entry.key())?;
    let hash = entry.content_hash();
    let size = entry.content_len();
    let timestamp = entry.timestamp();
    let author = entry.author();
}

// Get a single entry from a query
let entry: Option<Entry> = doc.get_one(Query::key_prefix("config/").build()).await?;
```

**To read the actual content bytes**, use the content hash with iroh-blobs:

```rust
let entry = doc.get_exact(author_id, "my-key", false).await?.unwrap();
let content: Bytes = blobs_store.get_bytes(entry.content_hash()).await?;
```

#### Sync & Sharing

```rust
// Start syncing with specific peers
doc.start_sync(vec![peer_addr]).await?;

// Stop syncing
doc.leave().await?;

// Share the document (creates a ticket)
let ticket: DocTicket = doc.share(ShareMode::Read, AddrInfoOptions::RelayAndAddresses).await?;
// or ShareMode::Write for write access

// Subscribe to live events
let mut events = doc.subscribe().await?;
while let Some(Ok(event)) = events.next().await {
    match event {
        LiveEvent::InsertLocal { entry } => { /* we inserted */ }
        LiveEvent::InsertRemote { from, entry, content_status } => { /* peer inserted */ }
        LiveEvent::ContentReady { hash } => { /* blob downloaded */ }
        LiveEvent::PendingContentReady => { /* all downloads done */ }
        LiveEvent::NeighborUp(peer) => { /* peer joined gossip */ }
        LiveEvent::NeighborDown(peer) => { /* peer left gossip */ }
        LiveEvent::SyncFinished(sync_event) => { /* reconciliation done */ }
    }
}

// Get current sync peers
let peers: Option<Vec<PeerIdBytes>> = doc.get_sync_peers().await?;

// Document status
let status: OpenState = doc.status().await?;
```

#### File import/export

```rust
// Import a file into the document
let progress = doc.import_file(
    &blobs_store,
    author_id,
    Bytes::from("files/report.pdf"),
    "/path/to/report.pdf",
    ImportMode::Copy,
).await?;
let outcome: ImportFileOutcome = progress.await?;
// outcome.hash, outcome.size, outcome.key

// Export an entry to a file
let entry = doc.get_exact(author_id, "files/report.pdf", false).await?.unwrap();
doc.export_file(&blobs_store, entry, "/path/to/output.pdf", ExportMode::Copy).await?;
```

#### Download policy

```rust
use iroh_docs::store::{DownloadPolicy, FilterKind};

// Download everything (default)
doc.set_download_policy(DownloadPolicy::EverythingExcept(vec![])).await?;

// Only download specific prefixes
doc.set_download_policy(DownloadPolicy::NothingExcept(vec![
    FilterKind::Prefix(Bytes::from("important/")),
    FilterKind::Exact(Bytes::from("config")),
])).await?;

// Skip large media files
doc.set_download_policy(DownloadPolicy::EverythingExcept(vec![
    FilterKind::Prefix(Bytes::from("media/")),
])).await?;

let policy = doc.get_download_policy().await?;
```

#### Close

```rust
doc.close().await?;
// After close, all methods return an error
```

-----

## 6. Query API

The `Query` builder provides flexible entry retrieval.

```rust
use iroh_docs::store::{Query, SortBy, SortDirection};

// All entries
Query::all().build()

// Filter by author
Query::author(author_id).build()

// Filter by exact key
Query::key_exact("my-key").build()

// Filter by key prefix
Query::key_prefix("config/").build()

// Combined filters with sorting
Query::all()
    .author(author_id)
    .key_prefix("settings/")
    .sort_by(SortBy::KeyAuthor, SortDirection::Asc)
    .limit(100)
    .offset(50)
    .include_empty()  // include deletion markers
    .build()

// Latest entry per key (deduplicates across authors)
Query::single_latest_per_key()
    .key_prefix("config/")
    .sort_direction(SortDirection::Desc)
    .build()
```

**`SortBy` options:** `AuthorKey` (default — sort by author, then key) or `KeyAuthor` (sort by key, then author).

**`single_latest_per_key`:** For each unique key, returns only the entry with the highest timestamp. This is the mode you want when treating the document as a conventional KV store where only the latest value matters. Note: the key filter is applied *before* grouping, the author filter *after*.

-----

## 7. `DocTicket`

A serializable token containing a document capability and peer addresses.

```rust
use iroh_docs::DocTicket;

// Create
let ticket = DocTicket::new(capability, vec![peer_addr]);

// Serialize (base32 string with "doc" prefix)
let s = ticket.to_string();    // "docaaaa..."
let t: DocTicket = s.parse()?;

// Fields
ticket.capability  // Capability (Read or Write)
ticket.nodes       // Vec<EndpointAddr>
```

-----

## 8. `Engine` — Live Sync Coordination

The `Engine` ties together the store, iroh-blobs, iroh-gossip, and the sync protocol into a live system.

```rust
use iroh_docs::engine::Engine;

let engine = Engine::spawn(
    endpoint,           // iroh Endpoint
    gossip,             // iroh_gossip::Gossip
    replica_store,      // iroh_docs::store::Store
    bao_store,          // iroh_blobs::api::Store
    downloader,         // iroh_blobs Downloader
    default_author_storage,
    protect_cb,         // Optional GC protection callback
).await?;
```

In production, you typically don’t interact with the Engine directly — you use `DocsApi::spawn(engine)` and work through the `DocsApi` / `Doc` handles.

### 8.1 `LiveEvent`

Events emitted during live sync.

```rust
pub enum LiveEvent {
    InsertLocal { entry: Entry },
    InsertRemote { from: PublicKey, entry: Entry, content_status: ContentStatus },
    ContentReady { hash: Hash },
    PendingContentReady,
    NeighborUp(PublicKey),
    NeighborDown(PublicKey),
    SyncFinished(SyncEvent),
}
```

### 8.2 `ContentStatus`

```rust
pub enum ContentStatus {
    Complete,    // Content blob is fully available locally
    Incomplete,  // Content blob is partially available
    Missing,     // Content blob is not available
}
```

-----

## 9. Sync Protocol

The sync protocol uses range-based set reconciliation over QUIC bi-directional streams.

**Protocol flow:**

1. Alice opens a QUIC stream to Bob using ALPN `b"/iroh-sync/1"`.
1. Alice sends the namespace she wants to sync.
1. Bob checks if he has the namespace and accepts/rejects.
1. They exchange recursive partition fingerprints until they’ve identified the differences.
1. Missing entries are exchanged. Each entry is verified against the namespace and author signatures.
1. The stream is closed cleanly.

**Functions for manual sync (without the Engine):**

```rust
use iroh_docs::net;

// Initiate sync (Alice side)
let result: SyncFinished = net::connect_and_sync(
    &endpoint, &sync_handle, namespace_id, peer_addr, Some(&metrics)
).await?;

// Handle incoming sync (Bob side)
let result: SyncFinished = net::handle_connection(
    sync_handle, connection, accept_callback, Some(&metrics)
).await?;
```

`SyncFinished` contains the namespace, peer, outcome (`SyncOutcome { num_recv, num_sent, heads_received }`), and timing information.

-----

## 10. Key Considerations & Gotchas

**Content is stored separately.** Entry values in iroh-docs are `(Hash, Size)` pointers, not the bytes themselves. You need an iroh-blobs store to actually store and retrieve content. When a remote entry arrives via sync, the content blob must be separately downloaded using iroh-blobs.

**Multi-author semantics.** The same key can have entries from multiple authors, and all are retained. If you want “latest wins” semantics (like a normal KV store), use `Query::single_latest_per_key()`. Otherwise, `get_exact(author, key)` retrieves a specific author’s entry.

**Deletion is a tombstone.** `doc.del()` inserts an empty entry (hash = EMPTY, size = 0) with the given key prefix. This tombstone replicates like any other entry. Old entries with matching keys are superseded by the tombstone’s newer timestamp.

**Write capability = the secret key.** Sharing `NamespaceSecret` (via `ShareMode::Write`) gives permanent, irrevocable write access. There is no way to revoke it once shared. `ShareMode::Read` shares only the `NamespaceId`.

**Eventual consistency.** iroh-docs provides eventual consistency through set reconciliation. There is no consensus protocol — all entries from all authors are accepted and replicated. Conflict resolution is timestamp-based (latest wins) within `single_latest_per_key`, but at the raw level all versions coexist.

**Sync is per-document.** Each document has its own gossip topic and sync state. Joining many documents increases the number of gossip connections and sync overhead.

**Store is single-threaded.** The `store::Store` is `!Send` and runs behind a dedicated actor thread (`SyncHandle`). All access goes through message passing. The `DocsApi` wraps this in an async-friendly interface.

**Download policies.** By default, all content referenced by synced entries is downloaded. Use `DownloadPolicy::NothingExcept` to selectively download only entries matching specific key patterns. This is important for large documents where you don’t need all content locally.

**Timestamps are wall clock.** Timestamps are microseconds since Unix epoch from `SystemTime::now()`. There is no logical clock. Clock skew between nodes can cause unexpected ordering — an entry from a node with a fast clock will appear “newer” than one from a slow clock even if the slow clock’s entry was actually written later in real time.

**Gossip integration.** The `Engine` creates a gossip topic per syncing document. When entries are inserted locally, a gossip notification is broadcast. When peers receive the notification, they initiate a set-reconciliation sync. This means changes propagate in near-real-time to all peers in the document’s swarm.