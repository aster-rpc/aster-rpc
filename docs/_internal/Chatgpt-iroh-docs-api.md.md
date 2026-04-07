# iroh-gossip, iroh-blobs, and iroh-docs API surface analysis

Analyzed from the uploaded source archives rather than generated rustdoc:

- `iroh-gossip` `0.97.0` — commit `9a30bbb4a9dee83dce43d1a4412acf3d3b6b1acd`
- `iroh-blobs` `0.99.0` — commit `8e0fa7afddd35f6344cf3e23d2045cf614ef568b`
- `iroh-docs` `0.97.0` — commit `6e1d6c3e2900cbc247ec6ed70a838cf0b89be7be`

This write-up is source-driven and tries to answer the practical question: **“what do I actually call, in what order, and what important behavior is only obvious from the source?”**

---

# 1. Mental model first

## `iroh-gossip`

A **topic-based swarm broadcast layer** on top of `iroh` connections.

You give peers:

- the same `TopicId`
- bootstrap peer IDs
- the gossip protocol mounted on the same ALPN on an `iroh::protocol::Router`

Then peers can:

- join the swarm for that topic
- receive neighbor up/down events
- broadcast bytes to the whole swarm or just direct neighbors

Important: **topic knowledge is basically rendezvous, not authorization**. If somebody knows the topic and can reach peers, your app should assume they can try to speak on it unless you add your own auth/signature/policy layer.

## `iroh-blobs`

A **content-addressed local store + transfer protocol**.

Three layers matter:

1. **Local store API** — add/read/export/list/tag/observe blobs.
2. **Network protocol handler** — `BlobsProtocol` serves the store over `iroh` connections.
3. **Download APIs** — fetch from one or many peers and store results locally.

The store is BLAKE3/BAO-based, so downloaded bytes are verified as they stream.

## `iroh-docs`

A **replicated multi-dimensional key/value metadata layer** built on top of:

- `iroh-gossip` for live notifications / mesh awareness
- `iroh-blobs` for content storage and transfer
- a docs sync protocol for reconciliation

Critical architectural point:

- **docs stores metadata entries**, not the content bytes themselves
- an entry points to content by **blob hash + size**
- document writes require both:
  - a **namespace capability** (`NamespaceSecret` for write or `NamespaceId` for read)
  - an **author key** to sign authorship

This is why `NamespaceId` alone is not write access.

---

# 2. How the three crates fit together

```rust
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol, ALPN as BLOBS_ALPN};
use iroh_docs::{protocol::Docs, ALPN as DOCS_ALPN};
use iroh_gossip::{Gossip, ALPN as GOSSIP_ALPN};

let endpoint = Endpoint::bind(presets::N0).await?;

let blobs = MemStore::new();
let gossip = Gossip::builder().spawn(endpoint.clone());
let docs = Docs::memory()
    .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
    .await?;

let router = Router::builder(endpoint)
    .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
    .accept(GOSSIP_ALPN, gossip)
    .accept(DOCS_ALPN, docs)
    .spawn();
```

Rough responsibilities:

- `iroh-gossip`: membership + ephemeral broadcast
- `iroh-blobs`: verified content bytes
- `iroh-docs`: replicated metadata / queries / live sync

---

# 3. `iroh-gossip` API surface

## Main types you will actually use

- `Gossip`
- `GossipApi`
- `GossipTopic`
- `GossipSender`
- `GossipReceiver`
- `TopicId`
- `Event`
- `Message`
- `JoinOptions`
- `ALPN`

The public entrypoint is effectively `Gossip::builder().spawn(endpoint)`.

## Minimal setup

```rust
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_gossip::{Gossip, TopicId, api::Event};
use n0_future::StreamExt;

let endpoint = Endpoint::bind(presets::N0).await?;
let gossip = Gossip::builder().spawn(endpoint.clone());

let router = Router::builder(endpoint)
    .accept(iroh_gossip::ALPN, gossip.clone())
    .spawn();

let topic = TopicId::from_bytes([7u8; 32]);
let bootstrap = vec![];

let (sender, mut receiver) = gossip.subscribe(topic, bootstrap).await?.split();
receiver.joined().await?;
sender.broadcast(b"hello".to_vec().into()).await?;

while let Some(ev) = receiver.next().await {
    match ev? {
        Event::Received(msg) => {
            println!("from {:?}: {:?}", msg.delivered_from, msg.content);
        }
        Event::NeighborUp(id) => println!("up: {id}"),
        Event::NeighborDown(id) => println!("down: {id}"),
        Event::Lagged => println!("receiver fell behind"),
    }
}
```

