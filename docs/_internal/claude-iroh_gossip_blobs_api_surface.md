# Iroh Primitives API Reference

**Covers:** `iroh-blobs` (content-addressed blob storage & transfer) and `iroh-gossip` (topic-based pub-sub)

**Source versions:** `iroh-blobs-main`, `iroh-gossip-main` (as uploaded 2026-04-03)

-----

## 1. iroh-blobs

### 1.1 Overview

iroh-blobs is a content-addressed data transfer and storage system built on BLAKE3 verified streaming. It provides:

- A protocol for streaming content-addressed data transfer using BLAKE3 verified streaming (bao).
- Pluggable store backends: in-memory (`MemStore`), filesystem-backed (`FsStore`), and read-only memory (`ReadonlyMemStore`).
- A `BlobsProtocol` handler that plugs into iroh’s `Router` for serving blobs over QUIC connections.
- A `Downloader` for fetching blobs from multiple remote providers with automatic failover and parallel splitting.
- `BlobTicket` for self-contained, serialisable capability tokens.

**ALPN:** `b"/iroh-blobs/0"` (exported as `iroh_blobs::ALPN`)

### 1.2 Core Types

#### `Hash`

A 32-byte BLAKE3 hash. This is the universal content identifier.

```rust
pub struct Hash(blake3::Hash);

impl Hash {
    pub const EMPTY: Hash;                       // Hash of b""
    pub fn new(buf: impl AsRef<[u8]>) -> Self;   // Compute hash
    pub fn as_bytes(&self) -> &[u8; 32];
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub fn to_hex(&self) -> String;
    pub fn fmt_short(&self) -> ArrayString<10>;  // First 5 bytes, hex
}
```

Implements `Copy`, `Eq`, `Hash`, `Serialize`, `Deserialize`, `FromStr` (hex), `Display` (hex).

#### `BlobFormat`

```rust
pub enum BlobFormat {
    Raw,      // A single blob
    HashSeq,  // A sequence of BLAKE3 hashes (collection/directory)
}
```

- `Raw` — a single opaque byte blob.
- `HashSeq` — the blob’s content is a sequence of 32-byte BLAKE3 hashes, each referencing another blob. This is how collections, directories, and multi-blob transfers work.

#### `HashAndFormat`

A `(Hash, BlobFormat)` pair. Used throughout as the canonical content identifier — the hash alone is ambiguous without knowing whether it’s raw or a hash sequence.

```rust
pub struct HashAndFormat {
    pub hash: Hash,
    pub format: BlobFormat,
}
```

Converts from `Hash` (defaults to `Raw`) and from `(Hash, BlobFormat)`.

### 1.3 Store Backends

All store backends `Deref` to `api::Store`, so every method on `Store`, `Blobs`, `Tags`, `Remote` is callable directly on the store.

#### `MemStore`

In-memory store. Good for tests, ephemeral data, and small datasets.

```rust
use iroh_blobs::store::mem::MemStore;

let store = MemStore::new();
// or with options (e.g. GC config):
let store = MemStore::new_with_opts(Options { gc_config: Some(gc), ..Default::default() });
```

**Note:** Spawns a background actor on creation. Data is lost when dropped.

#### `FsStore`

Persistent hybrid store: small blobs inline in a `redb` database, large blobs as files on disk. This is the production store.

```rust
use iroh_blobs::store::fs::FsStore;

let store = FsStore::load("path/to/blobs.db").await?;
// or with options:
let store = FsStore::load_with_opts(path, Options { ... }).await?;
```

**Requires** feature `fs-store`. Uses a hybrid approach: blobs ≤16 KiB are stored inline in the database; larger blobs get data and outboard files on disk. This gives good performance for both millions of tiny blobs and multi-GB files.

#### `ReadonlyMemStore`

For serving a fixed, known set of blobs without mutation.

### 1.4 The `Store` API (entry point)

`Store` is the unified client API. You reach it via `Deref` from any store backend, or from `BlobsProtocol`.

