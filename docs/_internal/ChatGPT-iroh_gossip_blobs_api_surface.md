# iroh-gossip and iroh-blobs API surface analysis

Analyzed from the uploaded source archives, not from generated rustdoc:

- `iroh-gossip` `0.97.0` — archive root commit `9a30bbb4a9dee83dce43d1a4412acf3d3b6b1acd`
- `iroh-blobs` `0.99.0` — archive root commit `8e0fa7afddd35f6344cf3e23d2045cf614ef568b`

I focused on the **user-facing API surface** and the behavior that is only obvious once you read the source and examples.

## Scope note

You mentioned “gossip and blobs (docs/kv)”, but the uploaded archives are for **`iroh-gossip`** and **`iroh-blobs`** only. `iroh-docs` was not included, so this write-up does **not** cover the docs/KV crate.

---

# 1. Big-picture mental model

## `iroh-gossip`

`iroh-gossip` is a **topic-based epidemic broadcast layer** built on top of `iroh` connections.

You give peers:

- a shared `TopicId`
- a way to reach bootstrap peers
- a gossip protocol handler mounted on the same ALPN

Then each peer subscribes to the topic and starts receiving:

- neighbor up/down events
- received messages
- lag notifications

Important: **topic membership is rendezvous, not authorization**. Possessing a topic ID is effectively enough to join/speak unless your application adds its own policy/signature layer.

## `iroh-blobs`

`iroh-blobs` is a **content-addressed blob store + transfer protocol**.

It gives you three distinct layers:

1. **A local store API** for adding, reading, tagging, exporting, and observing blobs.
2. **A protocol handler** (`BlobsProtocol`) for serving that store over `iroh` connections.
3. **Client/download APIs** for fetching from one or many peers.

It is BLAKE3/BAO-based, so transfers are **stream-verified** rather than “trust the peer and hope”.

---

# 2. `iroh-gossip` API surface

## Features and module shape

Public modules of interest:

- `api` — the user-facing API you will actually call
- `net` — the `iroh` integration and protocol handler
- `proto` — internal state machine/protocol logic; useful for understanding behavior, but usually not what you call directly

Feature flags that matter:

- `net` — enabled by default; needed for actual use over `iroh`
- `rpc` — optional remote-control surface over `noq`/`irpc`

In normal usage you want the **`net`** path.

## Main public types

### Core types

- `TopicId`
- `Gossip`
- `GossipApi`
- `GossipTopic`
- `GossipSender`
- `GossipReceiver`
- `Event`
- `Message`
- `JoinOptions`

## Minimal usage flow

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
let bootstrap = vec![]; // endpoint ids of peers you want to try first

let mut sub = gossip.subscribe(topic, bootstrap).await?;
sub.joined().await?;
sub.broadcast("hello".into()).await?;

while let Some(event) = sub.next().await {
    match event? {
        Event::Received(msg) => {
            println!("from {:?}: {:?}", msg.delivered_from, msg.content);
        }
        Event::NeighborUp(id) => println!("neighbor up: {id}"),
        Event::NeighborDown(id) => println!("neighbor down: {id}"),
        Event::Lagged => println!("receiver fell behind"),
    }
}