## `Gossip::builder()` and builder knobs

Builder methods in the source:

- `max_message_size(usize)`
- `membership_config(HyparviewConfig)`
- `broadcast_config(PlumtreeConfig)`
- `alpn(impl AsRef<[u8]>)`
- `spawn(endpoint)`

Practical notes:

- default max message size is **4096 bytes**
- the protocol hard-rejects absurdly small values via internal config validation
- if you set a custom ALPN, every peer and your router registration must match it exactly
- HyParView/PlumTree are the actual membership/broadcast algorithms underneath

The source comments in `proto.rs` also make clear that default HyParView behavior assumes an active view around **5** peers and passive view around **30**, which matters for connection fan-out and recovery behavior.

## `GossipApi`

This is what you call after spawning. `Gossip` derefs to `GossipApi`, so these methods are callable directly on `Gossip`.

### Join methods

- `subscribe(topic_id, bootstrap)`
- `subscribe_and_join(topic_id, bootstrap)`
- `subscribe_with_opts(topic_id, JoinOptions)`

Behavior from source:

### `subscribe(...)`

Returns immediately with a `GossipTopic`.

It **does not wait** until any peer is actually connected.

### `subscribe_and_join(...)`

Same as `subscribe`, but then waits until there is at least one neighbor.

This is the easiest “don’t continue until I’m actually attached to the swarm” call.

### `subscribe_with_opts(...)`

Lets you choose:

- bootstrap peers
- per-subscription event buffer capacity

Use this if you care about lag/drop behavior.

## `JoinOptions`

```rust
pub struct JoinOptions {
    pub bootstrap: BTreeSet<EndpointId>,
    pub subscription_capacity: usize,
}
```

Helper:

- `JoinOptions::with_bootstrap(iter)`

Source-level behavior worth knowing:

- default `subscription_capacity` is **2048** events
- if the receiver falls behind badly enough to overflow that channel:
  - you get an `Event::Lagged`
  - that lagged message was already dropped
  - the subscription is then closed

That means `Lagged` is not a harmless warning. Treat it as “this receiver should usually be recreated”.

## `GossipTopic`

A combined send/receive handle.

Methods:

- `split() -> (GossipSender, GossipReceiver)`
- `broadcast(Bytes)`
- `broadcast_neighbors(Bytes)`
- `neighbors()`
- `joined().await`
- `is_joined()`

And it is also a `Stream<Item = Result<Event, ApiError>>`.

### Important semantics

Dropping the topic leaves the swarm for that topic.

If you split it, the leave only happens once **both** halves are dropped.

## `GossipSender`

Methods:

- `broadcast(Bytes)`
- `broadcast_neighbors(Bytes)`
- `join_peers(Vec<EndpointId>)`

`join_peers` is useful when you discover more peers later and want to fan out without leaving/re-subscribing.

## `GossipReceiver`

Methods:

- `neighbors()`
- `joined().await`
- `is_joined()`

And it is a stream of events.

Important source detail:

- `joined().await` internally consumes events until the first `NeighborUp`
- that means the first `NeighborUp` will **not** still be waiting for you on the stream afterwards
- if you care about current neighbors after `joined()`, call `neighbors()` and then keep tracking future up/down events

## `Event` and `Message`

`Event` variants:

- `NeighborUp(EndpointId)`
- `NeighborDown(EndpointId)`
- `Received(Message)`
- `Lagged`

`Message` fields:

- `content: Bytes`
- `scope: DeliveryScope`
- `delivered_from: EndpointId`

Important nuance:

- `delivered_from` is the peer that handed you the message
- it is **not** necessarily the original author

If authorship matters, sign your payload yourself.

## Practical usage pattern for real apps

The example chat app in the source signs and verifies every gossip payload itself. That is a good model:

1. serialize your app message
2. sign it with an endpoint or app identity
3. broadcast bytes
4. verify on receipt before trusting contents

That is especially important because topic IDs are not an auth boundary.

## Things easy to miss

