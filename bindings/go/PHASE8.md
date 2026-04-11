# Phase 8 — Full API Surface Completion

The FFI exposes significantly more operations than the Phase 1–4 binding. This checklist tracks implementation of the remaining API groups, using idiomatic Go patterns.

## Go Idioms

| Java Pattern | Go Pattern |
|--------------|------------|
| `CompletableFuture<T>` | `chan T` or `*Node` method returning `(T, error)` |
| `Publisher<T>` | `<-chan Event` or `chan T` |
| `IrohBlobs` wrapper class | `*Blobs` attached to `*Node` |
| Sealed interface | Interface with concrete implementations |
| Builder pattern | Functional options (`WithXxx(...)`) |

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

## 8.1 — Connection Extras

Already done: `Connection` has `OpenBi`, `AcceptBi`, `Close`, `Info`, `SendDatagram`, `ReadDatagram`, `MaxDatagramSize`.

### FFI functions to wrap
- [x] `iroh_connection_remote_id` — get remote peer's node ID
- [x] `iroh_connection_closed` — wait for connection close event
- [x] `iroh_connection_send_datagram` — send unreliable datagram
- [x] `iroh_connection_read_datagram` — receive datagram
- [x] `iroh_connection_max_datagram_size` — query max size
- [x] `iroh_connection_datagram_send_buffer_space` — query available buffer space

### Methods to add to Connection
- [x] `RemoteID() (string, error)` — get remote peer's node ID as hex string
- [x] `OnClosed(ctx) error` — wait for connection close
- [x] `SendDatagram([]byte) error` — send unreliable datagram
- [x] `ReadDatagram(ctx) ([]byte, error)` — receive datagram
- [x] `MaxDatagramSize() (uint64, bool, error)` — sync query
- [x] `DatagramBufferSpace() (uint64, error)` — sync query

### Dependencies
- Uses existing `Connection` type

### Verification
- [ ] Test: send/receive datagram round-trip
- [ ] Test: remote ID matches expected peer
- [ ] Test: on closed fires on disconnect

---

## 8.2 — Blobs (`blobs.go`)

FFI functions: `iroh_blobs_add_bytes`, `iroh_blobs_read`, `iroh_blobs_add_bytes_as_collection`, `iroh_blobs_add_collection`, `iroh_blobs_list_collection`, `iroh_blobs_create_ticket`, `iroh_blobs_create_collection_ticket`, `iroh_blobs_download`, `iroh_blobs_status`, `iroh_blobs_has`, `iroh_blobs_observe_snapshot`, `iroh_blobs_observe_complete`, `iroh_blobs_local_info`

### Types
- [x] `BlobID` — hex string wrapper (32-byte hash as 64-char hex)
- [x] `BlobStatus` — uint32 constant: `0=NOT_FOUND`, `1=PARTIAL`, `2=COMPLETE`
- [x] `BlobTicket` — ticket string (format: `blob1...`)
- [x] `BlobEntry` — `Name string, Hash BlobID, Size int64`
- [x] `BlobCollection` — `Entries []BlobEntry`
- [x] `BlobInfo` — `Hash BlobID, Size int64, Status BlobStatus`
- [x] `BlobFormat` — `RAW=0`, `HASH_SEQ=1`
- [x] `Blobs` — attached to `Node` via `node.Blobs()` method

### Methods on Blobs
- [x] `AddBytes(ctx, []byte) (BlobID, error)` — emits `BLOB_ADDED`
- [x] `AddBytesAsCollection(ctx, []byte, string) (BlobID, error)` — with name
- [x] `AddCollection(ctx, string) (BlobID, error)` — JSON entries
- [x] `Read(ctx, string) ([]byte, error)` — emits `BLOB_READ`
- [x] `Download(ctx, BlobTicket) (BlobID, error)` — emits `BLOB_DOWNLOADED`
- [x] `Status(string) (BlobStatus, int64, error)` — sync
- [x] `Has(string) (bool, error)` — sync
- [x] `ObserveComplete(ctx, string) error` — emits `BLOB_OBSERVE_COMPLETE`
- [x] `ObserveSnapshot(string) (bool, int64, error)` — sync
- [x] `ListCollection(ctx, string) (*BlobCollection, error)` — emits `BLOB_READ`
- [x] `CreateTicket(string, BlobFormat) (BlobTicket, error)` — sync
- [x] `CreateCollectionTicket(string, []string) (BlobTicket, error)` — sync
- [x] `LocalInfo(string) (*BlobInfo, error)` — sync