router.shutdown().await?;
```

## `Gossip` and `Builder`

### `Gossip::builder() -> Builder`

Creates a builder for the local gossip actor.

### `Builder::spawn(endpoint: Endpoint) -> Gossip`

Starts the gossip actor and returns a handle.

This is the main entrypoint when embedding gossip into an app.

### Builder configuration methods

```rust
Builder::max_message_size(usize)
Builder::membership_config(HyparviewConfig)
Builder::broadcast_config(PlumtreeConfig)
Builder::alpn(impl AsRef<[u8]>)
```

Notes:

- Default max message size is **4096 bytes**.
- If you set a custom ALPN, every peer must use the same ALPN and you must register that same ALPN on the router.
- Under the hood the state machine enforces a minimum viable message size; setting an unrealistically tiny size will panic during config validation.

### `Gossip` instance methods

```rust
gossip.max_message_size() -> usize
gossip.shutdown().await -> Result<(), Error>
gossip.metrics() -> &Arc<Metrics>
```

RPC-only/local wiring helpers also exist, but for in-process use the main thing is: **`Gossip` derefs to `GossipApi`**.

That means you can call `gossip.subscribe(...)` directly.

## `GossipApi`

### `subscribe(topic_id, bootstrap) -> GossipTopic`

```rust
pub async fn subscribe(
    &self,
    topic_id: TopicId,
    bootstrap: Vec<EndpointId>,
) -> Result<GossipTopic, ApiError>
```

Behavior:

- joins the topic immediately from the API caller’s perspective
- **does not wait** for any actual connection to be established
- messages can be queued before the first peer connection exists

### `subscribe_and_join(topic_id, bootstrap) -> GossipTopic`

Same as `subscribe`, but waits until there is at least one active connection.

This is the easiest “I want this to be actually connected before I continue” call.

### `subscribe_with_opts(topic_id, JoinOptions) -> GossipTopic`

Allows control over:

- bootstrap set
- subscription event buffer capacity

Use this when you care about backpressure/lag behavior.

### RPC surface

With the `rpc` feature there is also:

```rust
GossipApi::connect(endpoint, addr) -> GossipApi
```

This is only for controlling a gossip instance over RPC, not for the topic data plane itself.

## `JoinOptions`

```rust
pub struct JoinOptions {
    pub bootstrap: BTreeSet<EndpointId>,
    pub subscription_capacity: usize,
}
```

Helper:

```rust
JoinOptions::with_bootstrap(iter)
```

Important behavior from source:

- default event buffer capacity is **2048** per subscription
- if the subscriber falls behind and overflows this buffer:
  - the receiver gets a **`Lagged`** event
  - the lagged message is dropped
  - the subscription is closed

That is a strong signal that this receiver should be recreated.

## `GossipTopic`

A combined send + receive handle.

### Methods

```rust
topic.split() -> (GossipSender, GossipReceiver)
topic.broadcast(Bytes).await
topic.broadcast_neighbors(Bytes).await
topic.neighbors() -> impl Iterator<Item = EndpointId>
topic.joined().await
topic.is_joined() -> bool
```

And it is also a `Stream<Item = Result<Event, ApiError>>`.

Usage styles:

### Combined style

```rust
let mut topic = gossip.subscribe(...).await?;
topic.broadcast(bytes).await?;
while let Some(ev) = topic.next().await { ... }
```

### Split style

```rust
let (sender, mut receiver) = gossip.subscribe(...).await?.split();
sender.broadcast(bytes).await?;
receiver.joined().await?;
while let Some(ev) = receiver.next().await { ... }
```

### Drop semantics

Dropping the topic handle leaves the topic.

More precisely:

- if you keep it unsplit, dropping `GossipTopic` leaves
- if split, the topic is only left once **both** `GossipSender` and `GossipReceiver` are dropped

This is important for lifecycle management.

## `GossipSender`

### Methods

```rust
sender.broadcast(Bytes).await
sender.broadcast_neighbors(Bytes).await
sender.join_peers(Vec<EndpointId>).await
```

Notes:

- `broadcast` sends to the full gossip swarm.
- `broadcast_neighbors` only sends to direct neighbors.
- `join_peers` is effectively “attempt to pull these peers into my current topic mesh now”.

That last one is useful if your discovery layer finds more peers after initial subscribe.

## `GossipReceiver`

### Methods

```rust
receiver.neighbors() -> impl Iterator<Item = EndpointId>
receiver.joined().await
receiver.is_joined() -> bool
```

And it is a stream of topic `Event`s.

### `joined()` semantics

This waits until the receiver has seen at least one `NeighborUp` event.

That means “I have at least one direct neighbor for this topic”, not “I am globally well-connected”.

## `Event` and `Message`

```rust
pub enum Event {
    NeighborUp(EndpointId),
    NeighborDown(EndpointId),
    Received(Message),
    Lagged,
}
```

```rust
pub struct Message {
    pub content: Bytes,
    pub scope: DeliveryScope,
    pub delivered_from: EndpointId,
}
```

Notes:

- `delivered_from` is **not** the original author, just the peer who delivered this copy.
- if you need author identity, put it in your payload and sign it yourself.
- `scope` tells you whether the message came via direct-neighbor delivery or via wider gossip dissemination.

## Practical bootstrap requirements

This is the biggest hidden gotcha.

`subscribe(topic_id, bootstrap_peer_ids)` takes **endpoint IDs**, but to actually dial those peers your endpoint also needs an address discovery mechanism that knows how to reach them.

In the examples this is done by:

- giving peers a ticket/out-of-band descriptor that contains endpoint addresses
- adding those addresses to an address lookup provider
- then passing the peer IDs as bootstrap IDs

So, in practice, to connect to a gossip mesh you usually need both:

- **topic id**
- **peer addressing info**

## Important behavior and considerations

### 1. Topic ID is rendezvous, not auth

If you build Aster on top of this, do not treat topic secrecy as sufficient authorization.

### 2. Slow consumers are cut off

A lagging receiver will not just miss a message; it will receive `Lagged` and the subscription closes. Design your code to recreate subscriptions.

### 3. Messages are size-limited

Gossip is for control-plane or relatively small event messages. Large payloads belong in blobs, not gossip.

### 4. Bootstrap is endpoint-id based, but routing still needs addresses

A topic ID and peer IDs are not enough if your address lookup cannot resolve the peers.

### 5. Clean shutdown matters

`shutdown()` sends disconnects and tears down state. If you care about mesh hygiene, do a clean shutdown.

### 6. Best use in a larger system

A very natural pattern is:

- **docs/KV** = durable truth
- **gossip** = invalidation / change notifications / liveness / fast fanout
- **blobs** = bulk payloads and artifacts

---

# 3. `iroh-blobs` API surface

## Features and module shape

Key public modules/types:

- `api::Store`
- `api::blobs::Blobs`
- `api::tags::Tags`
- `api::remote::Remote`
- `api::downloader::Downloader`
- `BlobsProtocol`
- `ticket::BlobTicket`
- `Hash`, `BlobFormat`, `HashAndFormat`
- `format::collection::Collection`
- store implementations under `store::mem` and `store::fs`

Feature flags that matter:

- `fs-store` — enabled by default; filesystem-backed store
- `rpc` — enabled by default; remote control over `noq`

## Core data types

### `Hash`

BLAKE3 content hash.

### `BlobFormat`

```rust
enum BlobFormat {
    Raw,
    HashSeq,
}
```

Semantics:

- `Raw` = a plain blob
- `HashSeq` = a blob whose content is a packed sequence of child hashes

A `HashSeq` is the primitive used for collections / recursive fetches.

### `HashAndFormat`

Pair of:

- hash
- format (`Raw` or `HashSeq`)

This matters because the same hash used as a raw blob vs as a hash-sequence root implies different traversal behavior.

## `Store`: the main top-level API

```rust
pub struct Store
```

You usually obtain it from a backing store such as `MemStore` or `FsStore`. `Store` derefs to `Blobs`, so many blob methods are directly available on store instances.

### Main methods

```rust
store.tags() -> &Tags
store.blobs() -> &Blobs
store.remote() -> &Remote
store.downloader(&Endpoint) -> Downloader
store.sync_db().await
store.shutdown().await
store.wait_idle().await
```

Notes:

- `downloader(&Endpoint)` creates an object with internal state and connection pooling. The source explicitly says: **do not create it ad hoc if you need it repeatedly**.
- `wait_idle()` is mostly for tests and can wait forever if the store is still being touched concurrently.

## Serving blobs: `BlobsProtocol`

This is the easiest way to expose your store over `iroh`.

### Construction

```rust
let blobs = BlobsProtocol::new(&store, None);
let router = Router::builder(endpoint)
    .accept(iroh_blobs::ALPN, blobs)
    .spawn();
