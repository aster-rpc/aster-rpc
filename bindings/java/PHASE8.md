# Phase 8 — Full API Surface Completion

The FFI exposes significantly more operations than the Phase 1–4 binding. This checklist tracks implementation of the remaining API groups.

## Implementation Order

| Order | Group | Rationale |
|-------|-------|-----------|
| 1 | Connection extras | Smallest surface, uses existing types |
| 2 | Blobs | Highest value — file transfer is core use case |
| 3 | Tags | Simple key→blob mapping, natural second step |
| 4 | Docs | Most complex — content-addressed store with sync |
| 5 | Gossip | Independent, well-scoped |
| 6 | Endpoint extras | Metrics + hooks — lower priority |
| 7 | Signing/tickets | Utility layer — depends on nothing |

---

## 8.1 — Connection Extras (`com.aster.handle`)

Already done: `IrohConnection` has `openBi`, `acceptBi`, `close`, `info`.

### FFIs to wrap
- [x] `iroh_connection_remote_id` — get remote peer's node ID
- [x] `iroh_connection_closed` — wait for connection close event
- [x] `iroh_connection_send_datagram` — send unreliable datagram
- [x] `iroh_connection_read_datagram` — receive datagram
- [x] `iroh_connection_max_datagram_size` — query max size
- [x] `iroh_connection_datagram_send_buffer_space` — query available buffer space

### Types to add
- [x] `IrohConnection.remoteId()` → `String` (hex)
- [x] `IrohConnection.onClosedAsync()` → `CompletableFuture<Void>`
- [x] `IrohConnection.sendDatagramAsync(byte[])` → `CompletableFuture<Void>`
- [x] `IrohConnection.readDatagramAsync()` → `CompletableFuture<Datagram>`
- [x] `IrohConnection.maxDatagramSize()` → `OptionalInt` (sync)
- [x] `IrohConnection.datagramBufferSpace()` → `int` (sync)
- [x] `Datagram` record — `(byte[] data)`

### Dependencies
- Uses existing `IrohConnection` handle
- Uses existing `NodeId` type

### Verification
- [ ] Unit test: send/receive datagram round-trip
- [ ] Unit test: remoteId matches expected peer
- [ ] Unit test: onClosed completes when peer disconnects

---

## 8.2 — Blobs (`com.aster.blobs`)

Wraps: `iroh_blobs_add_bytes`, `iroh_blobs_read`, `iroh_blobs_add_bytes_as_collection`, `iroh_blobs_add_collection`, `iroh_blobs_list_collection`, `iroh_blobs_create_ticket`, `iroh_blobs_create_collection_ticket`, `iroh_blobs_download`, `iroh_blobs_status`, `iroh_blobs_has`, `iroh_blobs_observe_snapshot`, `iroh_blobs_observe_complete`, `iroh_blobs_local_info`

### Types to add
- [x] `BlobId` — hex string wrapper around 32-byte hash
- [x] `BlobStatus` enum — `NOT_FOUND`, `PARTIAL`, `COMPLETE`
- [x] `BlobTicket` — ticket string encoding (format: `blob1...`)
- [x] `BlobCollection` — list of `BlobEntry(name, hash, size)` from list_collection
- [x] `BlobEntry` — `(String name, BlobId hash, long size)`
- [x] `BlobInfo` — `(BlobId hash, long size, BlobStatus status)`
- [x] `BlobFormat` — `RAW`, `HASH_SEQ`
- [x] `IrohBlobs` class — wraps node handle, exposes blob operations

### Methods on IrohBlobs
- [x] `addBytesAsync(byte[] data)` → `CompletableFuture<BlobId>` — emits `BLOB_ADDED`
- [x] `addBytesAsCollectionAsync(byte[] data, String name)` → `CompletableFuture<BlobId>`
- [x] `addCollectionAsync(String entriesJson)` → `CompletableFuture<BlobId>`
- [x] `readAsync(String hashHex)` → `CompletableFuture<byte[]>` — emits `BLOB_READ`
- [x] `downloadAsync(BlobTicket ticket)` → `CompletableFuture<BlobId>` — emits `BLOB_DOWNLOADED`
- [x] `status(BlobId id)` → `BlobStatus` (sync)
- [x] `has(BlobId id)` → `boolean` (sync)
- [x] `observeCompleteAsync(BlobId id)` → `CompletableFuture<Void>` — emits `BLOB_OBSERVE_COMPLETE`
- [x] `observeSnapshot(BlobId id)` → `Map<BlobId, BlobStatus>` (sync)
- [x] `listCollectionAsync(String hashHex)` → `CompletableFuture<BlobCollection>`
- [x] `createTicket(BlobId id, BlobFormat format)` → `BlobTicket` (sync)
- [x] `createCollectionTicket(BlobId id, Set<String> names)` → `BlobTicket` (sync)
- [x] `localInfo(BlobId id)` → `BlobInfo` (sync)

### Factory
- [x] `IrohNode.blobs()` → `IrohBlobs`