### Factory
- [x] `(*Node) Blobs() *Blobs`

### Dependencies
- Uses `Node` handle
- Uses existing FFI event infrastructure

### Verification
- [ ] Test: add bytes → read back same bytes
- [ ] Test: create collection ticket → download collection
- [ ] Test: observe_complete fires on download completion

---

## 8.3 — Tags (`tags.go`)

FFI functions: `iroh_tags_set`, `iroh_tags_get`, `iroh_tags_delete`, `iroh_tags_list_prefix`

### Types
- [ ] `TagFormat` — string constant: `"RAW"`, `"HASH_SEQ"`
- [ ] `TagEntry` — `Name string, Hash BlobID, Format TagFormat`
- [ ] `Tags` — attached to `Node`

### Methods on Tags
- [ ] `Set(ctx, string, BlobID, TagFormat) error` — emits `TAG_SET`
- [ ] `Get(ctx, string) (TagEntry, error)` — emits `TAG_GET`
- [ ] `Delete(ctx, string) error` — emits `TAG_DELETED`
- [ ] `ListPrefix(ctx, string) ([]TagEntry, error)` — emits `TAG_LIST`
- [ ] `ListAll(ctx) ([]TagEntry, error)`

### Factory
- [ ] `(*Node) Tags() *Tags`

### Dependencies
- Uses `BlobID` from Blobs

### Verification
- [ ] Test: set tag → get tag → values match
- [ ] Test: delete tag → get returns NOT_FOUND
- [ ] Test: list_prefix filters correctly

---

## 8.4 — Docs (`docs.go`)

FFI functions: `iroh_docs_create`, `iroh_docs_create_author`, `iroh_docs_join`, `iroh_doc_set_bytes`, `iroh_doc_get_exact`, `iroh_doc_share`, `iroh_doc_query`, `iroh_doc_read_entry_content`, `iroh_doc_start_sync`, `iroh_doc_leave`, `iroh_doc_subscribe`, `iroh_doc_event_recv`, `iroh_doc_set_download_policy`, `iroh_doc_share_with_addr`, `iroh_docs_join_and_subscribe`

### Types
- [ ] `AuthorID` — 32-byte author key as hex string
- [ ] `DocID` — document identifier as hex string
- [ ] `DocEntry` — `Key string, Author AuthorID, ContentHash BlobID, Value []byte`
- [ ] `DocQuery` — struct with `Mode QueryMode, KeyPrefix string`
- [ ] `QueryMode` — string constant: `"AUTHOR"`, `"ALL"`, `"PREFIX"`
- [ ] `DocEvent` — interface with `Set`, `Del`, `Insert` implementations
- [ ] `Doc` — document handle from `Docs.Open` or `subscribe`
- [ ] `Docs` — attached to `Node`

### DocEvent interface
```go
type DocEvent interface {
    IsSet() bool
    IsDel() bool
    IsInsert() bool
}
```

### Methods on Docs
- [ ] `Create(ctx) (Doc, error)` — emits `DOC_SHARED`
- [ ] `CreateAuthor(ctx) (AuthorID, error)`
- [ ] `Join(ctx, DocID, BlobTicket) error` — emits `DOC_JOINED_AND_SUBSCRIBED`
- [ ] `JoinAndSubscribe(ctx, BlobTicket) (Doc, error)` — returns Doc

### Methods on Doc
- [ ] `SetBytes(ctx, string, AuthorID, []byte) error` — emits `DOC_EVENT`
- [ ] `GetExact(ctx, string, AuthorID) (DocEntry, error)`
- [ ] `Query(ctx, DocQuery) ([]DocEntry, error)`
- [ ] `ReadEntryContent(ctx, DocEntry) ([]byte, error)`
- [ ] `StartSync(ctx) error`
- [ ] `Leave(ctx) error`
- [ ] `Subscribe(ctx) (<-chan DocEvent, error)` — returns event channel
- [ ] `SetDownloadPolicy(ctx, DownloadPolicy) error`
- [ ] `Share(ctx, ShareMode) (BlobTicket, error)` — emits `DOC_SHARED`
- [ ] `ShareWithAddr(ctx, ShareMode, []string) (BlobTicket, error)`

### Factories
- [ ] `(*Node) Docs() *Docs`
- [ ] `(*Docs) Open(ctx, DocID) (Doc, error)`

### Dependencies
- Complex: uses Authors, Blobs, Tickets

