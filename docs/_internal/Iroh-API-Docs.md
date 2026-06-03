# Iroh API Reference

> **Sources:** `iroh` (v1.0.0-rc.1), `iroh-blobs` (v0.102.0), `iroh-gossip` (v0.100.0), `iroh-docs` (v0.100.0), plus this workspace's pinned Aster forks.
> Verified against the local Cargo checkout on 2026-06-02.
>
> This document mainly covers the three core Iroh data crates, plus selected endpoint-level `iroh` API notes that matter when building applications on top of them. It is source-driven — every claim is traceable to the upstream Rust source. Where behaviour is ambiguous from rustdoc alone, the source is cited.
>
> **Python bindings note:** This document describes the upstream Rust API surface. Not every method is yet exposed in the Python bindings (`bindings/aster_rs/`). See the [Python Bindings Status](#python-bindings-status) appendix for what is currently available.

---

## Table of Contents

1. [Mental Model: How the Three Crates Fit Together](#1-mental-model-how-the-three-crates-fit-together)
   - [Endpoint incoming local addresses](#11-endpoint-incoming-local-addresses)
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

### 1.1 Endpoint incoming local addresses

Incoming connections no longer report the local address as only an IP address.
Use `IncomingLocalAddr` so direct IP, relay, and custom transports are handled
without assuming every accepted request arrived on a local socket address.

```rust
let incoming = endpoint.accept().await.context("no incoming")?;

match incoming.local_addr() {
    IncomingLocalAddr::Ip(ip) => {
        println!("direct IP, local ip: {ip:?}");
    }
    IncomingLocalAddr::Relay { url } => {
        println!("via relay {url}");
    }
    IncomingLocalAddr::Custom(addr) => {
        println!("via custom transport: {addr:?}");
    }
    _ => {}
}
```

This matters for server-side diagnostics, policy, and telemetry. Code that
stores or logs `local_addr()` should preserve the enum shape instead of
downcasting it to `SocketAddr` or `IpAddr`.

---

## 2. iroh-blobs

### 2.1 Overview

`iroh-blobs` is a content-addressed data transfer and storage system built on BLAKE3 verified streaming (bao). It provides:

- A protocol for streaming content-addressed data transfer with verification at 16 KiB chunk granularity.
- Pluggable store backends: in-memory (`MemStore`), filesystem-backed (`FsStore`), and read-only memory (`ReadonlyMemStore`).
- A `BlobsProtocol` handler for serving blobs over QUIC connections.
- A `Downloader` for fetching blobs from multiple remote providers with automatic failover.
- `BlobTicket` for self-contained, serialisable fetch tickets.

**ALPN:** `b"/iroh-bytes/4"` (exported as `iroh_blobs::ALPN`)

The concepts that are easiest to conflate are:

| Concept | What it tells you | What it does not tell you |
|---|---|---|
| `Hash` | Which exact bytes you want | Where to fetch them or whether they are retained locally |
| `BlobFormat` | Whether the hash is a single blob or a hash-sequence root | Whether the children are present |
| `Tag` / `TempTag` | What local content should be protected from GC | Who can fetch it remotely |
| `EndpointAddr` / `EndpointId` | Which provider node to contact | Which blob to ask for |

Blobs have no built-in access-control layer. A hash lets a peer request content from a provider that has it, and BLAKE3 verification ensures the bytes match the hash. Applications that need authorization must decide which peers are allowed to connect, which hashes to reveal, or which requests to serve.

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

A `Hash` is an integrity identity, not a location and not a retention handle. If you only have a hash, you still need a provider address or some provider-discovery mechanism, and the local store still needs a tag/temp tag if you want the data protected from garbage collection.

#### `BlobFormat`

```rust
pub enum BlobFormat {
    Raw,     // A single opaque blob
    HashSeq, // A blob whose content is a sequence of 32-byte child hashes
}
```

- `Raw` — a plain blob.
- `HashSeq` — the blob's content is a packed sequence of child hashes. This is how collections, directories, and multi-blob transfers work. When you tag a `HashSeq`, all referenced children are also protected from GC.

`HashSeq` is a manifest-like blob: the root blob contains child hashes. Fetching or retaining only the root is not always the same as having all child content. APIs that treat `HashSeq` recursively need the root plus the referenced child blobs.

#### `HashAndFormat`

A `(Hash, BlobFormat)` pair. The hash alone is ambiguous — the same hash could be a raw blob or a hash-sequence root. Always pass `HashAndFormat` to APIs that need to resolve content.

```rust
pub struct HashAndFormat {
    pub hash: Hash,
    pub format: BlobFormat,
}
```

Use `HashAndFormat` when the API needs to know whether `HashSeq` should be interpreted recursively. For example, downloader support for `HashAndFormat { format: HashSeq }` turns into a request for the root and its children; a plain `Hash` request means a single raw blob.

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

The default `.await` path is intentionally retention-friendly: it waits for import to finish, creates an auto-named persistent tag, drops the temporary import protection, and returns `TagInfo { name, hash, format }`.

If you call `.temp_tag().await`, the blob is protected only while that `TempTag` handle is alive. Dropping the temp tag makes the content GC-eligible unless a persistent tag or another temp tag also points at it. This is useful for short-lived downloads and batches, but it is the wrong choice for user data you expect to survive restarts.

If you call `.stream().await`, you are taking responsibility for consuming progress items and handling the final `Done(TempTag)` item. Make sure the resulting hash/format is tagged before dropping the temp tag if the content should remain in the store.

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

Persistent tags live in the store. Temp tags are process-scoped handles tracked by the running store actor. A temp tag is enough to prevent GC during an import/download pipeline, but it is not a durable reference.

For `HashSeq`, tag the `HashAndFormat` root with `format: BlobFormat::HashSeq`, not just `HashAndFormat::raw(root_hash)`. The GC root needs the format to know whether to walk child hashes.

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

`Remote` assumes you already have a connected provider. The usual shape is:

```rust
let conn = endpoint.connect(peer_addr, iroh_blobs::ALPN).await?;
let stats = store.remote().fetch(conn, hash_and_format).await?;
```

If you only have a hash, `Remote` cannot discover a provider. You need an `EndpointAddr`, an `EndpointId` that your endpoint can resolve, a `BlobTicket`, or a higher-level provider-discovery layer.

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

Providers are `EndpointId`s returned by a `ContentDiscovery` implementation. A vector is accepted for simple cases, and `Shuffled` randomises provider order. The downloader's connection pool still needs the local `Endpoint` to be able to resolve/connect those ids; a raw blob hash is not enough.

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

### 2.10 `BlobTicket` — Hash + Format + Provider Address

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

Conceptually:

```rust
pub struct BlobTicket {
    addr: EndpointAddr,
    format: BlobFormat,
    hash: Hash,
}
```

A `BlobTicket` answers three questions:

- which provider should I try first? (`EndpointAddr`)
- which bytes should I request? (`Hash`)
- should this be treated as a single blob or a recursive hash sequence? (`BlobFormat`)

It does not prove permission, reserve capacity on the provider, or keep the provider's copy alive. The serving node still needs `BlobsProtocol` registered on its router, and the content must still be present and retained in that node's store.

Common ways to consume a ticket:

```rust
// High-level downloader path. Works when the endpoint can resolve the ticket's provider id.
let downloader = store.downloader(&endpoint);
downloader
    .download(ticket.hash_and_format(), Some(ticket.addr().id))
    .await?;

// Direct remote path. Uses the address info embedded in the ticket.
let conn = endpoint.connect(ticket.addr().clone(), iroh_blobs::ALPN).await?;
store.remote().fetch(conn, ticket.hash_and_format()).await?;
```

### 2.11 Common Blobs Flows

#### Add bytes and keep them

```rust
let tag = store.blobs().add_slice(b"hello").await?;
let content = tag.hash_and_format();
```

Awaiting the add creates a persistent tag. Keep the returned `TagInfo` or at least the hash/format if you want to share or fetch the content later.

#### Add bytes only for a short pipeline

```rust
let temp = store.blobs().add_slice(b"scratch").temp_tag().await?;
let content = temp.hash_and_format();
// Content is GC-protected until `temp` is dropped.
```

Create a persistent tag before dropping `temp` if the data should survive.

#### Serve content to other nodes

```rust
let tag = store.blobs().add_path("/path/to/file").await?;

let blobs = BlobsProtocol::new(&store, None);
let router = Router::builder(endpoint.clone())
    .accept(iroh_blobs::ALPN, blobs)
    .spawn();

let ticket = BlobTicket::new(endpoint.addr(), tag.hash, tag.format);
```

The tag keeps the bytes alive. The router makes the blob protocol reachable. The ticket gives another node the provider address, hash, and format.

#### Fetch when you only have a hash

```rust
let content = HashAndFormat::raw(hash);
let conn = endpoint.connect(peer_addr, iroh_blobs::ALPN).await?;
store.remote().fetch(conn, content).await?;
```

The missing piece is the provider. A hash by itself is enough to verify bytes, but not enough to find a node that has them.

---

## 3. iroh-gossip

### 3.1 Overview

`iroh-gossip` is a topic-based pub-sub system implementing two protocols:

- **HyParView** (membership): maintains a partial view of peers per topic. Default: 5 active connections, 30 passive peers. Self-heals on node failure.
- **PlumTree** (broadcast): eager push to nearby peers, lazy push (IHave/Graft) to others, with automatic tree optimisation by latency.

Messages are broadcast to all peers subscribed to a topic. Each topic is an independent swarm with independent membership.

**⚠️ Topic ID is rendezvous, not authorisation.** Possessing a topic ID is effectively enough to join and speak unless your application adds its own policy or signature layer.

**ALPN:** `b"/iroh-gossip/1"` (exported as `iroh_gossip::ALPN`)

Gossip is intentionally ephemeral. It does not persist messages, replay missed history, or prove authorship. Treat it as a live signal plane: useful for chat-like fan-out, liveness, invalidation hints, and telling peers that something changed elsewhere.

The identities to keep separate are:

| Identity | What it identifies | What it does not do |
|---|---|---|
| `TopicId` | The swarm/topic to join | Authenticate speakers or discover peers by itself |
| `EndpointId` | A peer in the Iroh network | Say which topics that peer is subscribed to |
| `EndpointAddr` | Addressing info for contacting a peer | Authorize gossip messages |
| Application author key | Your app's message author | Provided by you, not by gossip |

If you only have a topic id, you know what room you want, but not who can introduce you to the room. Bootstrap peers or later `join_peers(...)` calls provide the initial peer ids.

### 3.2 Core Types

#### `TopicId`

A 32-byte topic identifier. Create random topics with `TopicId::from_bytes(rand::random())`.

A `TopicId` is both the rendezvous name and the broadcast scope. It is not a secret by construction and it is not an ACL. If topic membership should be private, protect the topic id and authenticate payloads at the application layer.

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

`Broadcast` enters the PlumTree swarm and can travel beyond your direct neighbors. `BroadcastNeighbors` is one hop only. `JoinPeers` asks the membership layer to connect/add more peers for this topic; it is not a message broadcast.

### 3.3 Setup and Lifecycle

```rust
use iroh_gossip::{Gossip, ALPN, TopicId};

let gossip = Gossip::builder()
    .max_message_size(4096)  // Default: 4096 bytes; use the same value across peers
    .spawn(endpoint.clone());

let router = Router::builder(endpoint)
    .accept(ALPN, gossip.clone())
    .spawn();
```

The maximum message size applies to postcard-encoded gossip frames and should be the same across a topic network. If a frame is larger than the configured limit, the lower-level read/write path errors rather than delivering a partial message.

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

What these calls do:

- `subscribe(...)` creates a local topic handle immediately. It does not wait for any bootstrap peer to be reachable.
- `subscribe_and_join(...)` calls `subscribe(...)` and then waits until at least one `NeighborUp` event has been observed.
- `subscribe_with_opts(...)` is the same join path with explicit bootstrap ids and subscription channel capacity.
- `bootstrap_peer_ids` are `EndpointId`s. Your `Endpoint` still needs a way to resolve/connect them, for example from known addresses, relay lookup, or another address-discovery mechanism.

`joined().await` consumes events until a neighbor is present. In particular, the first `NeighborUp` used to satisfy the wait is not returned later by the stream, but the receiver's `neighbors()` set is updated.

### 3.5 Sending and Receiving

`GossipTopic` is both a sender and a `Stream<Item = Result<Event, ApiError>>`.

```rust
let mut topic = gossip.subscribe_and_join(topic_id, peers).await?;

// Send
topic.broadcast(Bytes::from("hello")).await?;
topic.broadcast_neighbors(Bytes::from("local-only")).await?;

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

There is no replay buffer for late subscribers. If a peer was offline or not yet joined when a message was broadcast, gossip does not fetch old messages for it later. Store durable state in `iroh-docs` or your own storage, and use gossip as the live notification path.

### 3.6 Splitting Sender and Receiver

```rust
let (sender, receiver) = topic.split();
// sender: GossipSender — Clone, Send, Sync
// receiver: GossipReceiver — Stream<Item = Result<Event, ApiError>>

// Topic is alive until BOTH halves are dropped
```

Use this to broadcast from one task while receiving in another.

Dropping a `GossipTopic` leaves the topic. If you split it, the topic remains alive until both the sender and receiver halves are dropped.

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
sender.broadcast(signed).await?;

// Verify on receive
let (from, message) = SignedMessage::verify_and_decode(&msg.content)?;
```

### 3.9 Common Gossip Flows

#### Join a topic when you know one peer

```rust
let mut topic = gossip
    .subscribe_and_join(topic_id, vec![bootstrap_peer_id])
    .await?;

topic.broadcast(Bytes::from("hello")).await?;
```

This waits until at least one direct neighbor is connected before returning. It does not prove that the whole swarm is connected, only that this local subscription has joined at least one peer.

#### Subscribe immediately and handle connection later

```rust
let mut topic = gossip.subscribe(topic_id, vec![bootstrap_peer_id]).await?;

if !topic.is_joined() {
    topic.joined().await?;
}
```

Use this when you want a handle immediately, but remember that outbound messages are only queued until a connection exists and queues are finite.

#### Add peers after subscribing

```rust
let (sender, _receiver) = topic.split();
sender.join_peers(vec![new_peer_id]).await?;
```

`join_peers(...)` updates the topic mesh. It does not send your application payload to those peers by itself.

#### Recover from lag

```rust
loop {
    let mut topic = gossip
        .subscribe_and_join(topic_id, bootstrap_peer_ids.clone())
        .await?;

    while let Some(event) = topic.next().await {
        match event? {
            Event::Lagged => break, // resubscribe and reconcile durable state
            Event::Received(message) => { /* handle live message */ }
            _ => {}
        }
    }
}
```

After `Lagged`, assume you missed live messages. If the content matters, reconcile against durable state in `iroh-docs` or another store after resubscribing.

---

## 4. iroh-docs

### 4.1 Overview

`iroh-docs` is a replicated key-value store where each document (called a "replica") is identified by a cryptographic namespace keypair. Entries are keyed by `(NamespaceId, AuthorId, Key)` — meaning multiple authors can write to the same key, and each author's version is retained independently.

**Critical: docs stores metadata, not content bytes.** Entry values are `(Hash, Size)` pointers into `iroh-blobs`. You need an `iroh-blobs` store to actually store and retrieve content.

Synchronisation uses **range-based set reconciliation** (based on [this paper](https://arxiv.org/abs/2212.13567)) over QUIC streams. Live sync is coordinated via `iroh-gossip`.

**ALPN:** `b"/iroh-sync/1"` (exported as `iroh_docs::ALPN`)

The three identities that newcomers most often mix up are:

| Identity | What it identifies | What possession gives you |
|---|---|---|
| `NamespaceId` | A document/replica | Read/sync identity only; not contact info and not write access |
| `NamespaceSecret` | The document write key | Permanent write authority for that document |
| `AuthorId` | The author of an entry | A public signing identity; the secret `Author` key is needed to write as that author |

A fourth identity, `EndpointId` / `EndpointAddr`, belongs to the networking layer. It tells Iroh which node to contact. A `NamespaceId` tells Iroh *which document* you mean, but not *which peer* can supply it.

### 4.2 Identity & Capability Model

#### `Author` / `AuthorId`

An author is an ed25519 signing key used to prove authorship. `AuthorId` is the 32-byte public key.

```rust
let author = Author::new(&mut rng);
let author_id: AuthorId = author.id();  // Safe to share
// Author contains the secret key — treat as sensitive
```

Docs entries are signed twice: once by the namespace authority and once by the author. The namespace signature proves the entry belongs to a document. The author signature proves which application/user identity wrote that entry.

Every `Docs` node also has a **default author**. This is a convenience author used by applications that do not want to manage multiple authors manually. It is not the node's network identity and it is not the document namespace. For persistent `Docs`, the default author's id is saved in the docs data directory and the author secret is stored in the docs store. For in-memory `Docs`, a fresh default author is created for that process.

#### `NamespaceSecret` / `NamespaceId`

A namespace key authorises writes to a document. `NamespaceId` is the 32-byte public key.

```rust
let namespace = NamespaceSecret::new(&mut rng);
let namespace_id: NamespaceId = namespace.id();
```

A `NamespaceId` is enough to name a document and create a read capability. It is not enough to write. A `NamespaceSecret` is enough to write and can derive the matching `NamespaceId`.

#### Capability

```rust
pub enum Capability {
    Write(NamespaceSecret),  // Read and write
    Read(NamespaceId),      // Read/sync only
}
```

**⚠️ `NamespaceId` alone is NOT write access.** Possessing `NamespaceSecret` grants write access. Sharing a write-mode `DocTicket` shares the namespace secret permanently — there is no revocation mechanism.

If you already know what material you have, create the capability directly:

```rust
// I only know the document id: read/sync only
let cap = Capability::Read(namespace_id);

// I have the document secret: read and write
let cap = Capability::Write(namespace_secret);
```

The capability answers "what rights do I have for this document?" It does not answer "which peer should I call?" For that, you need `EndpointAddr` values, usually carried inside a `DocTicket`.

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
let default = docs.author_default().await?;      // Get default author
docs.author_set_default(author_id).await?;       // Set default author
let stream = docs.author_list().await?;          // List all authors
let author: Option<Author> = docs.author_export(author_id).await?;  // ⚠️ Contains secrets
docs.author_import(author).await?;
docs.author_delete(author_id).await?;
```

`author_default()` returns the node-wide default docs author. It is the signing identity used by simple examples before calling `doc.set_bytes(...)` or `doc.set_hash(...)`.

Under the hood:

- `Docs::persistent(...)` stores the default author id in a `default-author` file under the docs state directory and keeps the matching author secret in the docs store. On restart, it reloads that same default author.
- `Docs::memory()` creates a fresh default author for the process.
- `author_set_default(...)` fails if the author is not already imported into the local docs store.
- `author_delete(...)` refuses to delete the current default author.

#### Document lifecycle

```rust
let doc: Doc = docs.create().await?;                  // New local doc with a new namespace secret
let doc: Option<Doc> = docs.open(namespace_id).await?; // Open doc already known locally
let stream = docs.list().await?;                      // List locally known docs

// ⚠️ import_namespace() does NOT start sync
let doc = docs.import_namespace(capability).await?;

// import() starts sync to peers in the ticket
let doc = docs.import(ticket).await?;

// Safest: subscribe before sync — guaranteed not to miss initial events
let (doc, events) = docs.import_and_subscribe(ticket).await?;

docs.drop_doc(namespace_id).await?;  // ⚠️ Permanently deletes doc and keys
```

What these calls do:

- `create()` generates a fresh `NamespaceSecret`, stores the write capability locally, and returns an open `Doc` handle. It does not connect to any peers because no peers are known yet.
- `open(namespace_id)` opens a document that is already in the local store. It does not import a capability and it does not start sync.
- `list()` lists local documents known to this docs node. It is not a network query.
- `drop_doc(namespace_id)` deletes the local document state, including the document secret key and entries. Blob content may still exist if it is referenced or tagged elsewhere.

**⚠️ `DocsApi::open()` return type:** returns `Result<Option<Doc>>`, but the implementation always constructs `Some(Doc)` on success. The `None` path is a backend/RPC failure, not a logical "not found." Treat not-found as an error from the backend, not a meaningful `None` value.

#### Importing, opening, and syncing

`import_namespace(capability)` is the lower-level "install these rights locally" call. The `capability` type is `iroh_docs::Capability`:

```rust
let read_cap = Capability::Read(namespace_id);
let write_cap = Capability::Write(namespace_secret);

let doc = docs.import_namespace(read_cap).await?;
```

Under the hood, `import_namespace(...)` stores the capability in the local namespace table. If the same namespace was already known as read-only and you import a write capability, the stored capability is upgraded from `Read` to `Write`. If the namespace is already open, the open state is updated with the stronger capability. It returns an open `Doc` handle, but it does **not** contact the network.

`import(ticket)` is the "join from a share link" call:

```rust
let doc = docs.import(ticket).await?;
```

A `DocTicket` contains both:

- a `Capability`, which says whether you may read or write
- a `Vec<EndpointAddr>`, which says which peers to contact first

`import(ticket)` is effectively:

```rust
let doc = docs.import_namespace(ticket.capability).await?;
doc.start_sync(ticket.nodes).await?;
```

`import_and_subscribe(ticket)` follows the same shape but subscribes before starting sync, so the caller can observe the initial remote inserts instead of racing them.

Common "what do I have?" flows:

```rust
// I have a full DocTicket from another peer.
let doc = docs.import(ticket).await?;

// I only have the NamespaceId and at least one peer address.
// This gives read/sync access, then starts sync with that peer.
let doc = docs.import_namespace(Capability::Read(namespace_id)).await?;
doc.start_sync(vec![peer_addr]).await?;

// I have the NamespaceSecret and at least one peer address.
// This gives write access locally, then starts sync.
let doc = docs.import_namespace(Capability::Write(namespace_secret)).await?;
doc.start_sync(vec![peer_addr]).await?;

// I only have the NamespaceId or NamespaceSecret, but no peer address.
// You can import/open the doc locally, but no remote data will arrive until
// your application learns an EndpointAddr or receives a DocTicket.
let doc = docs.import_namespace(Capability::Read(namespace_id)).await?;
```

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

Every write needs an `AuthorId`. For simple single-author applications, call `docs.author_default().await?` once and reuse that id. The author controls the `(AuthorId, Key)` part of the entry identity; the document namespace controls whether the entry is valid for this document.

`set_bytes(...)` first imports the value bytes into the configured `iroh-blobs` store, then writes a docs entry containing the blob hash and size. `set_hash(...)` skips the byte import and records an existing blob hash, so the blob must already be available through your blob store or fetched later by download policy.

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

Conceptually:

```rust
pub struct DocTicket {
    capability: Capability,
    nodes: Vec<EndpointAddr>,
}
```

`doc.share(mode, addr_options)` creates a `DocTicket` for the current doc:

- `ShareMode::Read` creates `Capability::Read(doc.id())`.
- `ShareMode::Write` exports the document namespace secret and creates `Capability::Write(secret)`.
- The ticket's node list includes this node's endpoint address, filtered by `AddrInfoOptions`.
- The share path starts sync for the local document before returning the ticket.

If you already have the pieces, you can construct the same idea manually:

```rust
let ticket = DocTicket::new(Capability::Read(namespace_id), vec![peer_addr]);
let ticket = DocTicket::new(Capability::Write(namespace_secret), vec![peer_addr]);
```

Parsing an encoded ticket rejects empty addressing info. If you have a capability but no peers, use `import_namespace(...)` and start sync later once your application discovers an `EndpointAddr`.

### 4.8 Common Docs Flows

#### Create a local document and write a value

```rust
let doc = docs.create().await?;
let author = docs.author_default().await?;

doc.set_bytes(author, b"config/name", b"alice").await?;
```

#### Share a document

```rust
let read_ticket = doc
    .share(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
    .await?;

let write_ticket = doc
    .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
    .await?;
```

Prefer read tickets for untrusted peers. A write ticket gives away the namespace secret.

#### Join from a ticket

```rust
let doc = docs.import(ticket).await?;
```

This stores the capability and starts sync with the peers embedded in the ticket.

#### Join from a namespace id and explicit peer address

```rust
let doc = docs.import_namespace(Capability::Read(namespace_id)).await?;
doc.start_sync(vec![peer_addr]).await?;
```

This is equivalent to joining with a read ticket, except your application supplied the peer address out of band.

#### Reopen after restart

```rust
if let Some(doc) = docs.open(namespace_id).await? {
    // The namespace was already known to this local docs store.
}
```

For persistent docs, this works only after the namespace capability was previously created or imported locally. It does not rediscover peers by namespace id.

### 4.9 Query API

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

### 4.10 Download Policy

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

### Incoming local address is not always an IP

`incoming.local_addr()` returns `IncomingLocalAddr`, not a bare socket address.
Match `Ip`, `Relay`, and `Custom` explicitly if connection path matters.

### Blob retention is tag-driven, not write-once

Storing a blob does not mean it persists. Untagged blobs are GC-eligible. Always hold a `Tag` or `TempTag` for data you care about.

### A blob hash is identity, not discovery

A `Hash` verifies bytes, but it does not say which peer has them. To fetch, you need a provider address/id that your endpoint can connect to, a `BlobTicket`, or an application-level provider-discovery mechanism.

### `BlobTicket` is not an authorization token

A blob ticket carries provider address, hash, and format. It does not grant permission in a cryptographic sense and it does not keep the provider's blob alive. Protect sensitive hashes and enforce access policy outside `iroh-blobs`.

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

### Gossip has no history replay

A late or disconnected subscriber does not receive old messages. If the state matters, store it somewhere durable and use gossip only as the live notification path.

### `delivered_from` in gossip is the last-mile peer, not the author

If you need authorship, sign at the application layer.

### Topic IDs are not auth boundaries

Anyone who knows a topic ID and can reach peers can try to speak. Add your own auth/signature layer.

### Topic IDs are not peer discovery

A topic id tells gossip which swarm to join, but not which peer can introduce you. You still need bootstrap `EndpointId`s and an endpoint/address-lookup path that can connect to them.

### Timestamps are wall-clock, not logical

Clock skew between nodes causes unexpected ordering. This matters for multi-node deployments.

### `import_namespace()` ≠ sync

Importing a capability creates or upgrades local document state and returns a doc handle. It does not contact peers. Use `import(ticket)`, `import_and_subscribe(ticket)`, or `doc.start_sync(vec![peer_addr])` to begin network sync.

### `NamespaceId` is identity, not discovery

A namespace id names a document. It does not tell Iroh which node has the document, and it does not contain relay/direct addressing information. If you have only a namespace id, you still need an `EndpointAddr` from a ticket, rendezvous service, config file, QR code, or some other application-level channel.

### Write capability = the namespace secret

Sharing a write-mode `DocTicket` permanently grants write access. There is no revocation.

### Default author is not endpoint identity

`docs.author_default()` returns the default docs signing author. It is unrelated to the node's `EndpointId` and unrelated to the document's `NamespaceId`. Use it when you need an `AuthorId` for writes, not when you need to connect to peers.

### `DocTicket` is the easiest join format because it carries both halves

A ticket carries the capability plus initial peer addresses. A raw `NamespaceId` or `NamespaceSecret` carries only rights/identity, so your application must supply peer addresses separately.

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

*Consolidated from Claude and ChatGPT API analysis, April 2026. Newcomer detail pass verified against local Rust source, June 2026. To report inaccuracies, use `/reportbug`.*