### Dependencies
- Uses `IrohNode` handle
- Uses existing FFI event infrastructure

### Verification
- [ ] Unit test: add bytes → read back same bytes
- [ ] Unit test: create collection ticket → download collection
- [ ] Unit test: observe_complete fires on download completion

---

## 8.3 — Tags (`com.aster.tags`)

Wraps: `iroh_tags_set`, `iroh_tags_get`, `iroh_tags_delete`, `iroh_tags_list_prefix`

### Types to add
- [ ] `TagFormat` enum — `RAW`, `HASH_SEQ`
- [ ] `TagEntry` record — `(String name, BlobId hash, TagFormat format)`
- [ ] `IrohTags` class — wraps node handle

### Methods on IrohTags
- [ ] `setAsync(String name, BlobId hash, TagFormat format)` → `CompletableFuture<Void>` — emits `TAG_SET`
- [ ] `getAsync(String name)` → `CompletableFuture<TagEntry>` — emits `TAG_GET`
- [ ] `deleteAsync(String name)` → `CompletableFuture<Void>` — emits `TAG_DELETED`
- [ ] `listPrefixAsync(String prefix)` → `CompletableFuture<List<TagEntry>>` — emits `TAG_LIST`
- [ ] `listAllAsync()` → `CompletableFuture<List<TagEntry>>`

### Factory
- [ ] `IrohNode.tags()` → `IrohTags`

### Dependencies
- Uses `IrohBlobs` types (BlobId)

### Verification
- [ ] Unit test: set tag → get tag → values match
- [ ] Unit test: delete tag → get returns NOT_FOUND
- [ ] Unit test: list_prefix filters correctly

---

## 8.4 — Docs (`com.aster.docs`)

Wraps: `iroh_docs_create`, `iroh_docs_create_author`, `iroh_docs_join`, `iroh_doc_set_bytes`, `iroh_doc_get_exact`, `iroh_doc_share`, `iroh_doc_query`, `iroh_doc_read_entry_content`, `iroh_doc_start_sync`, `iroh_doc_leave`, `iroh_doc_subscribe`, `iroh_doc_event_recv`, `iroh_doc_set_download_policy`, `iroh_doc_share_with_addr`, `iroh_docs_join_and_subscribe`

### Types to add
- [ ] `AuthorId` — 32-byte author key (wrapped as hex string)
- [ ] `DocId` — document identifier (hex string)
- [ ] `DocEntry` record — `(String key, AuthorId author, BlobId contentHash, byte[] value)`
- [ ] `DocQuery` — query mode enum (`AUTHOR`, `ALL`, `PREFIX`) + key filter
- [ ] `DocEvent` sealed interface — `DocEvent.Set`, `DocEvent.Del`, `DocEvent.Insert` variants
- [ ] `DocSubscription` — `Publisher<DocEvent>` from `subscribe()`
- [ ] `IrohDocs` class — document store operations

### Methods on IrohDocs
- [ ] `createAsync()` → `CompletableFuture<DocId>` — emits `DOC_SHARED`
- [ ] `createAuthorAsync()` → `CompletableFuture<AuthorId>`
- [ ] `joinAsync(DocId id, BlobTicket ticket)` → `CompletableFuture<Void>` — emits `DOC_JOINED_AND_SUBSCRIBED`
- [ ] `joinAndSubscribeAsync(BlobTicket ticket)` → `CompletableFuture<DocId>`

### Methods on Doc (returned from docs.open or subscribe)
- [ ] `doc.setBytesAsync(String key, AuthorId author, byte[] value)` → `CompletableFuture<Void>` — emits `DOC_EVENT`
- [ ] `doc.getExactAsync(String key, AuthorId author)` → `CompletableFuture<DocEntry>`
- [ ] `doc.queryAsync(DocQuery query)` → `CompletableFuture<List<DocEntry>>`
- [ ] `doc.readEntryContentAsync(DocEntry entry)` → `CompletableFuture<byte[]>`
- [ ] `doc.startSyncAsync()` → `CompletableFuture<Void>`
- [ ] `doc.leaveAsync()` → `CompletableFuture<Void>`
- [ ] `doc.subscribeAsync()` → `CompletableFuture<DocSubscription>`
- [ ] `doc.setDownloadPolicyAsync(DownloadPolicy policy)` → `CompletableFuture<Void>`
- [ ] `doc.shareAsync(ShareMode mode)` → `CompletableFuture<BlobTicket>` — emits `DOC_SHARED`
- [ ] `doc.shareWithAddrAsync(ShareMode mode, Set<String> addrs)` → `CompletableFuture<BlobTicket>`

### Factories
- [ ] `IrohNode.docs()` → `IrohDocs`
- [ ] `IrohDocs.openAsync(DocId id)` → `CompletableFuture<Doc>`

### Dependencies
- Complex: uses Authors, Blobs, Tickets