- Obtaining bootstrap peers is **out of band**; gossip does not solve that for you.
- Joining multiple topics increases connection count and routing table size.
- `broadcast_neighbors` is direct-neighbor only; useful for local coordination patterns.
- If you never progress the receiver stream, you will eventually lag out.
- `subscribe(...)` can queue messages before the first connection is up, but that queue is finite.

## Gossip API inventory

### Primary handles

- `Gossip::builder()`
- `Gossip::shutdown()`
- `Gossip::max_message_size()`
- `Gossip::metrics()`

### Join / subscribe

- `GossipApi::subscribe(...)`
- `GossipApi::subscribe_and_join(...)`
- `GossipApi::subscribe_with_opts(...)`

### Send / receive

- `GossipTopic::broadcast(...)`
- `GossipTopic::broadcast_neighbors(...)`
- `GossipTopic::neighbors()`
- `GossipTopic::joined()`
- `GossipTopic::is_joined()`
- `GossipTopic::split()`
- `GossipSender::broadcast(...)`
- `GossipSender::broadcast_neighbors(...)`
- `GossipSender::join_peers(...)`
- `GossipReceiver::neighbors()`
- `GossipReceiver::joined()`
- `GossipReceiver::is_joined()`

---

# 4. `iroh-blobs` API surface

## Main types you will actually use

High-level:

- `store::mem::MemStore`
- `store::fs::FsStore`
- `api::Store`
- `api::blobs::Blobs`
- `api::tags::Tags`
- `api::downloader::Downloader`
- `BlobsProtocol`
- `ticket::BlobTicket`
- `Hash`
- `BlobFormat`
- `HashAndFormat`
- `TempTag`

Lower-level but useful:

- `api::remote::Remote`
- `get::request::get_blob(...)`
- `protocol::{GetRequest, GetManyRequest, ObserveRequest, PushRequest}`

## Store types and how to get a `Store`

### In-memory store

```rust
use iroh_blobs::store::mem::MemStore;

let store = MemStore::new();
```

`MemStore` derefs to `api::Store`, so you can call `store.blobs()`, `store.tags()`, etc.

### Filesystem store

```rust
use iroh_blobs::store::fs::FsStore;

let store = FsStore::load("./blobs").await?;
```

This is the persistent store.

## `api::Store`

Top-level accessors:

- `tags()`
- `blobs()`
- `remote()`
- `downloader(&Endpoint)`
- `sync_db()`
- `shutdown()`
- `wait_idle()`

Key idea:

- `Store` is the umbrella API
- `store.blobs()` is local blob operations
- `store.tags()` is named/temporary retention and lookup
- `store.remote()` is low-level one-connection remote execution
- `store.downloader(endpoint)` is the high-level multi-provider fetcher

## Serving blobs over the network

```rust
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol};

let endpoint = Endpoint::bind(presets::N0).await?;
let store = MemStore::new();
let blobs = BlobsProtocol::new(&store, None);

let router = Router::builder(endpoint)
    .accept(iroh_blobs::ALPN, blobs)
    .spawn();
```

`BlobsProtocol` derefs to the same `Store`, so it is both protocol handler and convenient handle to the local store.

## `BlobTicket`

This is the easiest bootstrap object for blob transfer.

It contains:

- provider `EndpointAddr`
- hash
- format

Main methods:

- `BlobTicket::new(addr, hash, format)`
- `hash()`
- `addr()`
- `format()`
- `hash_and_format()`
- `recursive()`
- `into_parts()`

`recursive()` is true for `HashSeq`, meaning “this points at a sequence/collection style object rather than just one raw blob”.

## `Hash`, `BlobFormat`, `HashAndFormat`

These are core identifiers.

- `Hash` = BLAKE3 hash
- `BlobFormat::Raw` = ordinary blob
- `BlobFormat::HashSeq` = a blob that is itself a sequence of child hashes
- `HashAndFormat` = both together

The source makes `HashSeq` central for “collection” behavior.

## `Blobs` API: the local blob operations you’ll use most

## Reading

- `reader(hash)`
- `reader_with_opts(...)`
- `get_bytes(hash)`
- `status(hash)`
- `has(hash)`
- `list()`

### Reader behavior

`reader(hash)` returns a `BlobReader` implementing `AsyncRead + AsyncSeek`.

Important: any access to parts not present locally returns an error. It does **not** fetch missing data for you.

## Adding/importing data locally