```

### Notes

- `BlobsProtocol` derefs to `Store`.
- When the protocol handler is mounted, peers can fetch from your store over the blobs ALPN.
- This is the serving side. Downloading is done through `Remote` or `Downloader`.

## `BlobTicket`

A convenience bootstrap object that contains:

- provider `EndpointAddr`
- `Hash`
- `BlobFormat`

### Construction

```rust
let ticket = BlobTicket::new(endpoint.addr(), hash, format);
```

### Useful methods

```rust
ticket.hash()
ticket.addr()
ticket.format()
ticket.hash_and_format()
ticket.recursive() -> bool
ticket.into_parts()
```

`recursive()` is `true` for `HashSeq`, which is the signal that the receiver should fetch the whole collection/hash-sequence graph rather than only a single raw blob.

This is one of the cleanest bootstrap objects in the crate.

---

# 4. Blobs local API: `api::blobs::Blobs`

## Add/import data

### Fast helpers

```rust
blobs.add_slice(data)
blobs.add_bytes(bytes)
blobs.add_bytes_with_opts(AddBytesOptions)
blobs.add_path(path)
blobs.add_path_with_opts(AddPathOptions)
blobs.add_stream(stream).await
```

### Important option types

```rust
struct AddBytesOptions {
    data: Bytes,
    format: BlobFormat,
}