### Verification
- [ ] Unit test: create doc → set bytes → query returns entry
- [ ] Unit test: subscribe → remote set → event received
- [ ] Unit test: share ticket → join → content matches

---

## 8.5 — Gossip (`com.aster.gossip`)

Wraps: `iroh_gossip_subscribe`, `iroh_gossip_broadcast`, `iroh_gossip_recv`

### Types to add
- [ ] `GossipMessage` record — `(String topic, AuthorId author, byte[] content)`
- [ ] `GossipPeer` record — `(NodeId id, InetSocketAddress addr)`
- [ ] `IrohGossip` class — pub/sub on topics

### Methods on IrohGossip
- [ ] `subscribeAsync(String topic)` → `CompletableFuture<GossipSubscription>` — emits `DOC_SUBSCRIBED`
- [ ] `broadcastAsync(String topic, byte[] content)` → `CompletableFuture<Void>`
- [ ] `recvAsync()` → `CompletableFuture<GossipMessage>` — emits `DOC_EVENT`

### GossipSubscription
- [ ] `messages()` → `Publisher<GossipMessage>`
- [ ] `closeAsync()` → `CompletableFuture<Void>`

### Factory
- [ ] `IrohNode.gossip()` → `IrohGossip`

### Dependencies
- Uses AuthorId from Docs

### Verification
- [ ] Unit test: subscribe topic → broadcast → message received
- [ ] Unit test: multiple peers receive same broadcast

---

## 8.6 — Endpoint Extras (`com.aster.handle`)

### Types to add
- [ ] `RemoteInfo` record — `(NodeId id, ConnectionType type, String relayUrl, long rttNs)`
- [ ] `ConnectionType` enum — `DIRECT`, `RELAY`, `UNKNOWN`
- [ ] `TransportMetrics` record — detailed transport statistics

### Methods on IrohEndpoint
- [ ] `addrInfo()` → `NodeAddr` — already done in Phase 5b
- [ ] `remoteInfoAsync(NodeId id)` → `CompletableFuture<RemoteInfo>`
- [ ] `remoteInfoListAsync()` → `CompletableFuture<List<RemoteInfo>>`
- [ ] `transportMetricsAsync()` → `CompletableFuture<TransportMetrics>`

### Hook support (lower priority)
- [ ] `iroh_hook_before_connect_respond`
- [ ] `iroh_hook_after_connect_respond`

### Dependencies
- Uses existing Endpoint handle

### Verification
- [ ] Unit test: remoteInfo returns correct peer info
- [ ] Unit test: transportMetrics shows connection stats

---

## 8.7 — Signing and Tickets (`com.aster.crypto`)

Wraps: `aster_contract_id`, `aster_canonical_bytes`, `aster_signing_bytes`, `aster_canonical_json`, `aster_ticket_encode`, `aster_ticket_decode`, `aster_frame_encode`, `aster_frame_decode`

### Types to add
- [ ] `CanonicalJson` — canonical JSON normalization utility
- [ ] `AsterContract` — compute contract ID from JSON
- [ ] `AsterTicket` — encode/decode aster1... tickets
- [ ] `AsterFrame` — encode/decode wire frames

### Methods
- [ ] `CanonicalJson.normalize(String json)` → `String`
- [ ] `AsterContract.computeId(String json)` → `ContractId`
- [ ] `AsterTicket.encode(BlobTicket ticket)` → `String`
- [ ] `AsterTicket.decode(String encoded)` → `BlobTicket`
- [ ] `AsterFrame.encode(FrameContent content)` → `byte[]`
- [ ] `AsterFrame.decode(byte[] data)` → `FrameContent`

### Dependencies
- No dependencies on other Phase 8 groups

### Verification
- [ ] Unit test: canonical JSON produces deterministic output
- [ ] Unit test: ticket round-trip encode/decode
- [ ] Unit test: frame encode/decode preserves content

---

## Verification Checklist

### Connection Extras (8.1)
- [ ] `IrohConnectionTest` — datagram send/receive, remote ID, onClosed

### Blobs (8.2)
- [ ] `IrohBlobsTest` — add/read bytes, collection tickets, observe_complete

### Tags (8.3)
- [ ] `IrohTagsTest` — set/get/delete/list

### Docs (8.4)
- [ ] `IrohDocsTest` — create/set/query/subscribe

### Gossip (8.5)
- [ ] `IrohGossipTest` — subscribe/broadcast

### Endpoint Extras (8.6)
- [ ] `IrohEndpointTest` — remote info, transport metrics

### Signing (8.7)
- [ ] `AsterCryptoTest` — canonical JSON, ticket encoding

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| `com.aster.blobs` package | Separate from `handle` — blobs are a distinct subsystem |
| `CompletableFuture<T>` return | Consistent with Phase 1–4 pattern |
| `Publisher<T>` for subscriptions | Java Flow API for streaming events |
| Sealed interface for DocEvent | Type-safe event variants |
| Sync methods for queries | `maxDatagramSize`, `datagramBufferSpace` — no async needed |