- `add_slice(data)`
- `add_bytes(data)`
- `add_bytes_with_opts(...)`
- `add_path(path)`
- `add_path_with_opts(...)`
- `add_stream(stream)`
- `batch()`

### Progress model

The add APIs return `AddProgress`.

The source guarantees these final semantics:

Known-size sources like files:

```text
Size -> CopyProgress* -> CopyDone -> OutboardProgress* -> Done
```

Unknown-size streams:

```text
CopyProgress* -> Size -> CopyDone -> OutboardProgress* -> Done
```

Final outcomes:

- `Done(TempTag)`
- `Error(io::Error)`

### Very important: `Done` gives you a `TempTag`

That temp tag protects the content from GC **for the lifetime of that tag value**.

If you want longer-lived retention:

- keep the `TempTag` alive in memory, or
- convert to / create a persistent named tag via `store.tags()`

If you drop all protection and all named tags, GC can eventually reclaim the content.

## Import modes for files

`ImportMode`:

- `Copy` — safe default
- `TryReference` — faster / less copying, but assumes the file remains unchanged

Source note: stores are allowed to ignore `TryReference` and still copy.

## Exporting / reading ranges / BAO

- `export(hash, path)`
- `export_with_opts(...)`
- `export_ranges(hash, ranges)`
- `export_ranges_with_opts(...)`
- `export_bao(hash, chunk_ranges)`
- `export_bao_with_opts(...)`
- `export_chunk(hash, offset)`

### Export progress

`export(...)` returns `ExportProgress` with events:

```text
Size -> CopyProgress* -> Done
```

### Range export gotcha

The source explicitly rounds requested ranges up to **chunk boundaries**.

So if you request `0..100`, you may actually receive data for the full first chunk. Your caller is responsible for clipping if exact byte slicing matters.

## Observing completeness

- `observe(hash)`
- `observe_with_opts(...)`

This returns `ObserveProgress`, which can either:

- be awaited for the current bitfield, or
- streamed for initial state + updates

Useful when doing incremental BAO imports or debugging whether a local store is partial vs complete.

## Importing BAO directly

- `import_bao(hash, size, local_update_cap)`
- `import_bao_with_opts(...)`
- `import_bao_reader(...)`
- `import_bao_bytes(...)`

This is the lower-level “I already have BAO chunks / verified stream pieces” path.

## Listing and status

- `list()`
- `status(hash)`
- `has(hash)`

`BlobStatus` values:

- `NotFound`
- `Partial { size: Option<u64> }`
- `Complete { size: u64 }`

## `Tags` API

This is how you retain and name content.

Methods:

- `get(name)`
- `set(name, value)`
- `create(value)`
- `rename(from, to)`
- `delete(name)`
- `delete_prefix(prefix)`
- `delete_range(...)`
- `delete_all()`
- `list()`
- `list_prefix(prefix)`
- `list_range(...)`
- `list_hash_seq()`
- `list_temp_tags()`
- `temp_tag(value)`

### Named tags vs temp tags

- **Named tags** are durable references in the store.
- **Temp tags** are in-memory liveness protection.

This is one of the most important non-obvious design points in `iroh-blobs`.

### `TempTag`

Key methods:

- `hash()`
- `format()`
- `hash_and_format()`
- `leak()`

`leak()` deliberately prevents the temp tag from decrementing its refcount on drop, effectively pinning it until process exit.

That is powerful but blunt. Usually prefer a persistent named tag if you want durable retention.

## `Downloader`: high-level fetching from one or more providers

Methods:

- `Downloader::new(store, endpoint)`
- `download(request, providers)`
- `download_with_opts(...)`

The default `download(...)` path tries providers sequentially until the request becomes complete. It re-checks local progress between providers, so partial progress from one provider is reused with the next.

Supported request inputs are broad:

- `Hash`
- `HashAndFormat`
- `GetRequest`
- `GetManyRequest`
- iterables of hashes

### Providers input

`providers` is anything implementing `ContentDiscovery`.

Practical options:

- just pass `vec![endpoint_id1, endpoint_id2]`
- or use `Shuffled::new(vec![...])`
- or implement your own `ContentDiscovery`

### Progress events

`DownloadProgressItem` includes:

- `TryProvider`
- `ProviderFailed`
- `PartComplete`
- `Progress(u64)`
- `DownloadError`
- `Error(...)`