struct AddPathOptions {
    path: PathBuf,
    format: BlobFormat,
    mode: ImportMode,
}
```

What these return:

- all add methods return an **`AddProgress`** handle
- awaiting `AddProgress` yields a **persistent named tag result** (`TagInfo`)
- other helpers let you finish differently

### `AddProgress` finishing methods

```rust
add.temp_tag().await -> TempTag
add.with_tag().await -> TagInfo
add.with_named_tag(name).await -> HashAndFormat
add.stream().await -> Stream<Item = AddProgressItem>
```

This distinction is important:

- `temp_tag()` gives you an **ephemeral in-memory pin** against GC
- `with_tag()` creates a **persistent store tag**
- `with_named_tag(name)` binds a specific tag name you choose

### `TempTag` semantics

A `TempTag` protects content from garbage collection while it is alive.

Important details from source:

- it is **process-local / in-memory**
- dropping it removes that protection
- `TempTag::leak()` intentionally keeps the object alive until process exit
- for `HashSeq`, protection means “protect the root and its children”

### Practical recommendation

When you ingest content for real use, do not stop at a temp tag. Convert it into a persistent tag unless you intentionally want GC-managed ephemeral content.

## Batch add API

```rust
let batch = blobs.batch().await?;
batch.add_bytes(...)
batch.add_slice(...)
batch.add_path_with_opts(...)
batch.temp_tag(...).await
```

Returns a `Batch<'_>` scoped handle.

What it does:

- groups multiple add operations under a temporary scope
- useful for building higher-level objects like collections
- source/examples show this being used to stage multiple files before creating a collection root

A `BatchAddProgress` supports the same finishing helpers as `AddProgress`.

## Read/access local data

### Reader

```rust
blobs.reader(hash) -> BlobReader
blobs.reader_with_opts(ReaderOptions) -> BlobReader
```

`BlobReader` implements async read/seek over the local blob.

Important note from source docs:

- reading a missing part of a partially present blob will error

### “Give me the whole thing” helper

```rust
blobs.get_bytes(hash).await -> Bytes
```

Very convenient, but the source explicitly warns this can exhaust memory on large blobs.

Use it for small blobs, metadata blobs, hash sequences, collection manifests, not giant file payloads.

## Export local data

### Export to file path

```rust
blobs.export(hash, target_path)
blobs.export_with_opts(ExportOptions)
```

Returns `ExportProgress`:

```rust
progress.stream().await
progress.finish().await -> u64 // final size
```

### Export BAO-encoded stream/ranges

```rust
blobs.export_bao(hash, ranges)
blobs.export_bao_with_opts(opts, local_update_cap)
```

Returns `ExportBaoProgress`.

Useful helpers on `ExportBaoProgress` include:

```rust
stream()
hashes_with_index()
hashes()
bao_to_vec().await
data_to_bytes().await
data_to_vec().await
```

This is the low-level, verification-friendly export surface.

### Export plain ranges

```rust
blobs.export_ranges(hash, ranges)
blobs.export_ranges_with_opts(opts)
```

Returns `ExportRangesProgress`.

Important source note:

- requested byte ranges are **rounded up to chunk boundaries** internally
- caller is responsible for clipping to the originally requested byte range if exact slicing matters

### Export a single chunk

```rust
blobs.export_chunk(hash, offset).await -> Leaf
```

Useful for lazy parsing of hash sequences/manifests.

## Observe local completeness / partial state

### Methods

```rust
blobs.observe(hash)
blobs.observe_with_opts(ObserveOptions)
```

Returns `ObserveProgress`.

Ways to use it:

```rust
observe.await -> Bitfield                 // first/current state
observe.await_completion().await -> Bitfield
observe.stream().await -> Stream<Item = Bitfield>
```

Semantics:

- first item is current bitfield
- later items are updates
- once complete, updates stop

This is one of the most useful APIs for resumable/partial transfer logic.

## Listing and existence

```rust
blobs.list()
blobs.status(hash).await -> BlobStatus
blobs.has(hash).await -> bool
```

`list()` returns `BlobsListProgress`, with helpers:

```rust
list.hashes().await -> Vec<Hash>
list.stream().await
```