```rust
pub struct Store { /* ... */ }

impl Store {
    pub fn blobs(&self) -> &Blobs;          // Blob import/export/read API
    pub fn tags(&self) -> &Tags;            // Tag management API
    pub fn remote(&self) -> &Remote;        // Single-node download API
    pub fn downloader(&self, endpoint: &Endpoint) -> Downloader; // Multi-node download

    pub async fn sync_db(&self) -> RequestResult<()>;  // Flush DB
    pub async fn shutdown(&self) -> irpc::Result<()>;   // Shutdown store actor
    pub async fn wait_idle(&self) -> irpc::Result<()>;  // Wait for background work to finish

    // RPC (requires feature "rpc")
    pub fn connect(endpoint: noq::Endpoint, addr: SocketAddr) -> Self;
    pub async fn listen(self, endpoint: noq::Endpoint);
}
```

`Store` also `Deref`s to `Blobs`, so all `Blobs` methods are directly available on `Store`.

### 1.5 `Blobs` API — Import, Export, Read

The primary API for interacting with blob content.

#### Adding data

```rust
// From a byte slice (copies)
let tag_info = store.add_slice(b"hello world").await?;

// From Bytes (zero-copy if already Bytes)
let tag_info = store.add_bytes(my_bytes).await?;

// From a file path
let tag_info = store.add_path("/path/to/file").await?;

// With explicit options (format, import mode)
let progress = store.add_path_with_opts(AddPathOptions {
    path: "/path/to/file".into(),
    format: BlobFormat::Raw,
    mode: ImportMode::Copy,   // Copy or TryReference
});
let tag_info = progress.await?;  // IntoFuture → TagInfo

// From an async byte stream
let progress = store.add_stream(byte_stream).await;
let tag_info = progress.await?;
```

All add methods return an `AddProgress` which:

- Implements `IntoFuture` → `TagInfo` (awaiting consumes progress, creates a persistent tag).
- Can be consumed as `.temp_tag()` → `TempTag` (GC-safe handle, no persistent tag).
- Can be consumed as `.stream()` → `Stream<Item = AddProgressItem>` for progress tracking.

**`ImportMode`**: `Copy` duplicates the file into the store. `TryReference` attempts to reference the original file in place (only on `FsStore`, with reflink support).

#### Reading data

```rust
// Get all bytes (careful with large blobs — loads into memory)
let bytes: Bytes = store.get_bytes(hash).await?;

// AsyncRead + AsyncSeek reader
let mut reader = store.reader(hash);
reader.read_to_string(&mut buf).await?;

// Export BAO-encoded ranges (verified streaming)
let progress = store.export_bao(hash, ChunkRanges::all());
let data = progress.data_to_bytes().await?;

// Export specific byte ranges (zero-copy random access)
let progress = store.export_ranges(hash, 0..1024u64);

// Export to a file path
store.export(hash, "/path/to/output").await?;
```

#### Querying

```rust
// Check if a blob exists and is complete
let exists: bool = store.has(hash).await?;

// Get detailed status
let status: BlobStatus = store.status(hash).await?;
// BlobStatus::Complete { size } | BlobStatus::Partial { .. } | BlobStatus::NotFound

// Observe the bitfield (which chunks are present)
let bitfield: Bitfield = store.observe(hash).await?;

// List all blobs
let mut list = store.list();
// list is a stream of (Hash, BlobStatus)
```

#### Batched operations

Batch provides a scope where added data is protected from GC until the batch is dropped.

```rust
let batch = store.batch().await?;
let tt1 = batch.add_bytes(b"data1").await?;   // → TempTag
let tt2 = batch.add_slice(b"data2").await?;
// tt1 and tt2 are GC-protected while batch lives
// Dropping batch releases the protection scope
```

### 1.6 `Tags` API — Named References to Blobs

Tags are named, persistent references to `HashAndFormat` values. A blob is protected from garbage collection as long as at least one tag (or temp tag) points to it.

```rust
let tags = store.tags();

// Create an auto-named tag
let tag: Tag = tags.create(hash_and_format).await?;

// Set a named tag
tags.set("my-data", HashAndFormat { hash, format: BlobFormat::Raw }).await?;

// Get a tag value
let info: Option<TagInfo> = tags.get("my-data").await?;
// TagInfo { name: Tag, hash: Hash, format: BlobFormat }

// List all tags
let mut stream = tags.list().await?;
while let Some(Ok(info)) = stream.next().await { /* ... */ }

// List by prefix
let stream = tags.list_prefix("project-").await?;

// List hash_seq tags only
let stream = tags.list_hash_seq().await?;

// Delete
let deleted_count = tags.delete("my-data").await?;
tags.delete_prefix("temp-").await?;
tags.delete_all().await?;   // ⚠️ All data becomes GC-eligible

// Rename atomically
tags.rename("old-name", "new-name").await?;

// Temp tags (GC-safe handles without persistent names)
let tt: TempTag = tags.temp_tag(hash_and_format).await?;
// Protected until tt is dropped
```