### Verification
- [ ] Test: create doc → set bytes → query returns entry
- [ ] Test: subscribe → remote set → event received
- [ ] Test: share ticket → join → content matches

---

## 8.5 — Gossip (`gossip.go`)

FFI functions: `iroh_gossip_subscribe`, `iroh_gossip_broadcast`, `iroh_gossip_recv`

### Types
- [ ] `GossipMessage` — `Topic string, Author AuthorID, Content []byte`
- [ ] `GossipPeer` — `ID NodeID, Addr string`
- [ ] `Gossip` — attached to `Node`

### Methods on Gossip
- [ ] `Subscribe(ctx, string) (*GossipSubscription, error)` — emits `DOC_SUBSCRIBED`
- [ ] `Broadcast(ctx, string, []byte) error`
- [ ] `Recv(ctx) (GossipMessage, error)` — emits `DOC_EVENT`

### GossipSubscription
- [ ] `Messages() <-chan GossipMessage`
- [ ] `Close() error`

### Factory
- [ ] `(*Node) Gossip() *Gossip`

### Dependencies
- Uses `AuthorID` from Docs

### Verification
- [ ] Test: subscribe topic → broadcast → message received
- [ ] Test: multiple peers receive same broadcast

---

## 8.6 — Endpoint Extras (`endpoint.go`)

### Types to add
- [ ] `RemoteInfo` — `ID NodeID, Type ConnType, RelayURL string, RTT time.Duration`
- [ ] `ConnType` — string constant: `"DIRECT"`, `"RELAY"`, `"UNKNOWN"`
- [ ] `TransportMetrics` — detailed transport statistics

### Methods to add to Endpoint
- [ ] `RemoteInfo(ctx, NodeID) (RemoteInfo, error)`
- [ ] `RemoteInfoList(ctx) ([]RemoteInfo, error)`
- [ ] `TransportMetrics(ctx) (TransportMetrics, error)`

### Hook support (lower priority)
- [ ] `iroh_hook_before_connect_respond`
- [ ] `iroh_hook_after_connect_respond`

### Dependencies
- Uses existing `Endpoint` type

### Verification
- [ ] Test: remote info returns correct peer info
- [ ] Test: transport metrics shows connection stats

---

## 8.7 — Signing and Tickets (`crypto.go`)

FFI functions: `aster_contract_id`, `aster_canonical_bytes`, `aster_signing_bytes`, `aster_canonical_json`, `aster_ticket_encode`, `aster_ticket_decode`, `aster_frame_encode`, `aster_frame_decode`

### Types
- [ ] `CanonicalJSON` — utility type with static methods
- [ ] `ContractID` — hex string
- [ ] `Ticket` — encoded ticket string
- [ ] `Frame` — wire frame representation

### Functions
- [ ] `CanonicalJSON.Normalize(string) (string, error)`
- [ ] `ComputeContractID(string) (ContractID, error)`
- [ ] `EncodeTicket(BlobTicket) (string, error)`
- [ ] `DecodeTicket(string) (BlobTicket, error)`
- [ ] `EncodeFrame(interface{}) ([]byte, error)`
- [ ] `DecodeFrame([]byte) (interface{}, error)`

### Dependencies
- No dependencies on other Phase 8 groups

### Verification
- [ ] Test: canonical JSON produces deterministic output
- [ ] Test: ticket round-trip encode/decode
- [ ] Test: frame encode/decode preserves content

---

## Verification Checklist

### Connection Extras (8.1)
- [ ] `connection_test.go` — datagram send/receive, remote ID, close channel

### Blobs (8.2)
- [ ] `blobs_test.go` — add/read bytes, collection tickets, observe_complete

### Tags (8.3)
- [ ] `tags_test.go` — set/get/delete/list

### Docs (8.4)
- [ ] `docs_test.go` — create/set/query/subscribe

### Gossip (8.5)
- [ ] `gossip_test.go` — subscribe/broadcast

### Endpoint Extras (8.6)
- [ ] `endpoint_test.go` — remote info, transport metrics

### Signing (8.7)
- [ ] `crypto_test.go` — canonical JSON, ticket encoding

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| `*Blobs` attached to `*Node` | Go idiom: encapsulate handle + behavior on same type |
| `chan T` for subscriptions | Go idiom: select on channel, natural integration with `context` |
| Error as last return value | Go idiom: `func() (T, error)` |
| Interface for DocEvent | Type-switch friendly, extensible |
| Options pattern for config | Go idiom: `WithXxx(...)` functional options |