`has()` is effectively “is complete?”, not just “is there any partial state for this hash?”.

## Hidden / sharp tools

The following exist but are not intended as ordinary app-level calls:

- `import_bao(...)`
- `import_bao_with_opts(...)`
- `import_bao_reader(...)`
- `import_bao_bytes(...)`

These are for feeding **already verified / properly ordered** BAO content items into the store.

The source is explicit: the store assumes the imported BAO items are already correct.

That makes these good building blocks for protocol internals or advanced pipelines, but not casual “maybe this data is okay” ingestion.

## Blob deletion

Deletion exists internally, but the public comment says it is intentionally **not** a normal public operation because of protection/tagging semantics.

The intended model is:

- content lifetime is controlled through tags and temp tags
- garbage collection removes unprotected data

So think of the store as **tag/pin managed**, not “delete files whenever you want”.

---

# 5. Tags API: `api::tags::Tags`

The tags layer is how you keep content alive and give it stable names.

## Main methods

```rust
tags.list_temp_tags().await
tags.list_with_opts(...)
tags.list().await
tags.list_prefix(prefix).await
tags.list_hash_seq().await
tags.get(name).await

tags.set(name, value).await
tags.set_with_opts(...).await

tags.rename(from, to).await
tags.rename_with_opts(...).await

tags.create(value).await -> Tag
tags.create_with_opts(...).await -> Tag

tags.temp_tag(value).await -> TempTag

tags.delete(name).await -> u64
tags.delete_prefix(prefix).await -> u64
tags.delete_all().await -> u64
```

## Semantics that matter

- `create(value)` creates a new persistent tag name for content.
- `set(name, value)` binds a chosen name to content.
- deleting a tag does **not** necessarily delete data immediately; it just removes that persistent root of liveness.
- if all tags/temp-tags protecting content are gone, GC can reclaim it.

The comments explicitly warn that `delete_all()` means all data becomes eligible for garbage collection.

---

# 6. Single-provider download API: `api::remote::Remote`

This is the “speak the blobs protocol over one connection / one peer” API.

It is more explicit and lower-level than `Downloader`.

## Mental model

`Remote` gives you three levels of control:

1. inspect what you already have locally
2. compute what is missing
3. execute a get/observe/push against one remote connection

## Local inspection

### Methods

```rust
remote.local(content).await -> LocalInfo
remote.local_for_request(get_request).await -> LocalInfo
```

`LocalInfo` gives you:

```rust
local.local_bytes() -> u64
local.children() -> Option<u64>
local.is_complete() -> bool
local.missing() -> GetRequest
```

This is a very useful API and easy to miss from poor rustdoc.

For Aster-like systems this is valuable because it lets you compute an efficient resume request without manually walking local bitfields yourself.

## Fetch with local-awareness

### Method

```rust
remote.fetch(conn_or_stream_pair, content) -> GetProgress
```

Behavior:

- inspects what is already present locally
- computes `missing()` ranges
- only asks the remote for missing data
- stores the result into the local store

This is the best single-peer “just get me this if I don’t already have it” call.

## Raw get execution

### Methods

```rust
remote.execute_get(conn_or_stream_pair, get_request) -> GetProgress
remote.execute_get_with_opts(conn_or_stream_pair, get_request) -> GetProgress
remote.execute_get_many(conn, get_many_request) -> GetProgress
```

Important difference vs `fetch`:

- `execute_get*` does **not** first account for locally present ranges
- it executes exactly the request you passed
- that means it can redownload data you already have

Use `fetch` unless you deliberately want low-level control.

## Remote observe

```rust
remote.observe(conn, ObserveRequest) -> Stream<Item = io::Result<Bitfield>>
```

This lets you watch a remote peer’s completeness bitfield for a blob.

That is useful for adaptive provider selection or partial availability logic.

## Push

```rust
remote.execute_push(conn, PushRequest) -> PushProgress
```

Source note worth surfacing:

- this is **experimental**
- many nodes may reject push requests

So treat push as an advanced/optional feature, not the default interoperability path.

## Progress handles

### `GetProgress`

```rust
progress.stream() -> Stream<Item = GetProgressItem>
progress.complete().await -> GetResult<Stats>
```

Items:

- `Progress(u64)`
- `Done(Stats)`
- `Error(GetError)`

### `PushProgress`

Same shape, but for push.

## Important practical notes

### 1. `fetch` is usually the right call