**Important:** Blobs are garbage-collected when no tags or temp tags reference them. Never rely on blobs persisting without a tag.

### 1.7 `Remote` API — Single-Node Downloads

For downloading from a single known remote node.

```rust
let remote = store.remote();

// Check what we have locally for a given content
let info: LocalInfo = remote.local(HashAndFormat { hash, format }).await?;
info.is_complete();      // bool
info.local_bytes();      // u64 — bytes present locally
info.children();         // Option<u64> — child count for hash sequences
let missing = info.missing();  // → GetRequest for only the missing parts

// Fetch content, taking local data into account
let stats = remote.fetch(connection, HashAndFormat { hash, format }).await?;

// Execute a specific get request
let stats = remote.execute_get(connection, get_request).await?;

// Observe remote bitfield
let stream = remote.observe(connection, hash);

// Push a blob to a remote node
let stats = remote.push(connection, content).await?;
```

`GetProgress` (returned by `fetch`) implements `IntoFuture → GetResult<Stats>` and can also be consumed as `.stream()` for progress tracking.

### 1.8 `Downloader` — Multi-Node Downloads

The `Downloader` fetches from multiple providers with automatic failover, parallel chunk splitting, and connection pooling.

```rust
let downloader = store.downloader(&endpoint);

// Simple download from a list of providers
downloader.download(hash, vec![node_id_1, node_id_2]).await?;

// Download with options (splitting, custom request)
downloader.download_with_opts(DownloadOptions::new(
    GetRequest::all(root_hash),         // Full hash sequence
    Shuffled::new(vec![id1, id2, id3]), // Randomised provider order
    SplitStrategy::Split,               // Parallel per-child downloads
)).await?;
```

**`ContentDiscovery` trait:** Any `Debug + Clone + IntoIterator<Item: Into<EndpointId>>` implements it, so `Vec<EndpointId>` works directly. For randomised order, use `Shuffled::new(nodes)`.

**`SplitStrategy`:**

- `None` — try providers sequentially for the whole request.
- `Split` — for hash sequences, split into per-child requests and download in parallel from different providers.

### 1.9 `BlobsProtocol` — Serving Over iroh

The integration point for serving blobs on an iroh `Router`.

```rust
use iroh::{protocol::Router, Endpoint, endpoint::presets};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN};

let store = MemStore::new();
let blobs = BlobsProtocol::new(&store, None);  // None = no event sender

let endpoint = Endpoint::bind(presets::N0).await?;
let router = Router::builder(endpoint)
    .accept(ALPN, blobs.clone())
    .spawn();

// blobs.store() gives access to the Store API
let tag = blobs.store().add_slice(b"hello").await?;
```

`BlobsProtocol` implements iroh’s `ProtocolHandler` trait. It `Deref`s to `Store`.

The optional `EventSender` parameter allows observing transfer events (progress, completion, errors) for monitoring/UI.

### 1.10 `BlobTicket` — Capability Tokens

A self-contained token encoding everything needed to fetch a blob: provider address, hash, and format. Serialises as a base32 string with a `blob` prefix.

```rust
use iroh_blobs::ticket::BlobTicket;

// Create
let ticket = BlobTicket::new(endpoint_addr, hash, BlobFormat::Raw);

// Serialise / deserialise
let s = ticket.to_string();    // "blobaaaa..."
let t: BlobTicket = s.parse()?;

// Inspect
ticket.hash();               // Hash
ticket.format();             // BlobFormat
ticket.addr();               // &EndpointAddr
ticket.recursive();          // true if HashSeq
let (addr, hash, fmt) = ticket.into_parts();
```

### 1.11 Key Considerations & Gotchas

**Garbage collection.** Blobs without tags are GC-eligible. Always hold a `Tag` or `TempTag` for data you care about. The GC runs periodically based on `GcConfig`.

