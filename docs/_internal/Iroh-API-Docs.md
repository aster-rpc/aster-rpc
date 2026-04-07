# Iroh API Reference

> **Sources:** `iroh-blobs` (v0.99.0), `iroh-gossip` (v0.97.0), `iroh-docs` (v0.97.0)
>
> This document covers the three core Iroh crates and is intended for developers building applications on top of them. It is source-driven — every claim is traceable to the upstream Rust source. Where behaviour is ambiguous from rustdoc alone, the source is cited.
>
> **Python bindings note:** This document describes the upstream Rust API surface. Not every method is yet exposed in the Python bindings (`bindings/aster_rs/`). See the [Python Bindings Status](#python-bindings-status) appendix for what is currently available.

---

## Table of Contents

1. [Mental Model: How the Three Crates Fit Together](#1-mental-model-how-the-three-crates-fit-together)
2. [iroh-blobs — Content-Addressed Blob Storage & Transfer](#2-iroh-blobs)
3. [iroh-gossip — Topic-Based Pub-Sub Broadcast](#3-iroh-gossip)
4. [iroh-docs — Replicated Key-Value Documents](#4-iroh-docs)
5. [Key Gotchas & Sharp Edges](#5-key-gotchas--sharp-edges)
6. [Python Bindings Status](#6-python-bindings-status)

---

## 1. Mental Model: How the Three Crates Fit Together

```
┌─────────────────────────────────────────────────────┐
│  iroh-gossip  (ephemeral broadcast / liveness)     │
│  topic-based pub-sub; no persistence; no auth       │
└──────────────────────┬──────────────────────────────┘
                       │ coordinates live sync
┌──────────────────────▼──────────────────────────────┐
│  iroh-docs  (replicated metadata / queries / sync) │
│  signed entries; set-reconciliation over QUIC        │
│  entries point to blob hashes, not bytes             │
└──────────────────────┬──────────────────────────────┘
                       │ stores & transfers content
┌──────────────────────▼──────────────────────────────┐
│  iroh-blobs  (verified content transport + storage) │
│  BLAKE3/BAO; content-addressed; tag-managed GC      │
└─────────────────────────────────────────────────────┘
```

| Crate | Purpose | Persistence | Auth model |
|---|---|---|---|
| `iroh-gossip` | Ephemeral fan-out, liveness, invalidation hints | None (in-memory only) | Topic ID = rendezvous, not auth |
| `iroh-blobs` | Verified bulk content transport and retention | Tag/GC-managed | None (content addressed by hash) |
| `iroh-docs` | Replicated signed metadata over blob hashes | Durable (redb) | Dual-signed (namespace + author) |

---

## 2. iroh-blobs

### 2.1 Overview

`iroh-blobs` is a content-addressed data transfer and storage system built on BLAKE3 verified streaming (bao). It provides:

- A protocol for streaming content-addressed data transfer with verification at 16 KiB chunk granularity.
- Pluggable store backends: in-memory (`MemStore`), filesystem-backed (`FsStore`), and read-only memory (`ReadonlyMemStore`).
- A `BlobsProtocol` handler for serving blobs over QUIC connections.
- A `Downloader` for fetching blobs from multiple remote providers with automatic failover.
- `BlobTicket` for self-contained, serialisable capability tokens.

**ALPN:** `b"/iroh-blobs/0"` (exported as `iroh_blobs::ALPN`)

### 2.2 Core Types

#### `Hash`

A 32-byte BLAKE3 hash — the universal content identifier.

```rust
pub struct Hash(blake3::Hash);

impl Hash {
    pub const EMPTY: Hash;
    pub fn new(buf: impl AsRef<[u8]>) -> Self;
    pub fn as_bytes(&self) -> &[u8; 32];
    pub fn from_bytes(bytes: [u8; 32]) -> Self;
    pub fn to_hex(&self) -> String;
    pub fn fmt_short(&self) -> ArrayString<10>;  // First 5 bytes, hex
}
```

Implements `Copy`, `Eq`, `Hash`, `Serialize`, `Deserialize`, `FromStr` (hex), `Display` (hex).

#### `BlobFormat`

```rust
pub enum BlobFormat {
    Raw,     // A single opaque blob
    HashSeq, // A blob whose content is a sequence of 32-byte child hashes
}
```

- `Raw` — a plain blob.
- `HashSeq` — the blob's content is a packed sequence of child hashes. This is how collections, directories, and multi-blob transfers work. When you tag a `HashSeq`, all referenced children are also protected from GC.

#### `HashAndFormat`

A `(Hash, BlobFormat)` pair. The hash alone is ambiguous — the same hash could be a raw blob or a hash-sequence root. Always pass `HashAndFormat` to APIs that need to resolve content.

```rust
pub struct HashAndFormat {
    pub hash: Hash,
    pub format: BlobFormat,
}
```

### 2.3 Store Backends

All store backends `Deref` to `api::Store`, so every method on `Store`, `Blobs`, `Tags`, `Remote` is callable directly.

#### `MemStore` — In-memory

Good for tests and ephemeral data. **Spawns a background actor on creation. Data is lost when dropped.**

```rust
let store = MemStore::new();
```

#### `FsStore` — Persistent filesystem (production default)

Hybrid approach: blobs ≤16 KiB are stored inline in a `redb` database; larger blobs get data + outboard files on disk. This gives good performance for millions of tiny blobs and multi-GB files.

```rust
let store = FsStore::load("path/to/blobs.db").await?;
```

**Requires feature `fs-store`.**

### 2.4 The `Store` API — Entry Point

```rust
impl Store {
    pub fn tags(&self) -> &Tags;           // Tag management
    pub fn blobs(&self) -> &Blobs;         // Blob import/export/read
    pub fn remote(&self) -> &Remote;        // Single-node download
    pub fn downloader(&self, endpoint: &Endpoint) -> Downloader; // Multi-node download

    pub async fn sync_db(&self) -> RequestResult<()>;
    pub async fn shutdown(&self) -> irpc::Result<()>;
    pub async fn wait_idle(&self) -> irpc::Result<()>; // Mostly for tests; can wait forever
}
```

**Important:** `Store` `Deref`s to `Blobs`, so blob methods are directly on `store`.

### 2.5 `Blobs` API — Import, Export, Read

#### Adding data

```rust
// From a byte slice
let tag_info = store.blobs().add_slice(b"hello world").await?;

// From Bytes (zero-copy if already Bytes)
let tag_info = store.blobs().add_bytes(my_bytes).await?;

// From a file path
let tag_info = store.blobs().add_path("/path/to/file").await?;

// With explicit options
let progress = store.blobs().add_path_with_opts(AddPathOptions {
    path: "/path/to/file".into(),
    format: BlobFormat::Raw,
    mode: ImportMode::Copy,  // Copy or TryReference
});
let tag_info = progress.await?;  // IntoFuture → TagInfo
```

All add methods return `AddProgress` which can be consumed as:
- `.await` / `.into_future()` → `TagInfo` (persistent tag)
- `.temp_tag().await` → `TempTag` (ephemeral in-memory protection)
- `.stream().await` → `Stream<Item = AddProgressItem>` for progress tracking

**`ImportMode`:** `Copy` is always safe. `TryReference` attempts to reference the original file in place (only on `FsStore`, with reflink support; store may fall back to copy).

#### Reading data

```rust
// Get all bytes (⚠️ can exhaust memory on large blobs — use only for small/metadata blobs)
let bytes: Bytes = store.blobs().get_bytes(hash).await?;

// AsyncRead + AsyncSeek reader (⚠️ errors on missing chunks — does not auto-fetch)
let mut reader = store.blobs().reader(hash);
reader.read_to_string(&mut buf).await?;

// Export specific byte ranges (rounds up to chunk boundaries internally)
let progress = store.blobs().export_ranges(hash, 0..1024u64);

// Export to a file path
store.blobs().export(hash, "/path/to/output").await?;
```

**⚠️ Range export rounding:** byte ranges are rounded up to chunk (16 KiB) boundaries internally. Callers are responsible for clipping to the originally requested byte range if exact slicing matters.

#### Batched operations

```rust
let batch = store.blobs().batch().await?;
let tt1 = batch.add_bytes(b"data1").await?;   // TempTag
let tt2 = batch.add_slice(b"data2").await?;
// Both are GC-protected while batch lives; dropping batch releases protection
```

#### Querying

```rust
store.blobs().has(hash).await?         // bool — "is complete?"
store.blobs().status(hash).await?      // BlobStatus::Complete | Partial | NotFound
store.blobs().observe(hash).await?     // Bitfield — which chunks are present
store.blobs().list()                    // Stream of (Hash, BlobStatus)
```

`observe()` is a sleeper feature: it returns the current bitfield on await, or streams updates as chunks arrive — ideal for resumable/partial transfer logic.

### 2.6 `Tags` API — Named References

Tags keep content alive against GC. A blob is protected as long as at least one tag or temp tag points to it.

```rust
let tags = store.tags();

// Create an auto-named tag
let tag = tags.create(hash_and_format).await?;

// Set a named tag
tags.set("my-data", hash_and_format).await?;

// Get a tag value
let info: Option<TagInfo> = tags.get("my-data").await?;

// List all tags
let stream = tags.list().await?;

// Temp tags — ephemeral in-memory protection
let tt: TempTag = tags.temp_tag(hash_and_format).await?;
// Protected until tt is dropped
// TempTag::leak() keeps it alive until process exit (blunt instrument)

tags.delete("my-data").await?;
tags.delete_all().await?;  // ⚠️ All data becomes GC-eligible immediately
```

**⚠️ `delete_all()` warning:** removes all tag protection. All content becomes eligible for garbage collection.

### 2.7 `Remote` API — Single-Node Downloads

For downloading from one known remote node.

```rust
let remote = store.remote();

// Inspect what we already have locally
let info: LocalInfo = remote.local(hash_and_format).await?;
info.is_complete();
info.local_bytes();
info.missing();  // Compute efficient resume request automatically

// Fetch (local-aware, resumable)
let stats = remote.fetch(connection, hash_and_format).await?;

// Raw get (ignores local state — fetches exactly what you request)
let stats = remote.execute_get(connection, get_request).await?;

// Observe remote completeness bitfield
let stream = remote.observe(connection, observe_request);
```

**`fetch()` is almost always the right call.** It inspects local state first, computes missing ranges, and only asks the remote for what you don't have. `execute_get*` is for low-level protocol control.

### 2.8 `Downloader` — Multi-Node Downloads

Fetches from multiple providers with automatic failover, parallel chunk splitting, and connection pooling.

```rust
let downloader = store.downloader(&endpoint);  // Reuse this object!

// Simple
downloader.download(hash_and_format, vec![peer_id1, peer_id2]).await?;

// With options
downloader.download_with_opts(DownloadOptions::new(
    GetRequest::all(root_hash),
    Shuffled::new(vec![id1, id2, id3]),  // Randomised provider order
    SplitStrategy::Split,  // Parallel per-child for HashSeq
)).await?;
```

**⚠️ Reuse the downloader.** It holds internal state and a connection pool. Creating it ad hoc loses connection reuse.

**⚠️ `Split` vs `None`:** `SplitStrategy::Split` first fetches the root/manifest to discover children, then fetches children in parallel from different providers. Essential for collections.

### 2.9 `BlobsProtocol` — Serving Over Iroh

The integration point for serving blobs on a `Router`.

```rust
let blobs = BlobsProtocol::new(&store, None);  // None = no event sender

let router = Router::builder(endpoint)
    .accept(iroh_blobs::ALPN, blobs.clone())
    .spawn();

// blobs.store() gives access to the Store API
let tag = blobs.store().blobs().add_slice(b"hello").await?;
```

`BlobsProtocol` `Deref`s to `Store`.

### 2.10 `BlobTicket` — Capability Tokens

A self-contained token encoding everything needed to fetch a blob.

```rust
let ticket = BlobTicket::new(endpoint.addr(), hash, BlobFormat::Raw);
let s = ticket.to_string();    // "blobaaaa..." base32 string
let t: BlobTicket = s.parse()?;

ticket.hash();
ticket.format();
ticket.addr();
ticket.recursive();  // true for HashSeq
```

---

## 3. iroh-gossip

### 3.1 Overview

`iroh-gossip` is a topic-based pub-sub system implementing two protocols:

- **HyParView** (membership): maintains a partial view of peers per topic. Default: 5 active connections, 30 passive peers. Self-heals on node failure.
- **PlumTree** (broadcast): eager push to nearby peers, lazy push (IHave/Graft) to others, with automatic tree optimisation by latency.

Messages are broadcast to all peers subscribed to a topic. Each topic is an independent swarm with independent membership.

**⚠️ Topic ID is rendezvous, not authorisation.** Possessing a topic ID is effectively enough to join and speak unless your application adds its own policy or signature layer.

**ALPN:** `b"/iroh-gossip/1"` (exported as `iroh_gossip::ALPN`)

### 3.2 Core Types

#### `TopicId`

A 32-byte topic identifier. Create random topics with `TopicId::from_bytes(rand::random())`.

#### `Event`

```rust
pub enum Event {
    NeighborUp(EndpointId),   // New direct neighbor connected
    NeighborDown(EndpointId), // Direct neighbor disconnected
    Received(Message),        // Gossip message received
    Lagged,                   // ⚠️ Receiver fell behind; subscription is now closed
}
```

**⚠️ `Lagged` is terminal.** When the internal event channel (default capacity: 2048) overflows, the subscriber receives `Lagged`, the lagged message is dropped, and the subscription is closed. You must recreate the subscription if this happens.

#### `Message`

```rust
pub struct Message {
    pub content: Bytes,              // The message payload
    pub scope: DeliveryScope,          // How the message arrived
    pub delivered_from: EndpointId,    // ⚠️ Peer who delivered it — NOT the original author
}
```

**⚠️ `delivered_from` is not the author.** If authorship matters, sign your payload at the application layer (see the chat example below).

#### `Command`

```rust
pub enum Command {
    Broadcast(Bytes),              // Broadcast to entire swarm
    BroadcastNeighbors(Bytes),     // Broadcast to direct neighbors only (1 hop)
    JoinPeers(Vec<EndpointId>),    // Pull additional peers into the topic mesh
}
```

### 3.3 Setup and Lifecycle

```rust
use iroh_gossip::{Gossip, GOSSIP_ALPN, TopicId};

let gossip = Gossip::builder()
    .max_message_size(4096)  // Default: 4096 bytes — exceeds = silent drop
    .spawn(endpoint.clone());

let router = Router::builder(endpoint)
    .accept(GOSSIP_ALPN, gossip.clone())
    .spawn();
```

### 3.4 Subscribing to a Topic

```rust
// Returns immediately; does NOT wait for a peer connection
let topic = gossip.subscribe(topic_id, bootstrap_peer_ids).await?;

// Waits until at least one neighbor is connected
let topic = gossip.subscribe_and_join(topic_id, bootstrap_peer_ids).await?;

// Fine-grained control
let topic = gossip.subscribe_with_opts(topic_id, JoinOptions {
    bootstrap: peers.into_iter().collect(),
    subscription_capacity: 4096,  // Default: 2048
}).await?;
```

**⚠️ `subscribe()` queues messages before the first connection is up, but that queue is finite.**

### 3.5 Sending and Receiving

`GossipTopic` is both a sender and a `Stream<Item = Result<Event, ApiError>>`.

```rust
let mut topic = gossip.subscribe_and_join(topic_id, peers).await?;

// Send
await topic.broadcast(Bytes::from("hello")).await?;
await topic.broadcast_neighbors(Bytes::from("local-only")).await?;

// Receive
while let Some(event) = topic.next().await {
    match event? {
        Event::Received(msg) => { ... }
        Event::NeighborUp(id) => { ... }
        Event::NeighborDown(id) => { ... }
        Event::Lagged => { /* recreate subscription */ }
    }
}
```

### 3.6 Splitting Sender and Receiver

```rust
let (sender, receiver) = topic.split();
// sender: GossipSender — Clone, Send, Sync
// receiver: GossipReceiver — Stream<Item = Result<Event, ApiError>>

// Topic is alive until BOTH halves are dropped
```

Use this to broadcast from one task while receiving in another.

### 3.7 Configuration Reference

**HyParViewConfig defaults:**

| Field | Default | Description |
|---|---|---|
| `active_view_capacity` | 5 | Active peer connections per topic |
| `passive_view_capacity` | 30 | Passive peer address book size |
| `shuffle_interval` | 60s | Interval between shuffle rounds |
| `neighbor_request_timeout` | 500ms | Timeout for Neighbor requests |

**PlumTreeConfig defaults:**

| Field | Default | Description |
|---|---|---|
| `graft_timeout_1` | 500ms | Timeout before Graft request |
| `graft_timeout_2` | 250ms | Retry timeout for Graft |
| `message_cache_retention` | 60s | How long messages stay in cache |
| `optimization_threshold` | Round(1) | Hop diff to promote lazy→eager peer |

### 3.8 Chat Example: Application-Layer Signing

The upstream chat example (`iroh-gossip/examples/chat.rs`) signs every message with the endpoint's `SecretKey` and verifies on receipt. Python clients are **not** wire-compatible with this format (it uses Rust's `postcard` binary serialiser). Here is the pattern for your own application layer:

```rust
// Sign on send
let signed = SignedMessage::sign_and_encode(endpoint.secret_key(), &message)?;
sender.broadcast(encoded).await?;

// Verify on receive
let (from, message) = SignedMessage::verify_and_decode(&msg.content)?;
```

---

## 4. iroh-docs

### 4.1 Overview

`iroh-docs` is a replicated key-value store where each document (called a "replica") is identified by a cryptographic namespace keypair. Entries are keyed by `(NamespaceId, AuthorId, Key)` — meaning multiple authors can write to the same key, and each author's version is retained independently.

**Critical: docs stores metadata, not content bytes.** Entry values are `(Hash, Size)` pointers into `iroh-blobs`. You need an `iroh-blobs` store to actually store and retrieve content.

Synchronisation uses **range-based set reconciliation** (based on [this paper](https://arxiv.org/abs/2212.13567)) over QUIC streams. Live sync is coordinated via `iroh-gossip`.

**ALPN:** `b"/iroh-sync/1"` (exported as `iroh_docs::ALPN`)

### 4.2 Identity & Capability Model

#### `Author` / `AuthorId`

An author is an ed25519 signing key used to prove authorship. `AuthorId` is the 32-byte public key.

```rust
let author = Author::new(&mut rng);
let author_id: AuthorId = author.id();  // Safe to share
// Author contains the secret key — treat as sensitive
```

#### `NamespaceSecret` / `NamespaceId`

A namespace key authorises writes to a document. `NamespaceId` is the 32-byte public key.

```rust
let namespace = NamespaceSecret::new(&mut rng);
let namespace_id: NamespaceId = namespace.id();
```

#### Capability

```rust
pub enum Capability {
    Write(NamespaceSecret),  // Read and write
    Read(NamespaceId),      // Read/sync only
}
```

**⚠️ `NamespaceId` alone is NOT write access.** Possessing `NamespaceSecret` grants write access. Sharing a write-mode `DocTicket` shares the namespace secret permanently — there is no revocation mechanism.

**⚠️ Clock skew:** timestamps are wall-clock microseconds since Unix epoch. A node with a fast clock will produce entries that appear "newer" than entries from a slow-clock node even if they were written later in real time.

### 4.3 Data Model

#### `RecordIdentifier`

Composite key: `NamespaceId (32B) || AuthorId (32B) || Key (variable)`.

**Critical implication:** two different authors writing to the same key produce **two distinct entries**. `(AuthorId, Key)` is the unique key. This is intentional — untrusted authors cannot shadow trusted authors' entries.

#### `Record` & `Entry`

```rust
pub struct Record {
    len: u64,       // Content size in bytes
    hash: Hash,     // BLAKE3 hash of the content (iroh-blobs Hash)
    timestamp: u64, // Microseconds since Unix epoch
}

// Entry = RecordIdentifier + Record
// SignedEntry = Entry + namespace signature + author signature
```

**Deletion is a tombstone.** `doc.del()` inserts an empty entry (hash = EMPTY, size = 0). This tombstone replicates like any other entry. Query with `include_empty: false` to skip deletion markers.

### 4.4 Setup

```rust
use iroh_docs::protocol::Docs;

// In-memory
let docs = Docs::memory()
    .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
    .await?;

// Persistent
let docs = Docs::persistent("./state".into())
    .spawn(endpoint.clone(), blobs_store.clone(), gossip.clone())
    .await?;
```

### 4.5 `DocsApi` — Author & Document Management

#### Author management

```rust
let author_id = docs.author_create().await?;     // Create new author
let default = docs.author_default().await?;     // Get default author
docs.author_set_default(author_id).await?;       // Set default author
let stream = docs.author_list().await?;          // List all authors
let author: Option<Author> = docs.author_export(author_id).await?;  // ⚠️ Contains secrets
docs.author_import(author).await?;
docs.author_delete(author_id).await?;
```

#### Document lifecycle

```rust
let doc: Doc = docs.create().await?;                              // New doc
let doc: Option<Doc> = docs.open(namespace_id).await?;             // Open existing
let stream = docs.list().await?;                                   // List all docs

// ⚠️ import_namespace() does NOT start sync
let doc = docs.import_namespace(capability).await?;

// import() starts sync to peers in the ticket
let doc = docs.import(ticket).await?;

// Safest: subscribe before sync — guaranteed not to miss initial events
let (doc, events) = docs.import_and_subscribe(ticket).await?;

docs.drop_doc(namespace_id).await?;  // ⚠️ Permanently deletes doc and keys
```

**⚠️ `DocsApi::open()` return type:** returns `Result<Option<Doc>>`, but the implementation always constructs `Some(Doc)` on success. The `None` path is a backend/RPC failure, not a logical "not found." Treat not-found as an error from the backend, not a meaningful `None` value.

### 4.6 `Doc` — Per-Document API

#### Writing

```rust
// set_bytes: imports value into blobs, then creates a signed entry
let hash = doc.set_bytes(author_id, b"my-key", b"my-value").await?;

// set_hash: references an existing blob (already imported into blobs)
doc.set_hash(author_id, b"my-key", hash, size).await?;

// Deletion: inserts tombstone for matching prefix
let removed = doc.del(author_id, b"prefix/").await?;
```

#### Reading

```rust
// Exact lookup
let entry: Option<Entry> = doc.get_exact(author_id, b"my-key", false).await?;

// Query with builder
let stream = doc.get_many(Query::key_prefix("config/").build()).await?;
let entry: Option<Entry> = doc.get_one(Query::single_latest_per_key().build()).await?;
```

**To read the actual content bytes**, use the content hash with `iroh-blobs`:

```rust
let entry = doc.get_exact(author_id, b"my-key", false).await?.unwrap();
let content: Bytes = blobs_store.get_bytes(entry.content_hash()).await?;
```

#### Sync & Sharing

```rust
// Start syncing
doc.start_sync(vec![peer_addr]).await?;

// Stop syncing
doc.leave().await?;

// Share (creates a DocTicket)
let ticket: DocTicket = doc.share(ShareMode::Read, AddrInfoOptions::RelayAndAddresses).await?;
// or ShareMode::Write for write access

// Subscribe to live events
let mut events = doc.subscribe().await?;
while let Some(Ok(event)) = events.next().await {
    match event {
        LiveEvent::InsertLocal { entry } => { }
        LiveEvent::InsertRemote { from, entry, content_status } => { }
        LiveEvent::ContentReady { hash } => { }           // Blob downloaded
        LiveEvent::PendingContentReady => { }             // All queued downloads done/failed
        LiveEvent::NeighborUp(peer) => { }
        LiveEvent::NeighborDown(peer) => { }
        LiveEvent::SyncFinished(ev) => { }
    }
}
```

**⚠️ `PendingContentReady` does not guarantee blobs persist forever.** It means the current queued download work has finished or failed. Blobs can still be garbage-collected if no tags reference them.

### 4.7 `DocTicket` — Capability + Peer Addresses

```rust
let ticket = DocTicket::new(capability, vec![peer_addr]);
let s = ticket.to_string();  // "docaaaa..." base32 string
let t: DocTicket = s.parse()?;
```

A `DocTicket` contains both the capability (rights) and the initial peer addresses. This is both "what can I do?" and "who should I connect to first?"

**⚠️ Write tickets contain the `NamespaceSecret`.** Treat them as secrets.

### 4.8 Query API

```rust
Query::all().build()
Query::author(author_id).build()
Query::key_exact("my-key").build()
Query::key_prefix("config/").build()

// ⚠️ For each unique key, returns only the entry with the highest timestamp.
// Key filtering happens BEFORE grouping; author filtering happens AFTER.
Query::single_latest_per_key()
    .key_prefix("config/")
    .sort_direction(SortDirection::Desc)
    .limit(100)
    .offset(50)
    .include_empty()  // Include deletion markers
    .build()
```

### 4.9 Download Policy

Separates metadata sync from blob content download.

```rust
// Download everything (default)
doc.set_download_policy(DownloadPolicy::EverythingExcept(vec![])).await?;

// Download nothing by default, except specific prefixes
doc.set_download_policy(DownloadPolicy::NothingExcept(vec![
    FilterKind::Prefix(Bytes::from("important/")),
    FilterKind::Exact(Bytes::from("config")),
])).await?;
```

---

## 5. Key Gotchas & Sharp Edges

### Blob retention is tag-driven, not write-once

Storing a blob does not mean it persists. Untagged blobs are GC-eligible. Always hold a `Tag` or `TempTag` for data you care about.

### `get_bytes()` is convenient but dangerous for large blobs

The source explicitly warns this can exhaust memory. Use for small blobs, metadata, and hash sequences only.

### `reader()` does not auto-fetch missing chunks

Attempting to read parts of a blob that are not locally present will error. It does not trigger a download. Use `remote.fetch()` or the `Downloader` to ensure completeness first.

### `observe()` is underused

Streaming bitfield updates is ideal for resumable downloads, progress tracking, and debugging partial state.

### Range export rounds up to chunk boundaries

If exact byte slicing matters, clip the output to the requested range after export.

### Gossip lag is terminal, not advisory

`Lagged` closes the subscription. Design your receiver to recreate on `Lagged`.

### `delivered_from` in gossip is the last-mile peer, not the author

If you need authorship, sign at the application layer.

### Topic IDs are not auth boundaries

Anyone who knows a topic ID and can reach peers can try to speak. Add your own auth/signature layer.

### Timestamps are wall-clock, not logical

Clock skew between nodes causes unexpected ordering. This matters for multi-node deployments.

### `import_namespace()` ≠ sync

Creating a doc handle locally does not start network sync. Use `import(ticket)` or `import_and_subscribe(ticket)` to begin sync.

### Write capability = the namespace secret

Sharing a write-mode `DocTicket` permanently grants write access. There is no revocation.

### `import_file()` / `export_file()` on `Doc` are not yet in the RPC protocol

These are commented out in the current source. See `docs/_internal/iroh-docs/src/api.rs`.

---

## 6. Python Bindings Status

> Last verified against `bindings/aster_rs/src/` and `tests/python/`.

### Available in Python

| Feature | Python method | Status |
|---|---|---|
| In-memory node | `IrohNode.memory()` | ✅ |
| Add blob bytes | `blobs_client(node).add_bytes(data)` | ✅ |
| Read blob bytes | `blobs_client(node).read_to_bytes(hash)` | ✅ |
| Gossip subscribe | `gossip_client(node).subscribe(topic, peer_ids)` | ✅ |
| Gossip broadcast | `topic.broadcast(data)` | ✅ |
| Gossip recv | `topic.recv()` → `(event_type, data)` | ✅ |
| Docs create doc | `docs_client(node).create()` | ✅ |
| Docs create author | `docs_client(node).create_author()` | ✅ |
| Docs set bytes | `doc.set_bytes(author, key, value)` | ✅ |
| Docs get exact | `doc.get_exact(author, key)` | ✅ |
| Docs share/join | `doc.share(mode)`, `docs_client.join(ticket)` | ✅ |

### Not yet available in Python

| Feature | Notes |
|---|---|
| Filesystem blob store | Only in-memory store exposed |
| `import_file()` / `export_file()` on `Doc` | Stubbed out in RPC protocol |
| `import_and_subscribe()` | Not yet bound |
| `Doc.subscribe()` (live events) | Not yet bound |
| `DownloadPolicy` | Not yet bound |
| `Downloader` / multi-provider downloads | Not yet bound |
| `Remote` API | Not yet bound |
| `BlobTicket` parsing | Not yet bound |
| `DocTicket` parsing | Not yet bound |
| Tags API | Not yet bound |
| `Batch` API | Not yet bound |
| `observe()` | Not yet bound |
| `start_sync()` / `leave()` | Not yet bound |

The Python examples (`gossip_chat.py`, `sendme_send.py`) demonstrate the currently supported surface.

---

*Consolidated from Claude and ChatGPT API analysis, April 2026. Corrections applied against upstream Rust source. To report inaccuracies, use `/reportbug`.*