This makes the downloader very suitable for UI / diagnostics.

### Split strategy

There is a lower-level `DownloadOptions` / `SplitStrategy` path.

- `SplitStrategy::None` — default behavior
- `SplitStrategy::Split` — split work into subrequests and run in parallel

Useful for `HashSeq` / multi-part scenarios.

## `Remote`: lower-level single-connection remote execution

Methods include:

- `local(...)`
- `local_for_request(...)`
- `fetch(...)`
- `observe(conn, request)`
- `execute_get(...)`
- `execute_get_many(...)`
- `execute_push(...)`

Use this layer if you want to control the connection and request execution yourself instead of using `Downloader`.

## Two practical fetch styles

### Easiest: downloader + ticket

```rust
let ticket: BlobTicket = ticket_str.parse()?;
let downloader = store.downloader(&endpoint);

downloader
    .download(ticket.hash_and_format(), vec![ticket.addr().id])
    .await?;
```

### Low-level: stream without storing first

The `examples/get-blob.rs` path shows direct streaming using `get::request::get_blob(connection, hash)`.

That is the right fit when you want to process bytes immediately rather than persist them into a store first.

## Batch operations

`blobs.batch()` gives a scoped batch handle. This is useful when adding several blobs whose temporary liveness should be tied together.

If you are building collections or multi-blob atomic-ish workflows, this is worth using.

## Things easy to miss

- The crate README explicitly says this version is **not yet considered production quality**, and suggests `iroh-blobs 0.35` if production quality is required.
- `HashSeq` is the building block for collection-style data.
- `reader()` does not auto-fetch missing chunks.
- `observe()` is ideal for debugging partial completion.
- `delete` on blobs is intentionally not exposed as a normal public user operation; retention is meant to be managed by tags + GC.
- `delete_all()` on tags is effectively “allow everything to be garbage-collected”.
- `TryReference` import/export modes trade safety for performance and may be ignored by the store.

## Blobs API inventory

### Top-level `Store`

- `tags()`
- `blobs()`
- `remote()`
- `downloader(&Endpoint)`
- `sync_db()`
- `shutdown()`
- `wait_idle()`

### `Blobs`

- `batch()`
- `reader()` / `reader_with_opts()`
- `add_slice()`
- `add_bytes()` / `add_bytes_with_opts()`
- `add_path()` / `add_path_with_opts()`
- `add_stream()`
- `export()` / `export_with_opts()`
- `export_ranges()` / `export_ranges_with_opts()`
- `export_bao()` / `export_bao_with_opts()`
- `export_chunk()`
- `get_bytes()`
- `observe()` / `observe_with_opts()`
- `import_bao()` / `import_bao_with_opts()` / `import_bao_reader()` / `import_bao_bytes()`
- `list()`
- `status()`
- `has()`

### `Tags`

- `get()`
- `set()` / `set_with_opts()`
- `create()` / `create_with_opts()`
- `rename()` / `rename_with_opts()`
- `delete()` / `delete_prefix()` / `delete_range()` / `delete_all()` / `delete_with_opts()`
- `list()` / `list_prefix()` / `list_range()` / `list_hash_seq()` / `list_with_opts()`
- `list_temp_tags()`
- `temp_tag()`

### Download side

- `Downloader::download()`
- `Downloader::download_with_opts()`
- `Shuffled::new(...)`
- `Remote::execute_get(...)`
- `Remote::execute_get_many(...)`
- `Remote::observe(...)`
- `Remote::fetch(...)`

---

# 5. `iroh-docs` API surface

## Main types you will actually use

High-level:

- `protocol::Docs`
- `api::DocsApi`
- `api::Doc`
- `DocTicket`
- `Capability`
- `CapabilityKind`
- `NamespaceId`
- `NamespaceSecret`
- `AuthorId`
- `Author`
- `Query`
- `DownloadPolicy`
- `engine::LiveEvent`

Important supporting value types:

- `Entry`
- `SignedEntry`
- `EntrySignature`
- `OpenState`
- `AddrInfoOptions`
- `ShareMode`

## Setup

`iroh-docs` is a meta-protocol. It is not standalone. It expects:

- an `iroh` endpoint
- a blobs store / downloader
- a gossip protocol instance
- the docs protocol mounted on the router

### In-memory docs

```rust
use iroh_docs::protocol::Docs;

let docs = Docs::memory()
    .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
    .await?;
```