**Verified streaming (BAO).** Every blob has an associated outboard containing the BLAKE3 hash tree. Transfers are verified at 16 KiB granularity — corruption is detected within one chunk group. This means partial downloads are safe and resumable.

**Hash sequences.** A `HashSeq` blob is a sequence of 32-byte hashes. It’s how collections and directories work. When you tag a hash sequence, all referenced child blobs are also protected from GC. Fetching a hash sequence fetches the root and all children.

**Hybrid storage (FsStore).** Blobs ≤16 KiB go inline in the `redb` database. Larger blobs get data + outboard files on disk. This threshold is configurable but the defaults are well-tuned. The consequence: millions of tiny blobs are fast (no filesystem metadata overhead), and large blobs are fast (direct file I/O, no database bottleneck).

**Import modes.** `ImportMode::Copy` is always safe. `ImportMode::TryReference` (reflink/hardlink) avoids copying but only works on FsStore with filesystem support. If reflink fails it falls back to copy.

**Bitfields.** `Bitfield` tracks which chunks of a blob are locally present. For complete blobs this is `ChunkRanges::all()`. For partial downloads it’s a sparse set. The `observe()` API streams bitfield updates as chunks arrive.

**Connection reuse.** The `Downloader` maintains a `ConnectionPool` that reuses QUIC connections to the same provider. If you’re doing many downloads from the same set of nodes, create one `Downloader` and reuse it.

-----

## 2. iroh-gossip

### 2.1 Overview

iroh-gossip is a topic-based pub-sub system implementing two well-known protocols:

- **HyParView** (membership): maintains a partial view of peers in each topic’s swarm. Default: 5 active connections, 30 passive peers. Self-heals on node failure.
- **PlumTree** (broadcast): eager push to nearby peers, lazy push (IHave) to others, with automatic tree optimization by latency.

Messages are broadcast to all peers subscribed to a topic. Each topic is an independent swarm with independent membership.

**ALPN:** `b"/iroh-gossip/1"` (exported as `iroh_gossip::ALPN`)

### 2.2 Core Types

#### `TopicId`

A 32-byte topic identifier. Topics are independent swarms.

```rust
pub struct TopicId([u8; 32]);

impl TopicId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self;
    pub fn as_bytes(&self) -> &[u8; 32];
    pub fn fmt_short(&self) -> String;  // First 5 bytes, hex
}
```

Implements `Copy`, `Eq`, `Hash`, `Serialize`, `Deserialize`, `Display` (hex), `FromStr` (hex).

Create random topics with `TopicId::from_bytes(rand::random())`.

#### `Event`

Events received from a gossip topic subscription.

```rust
pub enum Event {
    NeighborUp(EndpointId),      // New direct neighbor connected
    NeighborDown(EndpointId),    // Direct neighbor disconnected
    Received(Message),           // Gossip message received
    Lagged,                      // Subscription fell behind; messages dropped
}
```

#### `Message`

A received gossip message.

```rust
pub struct Message {
    pub content: Bytes,               // The message payload
    pub scope: DeliveryScope,         // How the message arrived
    pub delivered_from: EndpointId,   // The peer that delivered it (not necessarily the author)
}
```

#### `DeliveryScope`

```rust
pub enum DeliveryScope {
    Swarm(Round),    // Received via gossip tree; Round = hop count from broadcaster
    Neighbors,       // Received from a direct neighbor broadcast
}

impl DeliveryScope {
    pub fn is_direct(&self) -> bool;  // true if Neighbors or Swarm(Round(0))
}
```

#### `Command`

Commands sent to a gossip topic.

```rust
pub enum Command {
    Broadcast(Bytes),              // Broadcast to entire swarm
    BroadcastNeighbors(Bytes),     // Broadcast to direct neighbors only
    JoinPeers(Vec<EndpointId>),    // Connect to additional peers
}
```

### 2.3 `Gossip` — The Protocol Handler

Created via a builder pattern, spawns a background actor.

```rust
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};

// Default configuration
let gossip = Gossip::builder().spawn(endpoint.clone());

// Custom configuration
let gossip = Gossip::builder()
    .max_message_size(8192)                  // Default: 4096 bytes
    .membership_config(HyparviewConfig {
        active_view_capacity: 10,            // Default: 5
        passive_view_capacity: 60,           // Default: 30
        shuffle_interval: Duration::from_secs(120),
        ..Default::default()
    })
    .broadcast_config(PlumtreeConfig {
        message_cache_retention: Duration::from_secs(120),
        ..Default::default()
    })
    .alpn(b"/my-custom-gossip/1")            // Default: b"/iroh-gossip/1"
    .spawn(endpoint.clone());
```