It is the nicest “resumable, local-aware fetch from this peer” surface.

### 2. `execute_get` is protocol-oriented

It is ideal when you already computed the exact request you want to send.

### 3. Hash-sequence requests recurse through children

For `HashSeq`, remote get logic fetches the root and then children in sequence according to request ranges.

### 4. `Connection` is only one accepted transport adapter

Internally the API also accepts a generic `GetStreamPair`, so the design is intentionally a bit more abstract than “must be a raw `Connection`”.

---

# 7. Multi-provider download API: `api::downloader::Downloader`

This is the higher-level client for fetching from **multiple possible providers**.

## Construction

```rust
let downloader = store.downloader(&endpoint);
```

Equivalent lower-level constructor:

```rust
Downloader::new(&store, &endpoint)
Downloader::new_with_opts(&store, &endpoint, pool_options)
```

The source comment explicitly says to **reuse** this object if you will do multiple downloads, because it holds internal state and a connection pool.

## Simple download

```rust
downloader.download(request, providers)
```

Where:

- `request` can be a `Hash`, `HashAndFormat`, `GetRequest`, `GetManyRequest`, or an iterable of hashes
- `providers` can be any `ContentDiscovery` implementation, or even just a collection of `EndpointId`s

Returns `DownloadProgress`.

Awaiting it gives `Result<()>`.

## Advanced download

```rust
downloader.download_with_opts(DownloadOptions)
```

`DownloadOptions` is just `DownloadRequest`.

### `DownloadRequest`

```rust
DownloadRequest::new(request, providers, strategy)
```

Fields:

- `request: FiniteRequest`
- `providers: Arc<dyn ContentDiscovery>`
- `strategy: SplitStrategy`

### `SplitStrategy`

```rust
enum SplitStrategy {
    None,
    Split,
}
```

Semantics from source:

- `None` = sequentially try providers for the whole request
- `Split` = first fetch enough root/manifest data to split the request into parts, then fetch parts in parallel

This is especially useful for collections / hash-sequences / multi-blob requests.

## Progress items

```rust
enum DownloadProgressItem {
    Error(AnyError),
    TryProvider { id, request },
    ProviderFailed { id, request },
    PartComplete { request },
    Progress(u64),
    DownloadError,
}
```

This is better than a single byte counter because it also exposes provider-level behavior.

## Content discovery

### Trait

```rust
trait ContentDiscovery {
    fn find_providers(&self, hash: HashAndFormat) -> Boxed<EndpointId>;
}
```

Any cloneable iterable of peer IDs already works as a simple implementation.

### Built-in helper: `Shuffled`

```rust
Shuffled::new(Vec<EndpointId>)
```

This randomizes provider order per request.

That is useful when you do not want every client to always hammer the first peer.

## Important downloader behavior from source

### 1. It is sequential across providers for a given part

For each request/part, it tries providers in order until one completes it.

### 2. It is local-aware between retries

If provider A gave you the first 10%, provider B is only asked for the remaining 90%.

### 3. `Split` can parallelize parts

This is how you get more interesting swarm-style behavior.

### 4. Provider discovery is pluggable

You can back `ContentDiscovery` with your own service discovery / registry layer.

That makes it a natural fit for Aster.

---

# 8. Collections and hash sequences

This is one of the least obvious but most important parts of `iroh-blobs`.

## `HashSeq`

A `HashSeq` is just a blob whose payload is a sequence of 32-byte child hashes.

That means:

- `HashSeq` is the generic recursive building block
- `BlobFormat::HashSeq` tells the APIs to treat the root as such

## `Collection`

`format::collection::Collection` builds a friendlier higher-level structure.

A collection stores:

- a metadata blob with filenames (`CollectionMeta`)
- a hash-sequence blob linking metadata + children
- the collection root hash as the final addressable object

This means:

- plain `HashSeq` = structure without names
- `Collection` = structure with names/metadata layered on top

## App design implication

For “directory-like” results in your RPC system, returning a `HashSeq` or `Collection` root plus provider info is a very natural fit.

---

# 9. Sharp edges, hidden semantics, and recommendations

## `iroh-gossip`

### Use gossip for control plane, not payload plane

Default max message size is small and the whole design is event-oriented. Use blobs for anything bulky.

### Always include your own application-level auth

Topic IDs are not sufficient authorization.

### Expect to recreate lagged receivers

`Lagged` is not just informational; the subscription closes.