### Persistent docs

```rust
use iroh_docs::protocol::Docs;

let docs = Docs::persistent("./state".into())
    .spawn(endpoint.clone(), blobs_store.clone(), gossip.clone())
    .await?;
```

Source detail for persistence:

- replica state is stored in `docs.redb`
- default author information is stored under `default-author`

## `Docs` and `DocsApi`

`Docs` derefs to `DocsApi`, so you usually just call methods directly on the `Docs` handle.

Main `DocsApi` methods:

### Author management

- `author_create()`
- `author_default()`
- `author_set_default(author_id)`
- `author_list()`
- `author_export(author_id)`
- `author_import(author)`
- `author_delete(author_id)`

### Document lifecycle

- `create()`
- `drop_doc(namespace_id)`
- `list()`
- `open(namespace_id)`
- `import_namespace(capability)`
- `import(ticket)`
- `import_and_subscribe(ticket)`

## Very important capability model

This is central to using docs correctly.

```rust
pub enum Capability {
    Write(NamespaceSecret),
    Read(NamespaceId),
}
```

So:

- `NamespaceId` = public identifier / read capability
- `NamespaceSecret` = write capability

That means:

- **knowing the namespace id is not enough to write**
- **sharing a write ticket is sensitive**, because it may contain the namespace secret

Also, docs writes are dual-signed:

- by the namespace key
- by the author key

So to produce writes you need both the document write capability **and** an author secret key available locally.

## Author management patterns

### “I just need one local identity”

Use `author_default()`.

On persistent nodes this is created on first start and reloaded later.

### “I need multiple semantic authors”

Use `author_create()` and store the returned `AuthorId`. If you need to move authors between nodes, use `author_export()` / `author_import()`.

Warning from source/comments: `Author` contains secret material.

## Creating and opening docs

### Create

```rust
let doc = docs.create().await?;
let namespace_id = doc.id();
```

### Open an existing local doc

```rust
let doc = docs.open(namespace_id).await?;
```

Source nuance:

- `open()` returns `Result<Option<Doc>>`
- but the implementation returns `Some(doc)` on success and relies on the RPC failing if the doc is missing
- so in practice treat not-found as an error path coming from the backend rather than as a meaningful `None`

## Importing docs

### Import just the capability

```rust
let doc = docs.import_namespace(capability).await?;
```

Important: this **does not start sync**.

### Import from a ticket and immediately sync

```rust
let doc = docs.import(ticket).await?;
```

This:

1. imports the capability locally
2. starts sync to the peers inside the ticket

### Import and subscribe first

```rust
let (doc, mut events) = docs.import_and_subscribe(ticket).await?;
```

The source specifically sets up the subscription **before** sync starts so that you do not miss initial live events.

That makes this the safest “join and observe” API.

## Sharing docs

`Doc::share(mode, addr_options)` returns a `DocTicket`.

`ShareMode`:

- `Read`
- `Write`

`AddrInfoOptions` controls how much addressing detail goes into the embedded `EndpointAddr` values:

- `Id`
- `RelayAndAddresses`
- `Relay`
- `Addresses`

Practical implication:

- `Id` is smallest, but assumes discovery can later resolve addresses
- `RelayAndAddresses` is the most self-contained

## `Doc` handle

This is the per-document API.

### Lifecycle

- `id()`
- `close()`

Important source detail:

- `close()` flips a local `closed` flag in the handle
- afterwards the handle’s methods will fail locally via `ensure_open()`

## Writing document entries

### Easiest: store bytes through docs

```rust
let author = docs.author_default().await?;
let hash = doc.set_bytes(author, b"settings/theme", b"dark").await?;
```

This stores content bytes and inserts the metadata entry.

### Advanced: content already exists in blobs

```rust
doc.set_hash(author, b"settings/theme", hash, size).await?;
```

Use this when you already imported content into `iroh-blobs` separately.

### File import helper

```rust
let progress = doc.import_file(
    &blobs_store,
    author,
    b"photos/headshot.jpg".to_vec().into(),
    "/abs/path/to/file.jpg",
    iroh_blobs::api::blobs::ImportMode::Copy,
).await?;

let outcome = progress.await?;
```

This helper:

1. imports the file into the blobs store
2. waits for the blob import to finish
3. inserts the corresponding doc entry with `set_hash`