**Key methods on `Gossip`:**

```rust
impl Gossip {
    pub fn builder() -> Builder;
    pub fn max_message_size(&self) -> usize;
    pub async fn handle_connection(&self, conn: Connection) -> Result<(), Error>;
    pub async fn shutdown(&self) -> Result<(), Error>;
    pub fn metrics(&self) -> &Arc<Metrics>;
}
```

`Gossip` implements iroh’s `ProtocolHandler` and `Deref<Target = GossipApi>`, so all `GossipApi` methods are callable directly.

**Registering with a Router:**

```rust
let router = Router::builder(endpoint)
    .accept(GOSSIP_ALPN, gossip.clone())
    .spawn();
```

### 2.4 `GossipApi` — Topic Subscription

The API for joining topics and sending/receiving messages. Accessible via `Deref` from `Gossip`, or over RPC.

```rust
// Subscribe and wait for at least one peer connection
let topic: GossipTopic = gossip
    .subscribe_and_join(topic_id, vec![peer_id_1, peer_id_2])
    .await?;

// Subscribe without waiting (returns immediately)
let topic = gossip
    .subscribe(topic_id, vec![peer_id_1])
    .await?;

// Subscribe with options
let topic = gossip
    .subscribe_with_opts(topic_id, JoinOptions {
        bootstrap: [peer_id].into_iter().collect(),
        subscription_capacity: 4096,    // Default: 2048
    })
    .await?;
```

### 2.5 `GossipTopic` — Working with a Subscribed Topic

`GossipTopic` is both a sender and a `Stream<Item = Result<Event, ApiError>>`.

```rust
let mut topic = gossip.subscribe_and_join(topic_id, peers).await?;

// Send a message to the entire swarm
topic.broadcast(Bytes::from("hello")).await?;

// Send only to direct neighbors
topic.broadcast_neighbors(Bytes::from("local-only")).await?;

// Check connection state
topic.is_joined();                           // bool
topic.neighbors().collect::<Vec<_>>();       // Current direct neighbors

// Wait until connected to at least one peer
topic.joined().await?;

// Receive events (GossipTopic is a Stream)
while let Some(event) = topic.try_next().await? {
    match event {
        Event::Received(msg) => {
            println!("got {} bytes from {}", msg.content.len(), msg.delivered_from);
        }
        Event::NeighborUp(id) => println!("peer joined: {}", id),
        Event::NeighborDown(id) => println!("peer left: {}", id),
        Event::Lagged => println!("subscription lagged, messages dropped"),
    }
}
```

#### Splitting sender and receiver

```rust
let (sender, receiver) = topic.split();

// sender: GossipSender — Clone, Send, Sync
// receiver: GossipReceiver — Stream<Item = Result<Event, ApiError>>

// Topic stays active until BOTH halves are dropped

// Sender methods
sender.broadcast(msg).await?;
sender.broadcast_neighbors(msg).await?;
sender.join_peers(vec![new_peer_id]).await?;

// Receiver methods
receiver.joined().await?;
receiver.is_joined();
receiver.neighbors();
```

This split is essential when you need to broadcast from one task while receiving in another.

### 2.6 Configuration Reference

#### `HyparviewConfig` (Membership Layer)

|Field                       |Default|Description                                |
|----------------------------|-------|-------------------------------------------|
|`active_view_capacity`      |5      |Number of active peer connections per topic|
|`passive_view_capacity`     |30     |Size of passive peer address book          |
|`active_random_walk_length` |6      |Hops for ForwardJoin to reach active view  |
|`passive_random_walk_length`|3      |Hops for ForwardJoin to reach passive view |
|`shuffle_random_walk_length`|6      |Hops for Shuffle propagation               |
|`shuffle_active_view_count` |3      |Active peers included in Shuffle           |
|`shuffle_passive_view_count`|4      |Passive peers included in Shuffle          |
|`shuffle_interval`          |60s    |Interval between Shuffle rounds            |
|`neighbor_request_timeout`  |500ms  |Timeout for Neighbor requests              |

#### `PlumtreeConfig` (Broadcast Layer)