### Bootstrap needs more than peer IDs

You still need address lookup / addressing info for those endpoint IDs.

## `iroh-blobs`

### Tags are the real liveness mechanism

Do not think in terms of “I stored a file, therefore it stays forever”. Think in terms of:

- temp tags
- persistent tags
- garbage collection of unprotected data

### `get_bytes()` is a convenience, not a universal retrieval strategy

Great for metadata and small blobs, dangerous for giant payloads.

### `observe()` is a sleeper feature

It is extremely useful for resumable and partial availability logic.

### `fetch()` is your best single-provider high-level primitive

It is already local-aware and resume-friendly.

### `Downloader` is the best multi-provider primitive

And it is intentionally designed to be pluggable with your own content discovery layer.

### `import_bao*` assumes verified ordered input

That is an advanced internal/pipeline API, not the safe public “please validate arbitrary bytes” API.

### `BlobTicket` is an excellent bootstrap object

It packages:

- content identity
- format
- provider address

That makes it a very nice return type for RPC methods that want to hand off a large response via blobs.

---

# 10. What I would use in Aster

## For gossip

I would build around exactly these pieces:

```rust
let gossip = Gossip::builder()
    .max_message_size(4096)
    .spawn(endpoint.clone());

let router = Router::builder(endpoint.clone())
    .accept(iroh_gossip::ALPN, gossip.clone())
    .spawn();

let mut topic = gossip.subscribe_and_join(topic_id, bootstrap_ids).await?;
topic.broadcast(bytes).await?;
```

And I would treat topic payloads as:

- service announcements
- invalidations
- heartbeat/liveness
- “new doc version / new blob available” notifications

## For blobs

I would build around exactly these pieces:

### Local store + serving

```rust
let store = iroh_blobs::store::fs::FsStore::load(path).await?;
let blobs = BlobsProtocol::new(&store, None);
let router = Router::builder(endpoint.clone())
    .accept(iroh_blobs::ALPN, blobs)
    .spawn();
```

### Add content and return a ticket/capability-like object

```rust
let tag = store.add_bytes(payload).await?;
endpoint.online().await;
let ticket = BlobTicket::new(endpoint.addr(), tag.hash, tag.format);
```

### Single-provider fetch

```rust
store.remote().fetch(conn, ticket.hash_and_format()).await?;
```

### Multi-provider fetch

```rust
let downloader = store.downloader(&endpoint);
downloader.download(ticket.hash_and_format(), vec![provider_id]).await?;
```

### Recursive / directory-like results

Use `Collection` or `HashSeq` roots rather than inventing a separate “directory transfer protocol”.

---

# 11. Short cheat sheet

## `iroh-gossip`

Use these most of the time:

- `Gossip::builder().spawn(endpoint)`
- `Router::accept(iroh_gossip::ALPN, gossip)`
- `gossip.subscribe(...)`
- `gossip.subscribe_and_join(...)`
- `topic.broadcast(...)`
- `sender.join_peers(...)`
- `receiver.joined()`

## `iroh-blobs`

Use these most of the time:

- `FsStore::load(...)` or `MemStore::new()`
- `BlobsProtocol::new(&store, None)`
- `store.add_bytes(...)` / `store.add_path(...)`
- `store.tags().create(...)` or `with_tag()` to persist liveness
- `BlobTicket::new(endpoint.addr(), hash, format)`
- `store.remote().fetch(...)`
- `store.downloader(&endpoint).download(...)`
- `store.export(...)`
- `store.observe(...)`

---

# 12. Bottom line

## `iroh-gossip`

The public surface is actually quite small:

- spawn a local actor
- subscribe to a topic
- broadcast
- receive events
- optionally join more peers later

The real hidden complexity is in:

- bootstrap/addressing
- lag handling
- the fact that topics are not authorization

## `iroh-blobs`

The public surface looks larger, but it clusters nicely into four layers:

1. **local store ops** (`Blobs`, `Tags`)
2. **serving** (`BlobsProtocol`)
3. **single-peer transfer** (`Remote`)
4. **multi-peer transfer** (`Downloader`)

The most important hidden semantics are:

- content lifetime is tag-driven
- `HashSeq` is the recursive primitive
- `fetch()` is resume-aware
- `Downloader` is meant to plug into your own discovery layer

If you want, I can next turn this into a second markdown focused only on **“how Aster should wrap these APIs”** with proposed Rust traits and bootstrap objects.