That sequencing is not obvious from rustdoc, but it is exactly what the source does.

### Deletion model

```rust
doc.del(author, b"prefix/to/remove").await?;
```

Docs deletion is not “physical delete entry rows immediately”. It inserts an **empty entry / deletion marker** for that key prefix.

That means query behavior depends on whether you include empties.

## Reading and querying

### Exact lookup

- `get_exact(author, key, include_empty)`

### Query stream

- `get_many(query)`
- `get_one(query)`

### Query builder

Key constructors:

- `Query::all()`
- `Query::single_latest_per_key()`
- `Query::author(author_id)`
- `Query::key_exact(key)`
- `Query::key_prefix(prefix)`

Builder options:

- `author(...)`
- `key_exact(...)`
- `key_prefix(...)`
- `limit(...)`
- `offset(...)`
- `include_empty()`
- `sort_by(...)` for flat queries
- `sort_direction(...)` for single-latest-per-key queries
- `build()`

### Important query nuance from source

For `SingleLatestPerKey`:

- key filtering happens **before** grouping
- author filtering happens **after** grouping

That is exactly the sort of subtle behavior you want documented because it affects results in multi-author docs.

## Live sync

### Start syncing with peers

```rust
doc.start_sync(vec![peer_addr1, peer_addr2]).await?;
```

### Stop syncing

```rust
doc.leave().await?;
```

### Subscribe to events

```rust
let mut events = doc.subscribe().await?;
while let Some(ev) = events.next().await {
    match ev? {
        iroh_docs::engine::LiveEvent::InsertLocal { entry } => {}
        iroh_docs::engine::LiveEvent::InsertRemote { from, entry, content_status } => {}
        iroh_docs::engine::LiveEvent::ContentReady { hash } => {}
        iroh_docs::engine::LiveEvent::PendingContentReady => {}
        iroh_docs::engine::LiveEvent::NeighborUp(peer) => {}
        iroh_docs::engine::LiveEvent::NeighborDown(peer) => {}
        iroh_docs::engine::LiveEvent::SyncFinished(ev) => {}
    }
}
```

### `LiveEvent` variants worth watching

- `InsertLocal`
- `InsertRemote`
- `ContentReady`
- `PendingContentReady`
- `NeighborUp`
- `NeighborDown`
- `SyncFinished`

Useful interpretation:

- `InsertRemote` tells you metadata arrived
- `ContentReady` tells you referenced blob content is now local
- `PendingContentReady` tells you the queued content downloads from the last sync cycle have all finished or failed

## Download policy

Per-document methods:

- `set_download_policy(policy)`
- `get_download_policy()`

Policy types:

- `DownloadPolicy::NothingExcept(Vec<FilterKind>)`
- `DownloadPolicy::EverythingExcept(Vec<FilterKind>)`

Filters:

- `FilterKind::Prefix(Bytes)`
- `FilterKind::Exact(Bytes)`

This is a very important docs feature: metadata sync and blob-content download are separate concerns.

You can sync the document metadata while choosing which key prefixes actually trigger content download.

## Sync peer inspection and status

- `status()` returns `OpenState`
- `get_sync_peers()` returns the current sync peers if present

`OpenState` fields:

- `sync: bool`
- `subscribers: usize`
- `handles: usize`

Useful for diagnostics and admin UIs.

## `Entry`, `SignedEntry`, and signature model

`Entry` is the metadata row.

It contains:

- namespace
- author
- key
- content hash
- content length
- timestamp

`SignedEntry` wraps the entry plus:

- namespace signature
- author signature

So writes are explicitly dual-signed.

This is a strong conceptual difference from gossip, where messages are just bytes unless your app signs them.

## `DocTicket`

Contains:

- `capability: Capability`
- `nodes: Vec<EndpointAddr>`

So a docs ticket is both:

- “what rights do I have on this doc?”
- “who should I connect to first?”

Practical security note:

- a read ticket contains a `NamespaceId`
- a write ticket may contain a `NamespaceSecret`

Treat write tickets as secrets.

## Things easy to miss

- `iroh-docs` does **not** store content bytes itself.
- `import_namespace()` does not sync; `import(ticket)` does.
- write access requires `NamespaceSecret`, not just `NamespaceId`.
- producing writes also requires a local author secret.
- deletion is modeled with empty entries / tombstones.
- `import_file()` is a convenience bridge into `iroh-blobs`.
- docs live sync depends on both gossip and the docs sync protocol; it is not just “gossip all entries”.
- `PendingContentReady` does not guarantee every blob exists locally forever; it means the current queued work finished or failed.