|Field                    |Default |Description                                          |
|-------------------------|--------|-----------------------------------------------------|
|`graft_timeout_1`        |500ms   |Timeout before requesting a missing message via Graft|
|`graft_timeout_2`        |250ms   |Timeout for Graft retry from next peer               |
|`dispatch_timeout`       |200ms   |Delay before batching IHave messages                 |
|`optimization_threshold` |Round(1)|Hop difference needed to promote lazy→eager peer     |
|`message_cache_retention`|60s     |How long messages stay in the internal cache         |

### 2.7 Key Considerations & Gotchas

**Message size limit.** Default max message size is **4096 bytes**. Exceeding this silently drops the message. For larger payloads, use iroh-blobs and gossip the hash/ticket instead.

**Lagged subscribers.** If a `GossipReceiver` isn’t polled fast enough, the internal channel fills up (default capacity: 2048 events). When full, the subscriber receives a `Lagged` event, messages are dropped, and the stream closes. You must resubscribe if this happens.

**Topic lifetime.** A topic subscription is alive as long as the `GossipTopic` (or both `GossipSender` + `GossipReceiver` after split) is alive. Dropping it leaves the topic and disconnects from peers for that topic.

**Bootstrap peers.** You must provide at least one bootstrap peer’s `EndpointId` to join an existing swarm. If you provide none, you create a new (lonely) swarm and must wait for others to join you. The endpoint addresses must be resolvable by iroh’s address lookup (via `MemoryLookup`, DNS, DHT, or mDNS).

**Message integrity.** Messages are identified by their BLAKE3 hash internally (for deduplication and IHave/Graft). The protocol does **not** authenticate message origin — if you need author verification, sign messages at the application layer (as the chat example demonstrates).

**Broadcast vs. BroadcastNeighbors.** `broadcast()` propagates to the entire swarm via the PlumTree protocol. `broadcast_neighbors()` sends only to direct neighbors (1 hop) — useful for protocol-level control messages that shouldn’t propagate.

**No persistence.** Gossip state is entirely in-memory. Topic membership, message cache, and peer views are lost on restart. Reconnecting requires re-bootstrapping.

**Connection initiation.** The gossip actor initiates connections to peers autonomously. You don’t need to manually dial peers — just provide bootstrap `EndpointId`s and ensure the address lookup can resolve them.

-----

## 3. Aster-Relevant Usage Patterns

### 3.1 iroh-blobs for Large Artifact Transfer

Per the Aster spec (§5.9), services return `FileRef`/`BlobTicket` capabilities instead of inlining large payloads. The flow:

```rust
// Provider side
let tag = store.add_path("/path/to/model.bin").await?;
let ticket = BlobTicket::new(endpoint.addr(), tag.hash, tag.format);
// Return ticket in RPC response

// Consumer side
let ticket: BlobTicket = /* from RPC response */;
let downloader = store.downloader(&endpoint);
downloader.download(ticket.hash(), vec![ticket.addr().id]).await?;
let bytes = store.get_bytes(ticket.hash()).await?;
```

### 3.2 iroh-gossip for Registry Change Notifications

Per the Aster spec (§11.7), gossip carries low-latency hints about registry changes. Consumers always reconcile against iroh-docs before acting.

```rust
// Publisher
let topic_id = registry_gossip_topic;  // From _aster/config/gossip_topic
let mut topic = gossip.subscribe_and_join(topic_id, bootstrap).await?;

// On contract publication:
let event = serde_json::to_vec(&GossipEvent {
    r#type: "CONTRACT_PUBLISHED",
    contract_id: Some(contract_id),
    ..
})?;
topic.broadcast(event.into()).await?;

// Consumer
let mut topic = gossip.subscribe_and_join(topic_id, bootstrap).await?;
while let Some(Ok(Event::Received(msg))) = topic.next().await {
    let event: GossipEvent = serde_json::from_slice(&msg.content)?;
    // Reconcile against iroh-docs before updating routing
}
```

### 3.3 Message Size Constraint

Gossip messages are limited to 4096 bytes by default. For Aster’s `GossipEvent` payloads this is more than adequate (they’re small JSON notifications). If a registry needs to broadcast larger data, bump `max_message_size` in the builder, or (preferably) gossip a reference and fetch the payload via iroh-blobs.