## Docs API inventory

### `Docs` / `DocsApi`

- `author_create()`
- `author_default()`
- `author_set_default()`
- `author_list()`
- `author_export()`
- `author_import()`
- `author_delete()`
- `create()`
- `drop_doc()`
- `import_namespace()`
- `import()`
- `import_and_subscribe()`
- `list()`
- `open()`

### `Doc`

- `id()`
- `close()`
- `set_bytes()`
- `set_hash()`
- `del()`
- `get_exact()`
- `get_many()`
- `get_one()`
- `share()`
- `start_sync()`
- `leave()`
- `subscribe()`
- `status()`
- `set_download_policy()`
- `get_download_policy()`
- `get_sync_peers()`
- `import_file()`
- `export_file()`

### Supporting public types you will likely touch

- `Capability`
- `CapabilityKind`
- `NamespaceId`
- `NamespaceSecret`
- `AuthorId`
- `Author`
- `Query`
- `SortBy`
- `SortDirection`
- `DownloadPolicy`
- `FilterKind`
- `LiveEvent`
- `OpenState`
- `DocTicket`

---

# 6. Suggested “default usage” recipes

## Recipe A: simple broadcast channel

Use `iroh-gossip` only.

- bootstrap peers out of band
- `subscribe_and_join(...)`
- sign app payloads yourself
- recreate subscription on `Lagged`

## Recipe B: send files or immutable payloads

Use `iroh-blobs`.

- add content to local store
- keep/persist a tag
- serve via `BlobsProtocol`
- exchange a `BlobTicket`
- receiver downloads with `Downloader`

## Recipe C: replicated metadata pointing to blob content

Use `iroh-docs` + `iroh-blobs` + `iroh-gossip`.

- `Docs::memory()` or `Docs::persistent()`
- `author_default()`
- `create()` or `import(ticket)`
- `set_bytes()` for small values or `import_file()` / `set_hash()` for blob-backed values
- query with `Query`
- subscribe to `LiveEvent`
- set `DownloadPolicy` if you do selective content materialization

---

# 7. My practical takeaways after reading the source

## Best mental framing

- `gossip` is **ephemeral fan-out**
- `blobs` is **verified content transport + retention**
- `docs` is **replicated signed metadata over blob hashes**

## Most important non-obvious behavior

### Gossip

- lag is terminal enough that you should usually rebuild the subscription
- authorship is your app’s job, not gossip’s

### Blobs

- retention is tag-driven
- `TempTag` lifetime really matters
- `HashSeq` is how you build higher-level collections

### Docs

- docs is metadata, not bytes
- `NamespaceId != write access`
- syncing metadata and downloading referenced blobs are separate knobs

## Where I would personally be careful

- handing out write-mode `DocTicket`s
- dropping temp tags accidentally in blobs workflows
- assuming `subscribe()` means “already connected” in gossip
- assuming `reader()` or docs queries will auto-fetch missing content

---

# 8. Short “what should I call?” cheat sheet

## Gossip

- create protocol: `Gossip::builder().spawn(endpoint)`
- join topic: `gossip.subscribe_and_join(topic, peers)`
- send: `sender.broadcast(bytes)`
- receive: `while let Some(ev) = receiver.next().await { ... }`

## Blobs

- local store: `MemStore::new()` or `FsStore::load(path).await?`
- add bytes: `store.blobs().add_bytes(...)`
- serve: `BlobsProtocol::new(&store, None)`
- fetch: `store.downloader(&endpoint).download(request, providers)`
- persist retention: `store.tags().create(hash_and_format).await?`

## Docs

- build protocol: `Docs::memory().spawn(endpoint, blobs, gossip).await?`
- author: `docs.author_default().await?`
- new doc: `docs.create().await?`
- import from ticket: `docs.import(ticket).await?`
- write small value: `doc.set_bytes(author, key, value).await?`
- write existing blob: `doc.set_hash(author, key, hash, size).await?`
- query: `doc.get_many(Query::key_prefix(...).build()).await?`
- live sync: `doc.start_sync(peers).await?` and `doc.subscribe().await?`

